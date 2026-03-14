use crate::{
    functions::*,
    internal::{install::install, packages},
};
use log::{debug, error, info};
use serde_json::{self, Value, Map};
use shared::{
    args::{self, Config, ConfigInput, DesktopSetup, ExtendIntoString, PackageManager, ThemeSetup, DMSetup, ShellSetup, set_base, is_arch, is_nix},
    files,
    partition,
    returncode_eval::files_eval,
    strings::crash,
};
use std::{
    fs, path::PathBuf,
};

fn merge_values(dst: &mut Value, src: Value) {
    match (dst, src) {
        (Value::Object(a), Value::Object(b)) => {
            for (k, v) in b {
                merge_values(a.entry(k).or_insert(Value::Null), v);
            }
        }
        // Replace arrays by default (predictable). If you'd rather append, change this arm.
        (dst_slot @ Value::Array(_), Value::Array(b)) => {
            *dst_slot = Value::Array(b);
        }
        // For all other cases, override.
        (dst_slot, v) => {
            *dst_slot = v;
        }
    }
}

// Parse a string that may be:
//  - a single JSON object
//  - a JSON array of objects
//  - NDJSON (one object per line)
// Returns a list of JSON Values to be merged in order.
fn parse_fragments_from_str(label: &str, s: &str) -> Vec<Value> {
    // Try as one JSON value first
    if let Ok(val) = serde_json::from_str::<Value>(s) {
        match val {
            Value::Array(arr) => arr,
            other => vec![other],
        }
    } else {
        // Fallback: NDJSON by lines
        let mut out = Vec::new();
        for (i, line) in s.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() { continue; }
            match serde_json::from_str::<Value>(line) {
                Ok(v) => out.push(v),
                Err(e) => {
                    crash(
                        format!("Parse JSON (NDJSON) from {label} line {} ERROR: {e}", i + 1),
                        1,
                    );
                }
            }
        }
        if out.is_empty() {
            crash(format!("No valid JSON found in {label}"), 1);
        }
        out
    }
}

fn unwrap_known_roots(root: &mut serde_json::Value) {
    // snapshot the wrapped objects (if any) without holding the borrow
    let (cfg, drv) = match root {
        serde_json::Value::Object(map) => (map.get("config").cloned(), map.get("drives").cloned()),
        _ => (None, None),
    };

    // merge their contents into the root
    if let Some(c) = cfg { merge_values(root, c); }
    if let Some(d) = drv { merge_values(root, d); }

    // drop the wrapper keys to avoid surprises
    if let serde_json::Value::Object(map) = root {
        map.remove("config");
        map.remove("drives");
    }
}

pub fn read_config(inputs: &[ConfigInput]) -> Config {
    let mut merged = Value::Object(Map::new());
    
    for (idx, input) in inputs.iter().enumerate() {
        match input {
            ConfigInput::File(path) => {
                // Read original content
                let data = fs::read_to_string(path).unwrap_or_else(|e| {
                    crash(format!("Read config file {path:?}  ERROR: {e}"), e.raw_os_error().unwrap_or(1))
                });

                // Redact on disk for logging (keeps your existing behavior)
                // NOTE: This modifies the file(s). If you do not want that, remove these calls
                // and just log a redacted copy from memory (as done for JSON strings).
                files_eval(
                    files::sed_file(
                        path.to_str().unwrap(),
                        "\"password_hash\":.*",
                        "\"password_hash\": \"*REDACTED*\",",
                    ),
                    "Redact user information",
                );
                files_eval(
                    files::sed_file(
                        path.to_str().unwrap(),
                        "\"root_passwd_hash\":.*",
                        "\"root_passwd_hash\": \"*REDACTED*\",",
                    ),
                    "Redact root information",
                );

                // Log redacted on-disk content
                let redacted = fs::read_to_string(path)
                    .expect("Failed to read config file after redaction");
                info!("Configuration fragment #{idx} (file: {}):\n{redacted}", path.display());

                // Merge original (unredacted) content
                let frags = parse_fragments_from_str(&format!("file {}", path.display()), &data);
                for frag in frags {
                    merge_values(&mut merged, frag);
                }

                debug!("[ \x1b[2;1;32mOK\x1b[0m ] Parsed and merged config file {path:?}");
            }

            ConfigInput::JsonString(s) => {
                info!("Configuration fragment #{idx} (json/string or stdin):\n{s}");

                // Merge original
                let frags = parse_fragments_from_str("JSON string/STDIN", s);
                for frag in frags {
                    merge_values(&mut merged, frag);
                }

                debug!("[ \x1b[2;1;32mOK\x1b[0m ] Parsed and merged config from JSON string/STDIN");
            }
        }
    }

    unwrap_known_roots(&mut merged);

    // Deserialize into your strong `Config` type
    match serde_json::from_value::<Config>(merged) {
        Ok(cfg) => {
            debug!("[ \x1b[2;1;32mOK\x1b[0m ] Deserialized merged configuration into Config");
            cfg
        }
        Err(e) => {
            crash(format!("Merged configuration is invalid for Config  ERROR: {e}"), 1);
        }
    }
}

pub fn install_config(inputs: &[ConfigInput], log_path: String) -> i32 {
    let config = read_config(inputs);
    set_base(&config.base);
    let mut exit_code = 0;
    let kernel = "linux-lts";
    //let kernel = "linux";
    let mut package_set = packages::to_strings(packages::COMMON);
    
    packages::extend(&mut package_set, packages::ARCH_ONLY);

    /*    PARTITIONING    */
    let part_table = config.partition.content.table_type;
    let mode = config.partition.mode;
    info!("Disk device to use : {}", config.partition.device);
    info!("Partition Table type : {part_table}");
    info!("Partitioning mode: {mode}");

    // Build args::Partition list from the structured JSON.
    // Encrypt per-partition if flags contain "encrypt".
    let mut partitions: Vec<args::Partition> = Vec::new();
    for p in &config.partition.content.partitions {
        let action = p.action.clone(); // e.g. create, modify, delete
        let mountpoint    = p.mountpoint.clone(); // empty for swap
        let blockdevice   = p.blockdevice.clone();                         // "/dev/nvme0n1p2"
        let start   = p.start.clone();                         // start sector
        let end   = p.end.clone();                         // end sector
        let filesystem    = p.filesystem.clone();      // "ext4", "vfat", "swap"
        let flags         = p.flags.clone();

        partitions.push(args::Partition::new(
            action, mountpoint, blockdevice, start, end, filesystem, flags,
        ));
    }

    // Handle both "/dev/nvme0n1" and "nvme0n1"
    let device = if config.partition.device.starts_with("/dev/") {
        PathBuf::from(&config.partition.device)
    } else {
        PathBuf::from("/dev").join(&config.partition.device)
    };
    
    partition::partition(
        device,
        &part_table,
        &mode,
        &mut partitions,
    );

    if is_nix() {
        base::install_nix_config(&config.partition.device);
    }
    /* BOOTLOADER SET */

    if partition::is_uefi() {
        package_set.push("efibootmgr".into());
        package_set.push("mokutil".into());
        package_set.push("sbsigntools".into());
        package_set.push("shim-signed".into());
        package_set.push("mokutil".into());
        package_set.push("systemd-ukify".into());
    }
    partition::partition_info();
    /**************************/
    /*        DESKTOP         */
    info!("Selected desktop : {:?}", config.desktop);
    let desktop = match config.desktop.trim().to_lowercase().as_str() {
        "onyx" => Some(DesktopSetup::Onyx),
        "kde plasma" => Some(DesktopSetup::Kde),
        "mate" => Some(DesktopSetup::Mate),
        "gnome" => Some(DesktopSetup::Gnome),
        "cinnamon" => Some(DesktopSetup::Cinnamon),
        "xfce picom" => Some(DesktopSetup::XfcePicom),
        "xfce" => Some(DesktopSetup::XfceRefined),
        "budgie" => Some(DesktopSetup::Budgie),
        "enlightenment" => Some(DesktopSetup::Enlightenment),
        "lxqt" => Some(DesktopSetup::Lxqt),
        "sway" => Some(DesktopSetup::Sway),
        "i3" => Some(DesktopSetup::I3),
        "herbstluftwm" => Some(DesktopSetup::Herbstluftwm),
        "awesome" => Some(DesktopSetup::Awesome),
        "bspwm" => Some(DesktopSetup::Bspwm),
        "hyprland" => Some(DesktopSetup::Hyprland),
        "none" => Some(DesktopSetup::None),
        _ => None,
    };

    if let Some(desktop) = desktop {
        package_set.extend_into(desktops::install_desktop_setup(desktop));
    } else {
        info!("No desktop setup selected!");
    }
    /**************************/

    /*     DISPLAY MANAGER    */
    info!("Selected display manager : {:?}", config.displaymanager);
    let dm = match config.displaymanager.trim().to_lowercase().as_str() {
        "gdm" => Some(DMSetup::Gdm),
        "lightdm neon" => Some(DMSetup::LightDMNeon),
        "ly" => Some(DMSetup::Ly),
        // all these map to Sddm
        "astronaut" | "black hole" | "cyberpunk" | "cyborg" | "jake the dog"
        | "kath" | "pixel sakura" | "post-apocalypse" | "purple leaves"
            => Some(DMSetup::Sddm),

        _ => None,
    };

    if let Some(dm) = dm {
        package_set.extend_into(displaymanagers::install_dm_setup(dm));
    } else {
        info!("No display manager setup selected!");
    }
    /**************************/
    /*         DESIGN         */
    info!("Selected design : {:?}", config.design);
    let theme = match config.design.trim().to_lowercase().as_str() {
        "cyborg" => Some(ThemeSetup::Cyborg),
        "frost" => Some(ThemeSetup::Frost),
        "graphite" => Some(ThemeSetup::Graphite),
        "hackthebox" => Some(ThemeSetup::HackTheBox),
        "redmoon" => Some(ThemeSetup::RedMoon),
        "samurai" => Some(ThemeSetup::Samurai),
        "sweet" => Some(ThemeSetup::Sweet),
        "temple" => Some(ThemeSetup::Temple),
        _ => None,
    };

    if let Some(theme) = theme {
        package_set.extend_into(themes::install_theme_setup(theme));
    } else {
        info!("No design setup selected!");
    }
    /**************************/
    /*    EXTRA PACKAGES    */
    info!("Extra packages : {:?}", config.extra_packages);
    package_set.extend(config.extra_packages.clone());
    /**************************/
    /*         USERS         */
    for user in &config.users {
        let shell = match user.shell.trim().to_lowercase().as_str() {
            "bash" => Some(ShellSetup::Bash),
            "fish" => Some(ShellSetup::Fish),
            "zsh" => Some(ShellSetup::Zsh),
            _ => None,
        };

        if let Some(shell) = shell {
            package_set.extend_into(shells::install_shell_setup(shell));
        } else {
            info!("No shell setup selected!");
        }
    }
    /**************************/
    /*        KEYBOARD       */
    fs::create_dir_all("/mnt/etc").unwrap();
    let chosen_kbd = config.keyboard_layout.as_deref().unwrap_or("us");
    info!("Using keymap : {chosen_kbd}");
    if let Err(e) = locale::set_keyboard(chosen_kbd) {
        error!("Error setting keyboard configuration: {e}");
    }
    
    /********** INSTALLATION **********/
    if !is_nix() {
        package_set.sort();
        package_set.dedup();
        exit_code = base::install_packages(package_set, kernel);
        if exit_code != 0 {
            return exit_code;
        }
        base::genfstab();
    }

    /********** CONFIGURATION **********/
    /*         LOCALES        */
    // Set locales at the beginning to prevent some warning messages about "Setting locale failed"
    info!("Adding Locale : {}", config.locale);
    locale::set_locale(config.locale.clone());
    info!("Setting timezone : {}", config.timezone);
    locale::set_timezone(config.timezone.as_str());
    /**************************/
    info!("Hostname : {}", config.hostname);
    network::set_hostname(config.hostname.as_str());

    /**************************/
    if !is_nix() {
        network::create_hosts();
        /**************************/
        /*     DESKTOP CONFIG     */
        info!("Configuring desktop : {:?}", config.desktop);
        match config.desktop.to_lowercase().as_str() {
            "gnome" => desktops::configure_gnome(),
            "cinnamon" => desktops::configure_cinnamon(),
            "hyprland" => desktops::configure_hyprland(),
            "xfce" => desktops::configure_xfce(),
            _ => info!("No desktop configuration needed."),
        }
        /**************************/
        /*      DESIGN CONFIG     */
        info!("Configuring design : {:?}", config.design);
        match config.design.to_lowercase().as_str() {
            "cyborg" => themes::configure_cyborg(),
            "frost" => themes::configure_frost(),
            "graphite" => themes::configure_graphite(),
            "hackthebox" => themes::configure_hackthebox(),
            "redmoon" => themes::configure_redmoon(),
            "samurai" => themes::configure_samurai(),
            "sweet" => themes::configure_sweet(),
            "temple" => themes::configure_temple(),
            _ => info!("No design configuration needed."),
        }

        /**************************/
        /*     SHELL CONFIG     */
        // The shell of the first created user will be applied on shell.desktop and on SHELL variable
        match config.users[0].shell.to_lowercase().as_str() {
            "fish" => shells::configure_fish(),
            "zsh" => shells::configure_zsh(),
            _ => info!("No shell configuration needed."),
        }
    }

    /* DISPLAY MANAGER CONFIG */
    info!("Configuring display manager : {:?}", config.displaymanager);
    match config.displaymanager.to_lowercase().as_str() {
        "gdm" => displaymanagers::configure_gdm(&config.desktop),
        "lightdm neon" => displaymanagers::configure_lightdm_neon(&config.desktop),
        "ly" => displaymanagers::configure_ly(),
        //"sddm" => displaymanagers::configure_sddm(),
        "astronaut" => displaymanagers::configure_sddm_astronaut(),
        "black hole" => displaymanagers::configure_sddm_blackhole(),
        "cyberpunk" => displaymanagers::configure_sddm_cyberpunk(),
        "cyborg" => displaymanagers::configure_sddm_cyborg(),
        "jake the dog" => displaymanagers::configure_sddm_jake(),
        "kath" => displaymanagers::configure_sddm_kath(),
        "pixel sakura" => displaymanagers::configure_sddm_pixelsakura(),
        "post-apocalypse" => displaymanagers::configure_sddm_postapocalypse(),
        "purple leaves" => displaymanagers::configure_sddm_purpleleaves(),
        _ => info!("No display manager configuration needed."),
    }

    /*      USER CONFIG      */
    for i in 0..config.users.len() {
        info!("Creating user : {}", config.users[i].name);
        //info!("Setting user password : {}", config.users[i].password.as_str());
        info!("Setting user shell : {}", config.users[i].shell);

        users::new_user(
            config.users[i].name.as_str(),
            config.users[i].password.as_str(),
            &config.users[i].groups,
            "bash", //config.users[i].shell.as_str(), // Use bash because it must be the shell associated to the user in order to source the initial .sh files at login time
        );
    }
    //info!("Setting root password : {}", config.rootpass.as_str());
    users::root_pass(config.rootpass.as_str());

    /*    BOOTLOADER CONFIG     */
    // mokutil needs to be run after root password creation
    if is_arch() && partition::is_uefi() {
        base::configure_bootloader_systemd_boot_shim(PathBuf::from("/efi"));
    }

    if is_nix() {
        info!("Install Athena OS");
        exit_code = install(PackageManager::Nix, vec![], None);
        if exit_code != 0 {
            return exit_code;
        }
    }
    
    /**************************/

    /**************************/
    if !is_nix() {
        /*    ENABLE SERVICES    */
        info!("Enabling system services...");
        base::enable_system_services();
    }
    /**************************/
    files::copy_multiple_files("/etc/NetworkManager/system-connections/*", "/mnt/etc/NetworkManager/system-connections/");

    info!("Installation log file copied to /var/log/aegis.log");
    files_eval(files::create_directory("/mnt/var/log"), "Create /mnt/var/log");
    files::copy_file(&log_path, "/mnt/var/log/aegis.log");

    partition::umount("/mnt"); // Recursive umount

    // The closing of LUKS must be after unmount
    for p in &config.partition.content.partitions {
        if p.flags.iter().any(|f| f.eq_ignore_ascii_case("encrypt")) {
            // p.blockdevice is the *underlying* partition (e.g., /dev/vda2)
            shared::partition::close_luks_best_effort(&p.blockdevice);
        }
    }

    exit_code
}
