use crate::{
  functions::*,
  internal::{install::install, packages},
};
use log::{debug, error, info, warn};
use regex::Regex;
use serde_json::{Map, Value};
use shared::{
  args::{
    Config, ConfigInput, DMSetup, DesktopSetup, ExtendIntoString, PackageManager, ShellSetup,
    ThemeSetup, is_arch, is_nix, set_base,
  },
  files, partition,
  returncode_eval::files_eval,
  strings::crash,
};
use std::{fs, path::PathBuf, sync::LazyLock};

fn merge_values(dst: &mut Value, src: Value) {
  match (dst, src) {
    (Value::Object(a), Value::Object(b)) => {
      for (k, v) in b {
        merge_values(a.entry(k).or_insert(Value::Null), v);
      }
    }
    // Arrays are replaced, not appended (predictable).
    (dst_slot, v) => *dst_slot = v,
  }
}

/// Parse a string that may be a single JSON object, a JSON array of objects,
/// or NDJSON (one object per line). Returns the values to merge in order.
fn parse_fragments_from_str(label: &str, s: &str) -> Vec<Value> {
  if let Ok(val) = serde_json::from_str::<Value>(s) {
    return match val {
      Value::Array(arr) => arr,
      other => vec![other],
    };
  }
  let out: Vec<Value> = s
    .lines()
    .enumerate()
    .map(|(i, line)| (i, line.trim()))
    .filter(|(_, line)| !line.is_empty())
    .map(|(i, line)| {
      serde_json::from_str::<Value>(line).unwrap_or_else(|e| {
        crash(
          format!("Parse JSON (NDJSON) from {label} line {} ERROR: {e}", i + 1),
          1,
        )
      })
    })
    .collect();
  if out.is_empty() {
    crash(format!("No valid JSON found in {label}"), 1);
  }
  out
}

/// Flatten `{ "config": {...} }` and `{ "drives": {...} }` wrappers into the root.
fn unwrap_known_roots(root: &mut Value) {
  let (cfg, drv) = match root {
    Value::Object(map) => (map.remove("config"), map.remove("drives")),
    _ => (None, None),
  };
  if let Some(c) = cfg {
    merge_values(root, c);
  }
  if let Some(d) = drv {
    merge_values(root, d);
  }
}

/// Copy of the text with password hashes blanked, for logging only. The
/// input file is never modified.
fn redacted(text: &str) -> std::borrow::Cow<'_, str> {
  static RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""(password_hash|root_passwd_hash)"\s*:\s*"[^"]*""#).expect("regex")
  });
  RE.replace_all(text, r#""$1": "*REDACTED*""#)
}

pub fn read_config(inputs: &[ConfigInput]) -> Config {
  let mut merged = Value::Object(Map::new());

  for (idx, input) in inputs.iter().enumerate() {
    let (label, data) = match input {
      ConfigInput::File(path) => {
        let data = fs::read_to_string(path).unwrap_or_else(|e| {
          crash(
            format!("Read config file {path:?}  ERROR: {e}"),
            e.raw_os_error().unwrap_or(1),
          )
        });
        (format!("file {}", path.display()), data)
      }
      ConfigInput::JsonString(s) => ("JSON string/STDIN".to_string(), s.clone()),
    };
    info!(
      "Configuration fragment #{idx} ({label}):\n{}",
      redacted(&data)
    );
    for frag in parse_fragments_from_str(&label, &data) {
      merge_values(&mut merged, frag);
    }
    debug!("Parsed and merged config from {label}");
  }

  unwrap_known_roots(&mut merged);

  match serde_json::from_value::<Config>(merged) {
    Ok(cfg) => {
      debug!("Deserialized merged configuration into Config");
      cfg
    }
    Err(e) => crash(
      format!("Merged configuration is invalid for Config  ERROR: {e}"),
      1,
    ),
  }
}

/// Parse an option string, logging and returning `None` when it is unknown.
fn parse_opt<T: std::str::FromStr<Err = shared::args::UnknownOption>>(
  what: &str,
  s: &str,
) -> Option<T> {
  match s.parse::<T>() {
    Ok(v) => Some(v),
    Err(e) => {
      warn!("No {what} setup selected ({e})");
      None
    }
  }
}

pub fn install_config(inputs: &[ConfigInput], log_path: &str) -> i32 {
  let config = read_config(inputs);
  set_base(&config.base);

  if config.users.is_empty() {
    crash("At least one user account is required", 1);
  }
  if is_arch() && !partition::is_uefi() {
    crash(
      "Athena OS requires a UEFI system (systemd-boot); legacy BIOS boot is not supported",
      1,
    );
  }

  let mut package_set = packages::to_strings(packages::COMMON);
  package_set.extend_into(packages::ARCH_ONLY.iter().copied());

  /*    PARTITIONING    */
  let part_table = &config.partition.content.table_type;
  let mode = &config.partition.mode;
  info!("Disk device to use : {}", config.partition.device);
  info!("Partition Table type : {part_table}");
  info!("Partitioning mode: {mode}");

  // Handle both "/dev/nvme0n1" and "nvme0n1"
  let device = if config.partition.device.starts_with("/dev/") {
    PathBuf::from(&config.partition.device)
  } else {
    PathBuf::from("/dev").join(&config.partition.device)
  };

  partition::partition(
    device,
    part_table,
    mode,
    &config.partition.content.partitions,
  );

  if is_nix() {
    base::install_nix_config(&config.partition.device);
  }

  /* BOOTLOADER PACKAGES */
  if partition::is_uefi() {
    package_set.extend_into(packages::UEFI.iter().copied());
  }
  partition::partition_info();

  /* DESKTOP / DISPLAY MANAGER / DESIGN: parse once, use everywhere */
  info!("Selected desktop : {:?}", config.desktop);
  let desktop = parse_opt::<DesktopSetup>("desktop", &config.desktop);
  if let Some(desktop) = desktop {
    package_set.extend_into(desktops::install_desktop_setup(desktop));
  }

  info!("Selected display manager : {:?}", config.displaymanager);
  let dm = parse_opt::<DMSetup>("display manager", &config.displaymanager);
  if let Some(dm) = dm {
    package_set.extend_into(displaymanagers::install_dm_setup(dm));
  }

  info!("Selected design : {:?}", config.design);
  let theme = parse_opt::<ThemeSetup>("design", &config.design);
  if let Some(theme) = theme {
    package_set.extend_into(themes::install_theme_setup(theme));
  }

  /*    EXTRA PACKAGES    */
  info!("Extra packages : {:?}", config.extra_packages);
  package_set.extend_into(&config.extra_packages);

  /*         USERS         */
  let user_shells: Vec<Option<ShellSetup>> = config
    .users
    .iter()
    .map(|u| parse_opt::<ShellSetup>("shell", &u.shell))
    .collect();
  for shell in user_shells.iter().flatten() {
    package_set.extend_into(shells::install_shell_setup(*shell));
  }

  /*        KEYBOARD       */
  files_eval(files::create_directory("/mnt/etc"), "Create /mnt/etc");
  let chosen_kbd = config.keyboard_layout.as_deref().unwrap_or("us");
  info!("Using keymap : {chosen_kbd}");
  if let Err(e) = locale::set_keyboard(chosen_kbd) {
    error!("Error setting keyboard configuration: {e}");
  }

  /********** INSTALLATION **********/
  if !is_nix() {
    package_set.sort_unstable();
    package_set.dedup();
    let exit_code = base::install_packages(package_set);
    if exit_code != 0 {
      return exit_code;
    }
    base::genfstab();
  }

  /********** CONFIGURATION **********/
  // Set locales first to prevent "Setting locale failed" warnings from chroot commands.
  info!("Adding Locale : {}", config.locale);
  locale::set_locale(&config.locale);
  info!("Setting timezone : {}", config.timezone);
  locale::set_timezone(&config.timezone);
  info!("Hostname : {}", config.hostname);
  network::set_hostname(&config.hostname);

  if !is_nix() {
    network::create_hosts();
    info!("Configuring desktop : {:?}", config.desktop);
    if let Some(desktop) = desktop {
      desktops::configure(desktop);
    }
    info!("Configuring design : {:?}", config.design);
    if let Some(theme) = theme {
      themes::configure(theme);
    }
    // The shell of the first created user is applied to shell.desktop and $SHELL
    if let Some(Some(shell)) = user_shells.first() {
      shells::configure(*shell);
    }
  }

  info!("Configuring display manager : {:?}", config.displaymanager);
  if let Some(dm) = dm {
    displaymanagers::configure(dm, desktop.unwrap_or(DesktopSetup::None));
  }

  /*      USER CONFIG      */
  for user in &config.users {
    info!("Creating user : {} (shell {})", user.name, user.shell);
    // Use bash as the login shell: it must source the initial .sh files at login time.
    users::new_user(&user.name, &user.password, &user.groups, "bash");
  }
  if !is_nix() {
    users::enable_wheel_sudo();
  }
  users::root_pass(&config.rootpass);

  /*    BOOTLOADER CONFIG     */
  // mokutil needs to be run after root password creation
  if is_arch() && partition::is_uefi() {
    base::configure_bootloader_systemd_boot_shim("/efi");
  }

  let mut exit_code = 0;
  if is_nix() {
    info!("Install Athena OS");
    exit_code = install(PackageManager::Nix, &[]);
    if exit_code != 0 {
      return exit_code;
    }
  }

  if !is_nix() {
    info!("Enabling system services...");
    base::enable_system_services();
  }

  files::copy_multiple_files(
    "/etc/NetworkManager/system-connections/*",
    "/mnt/etc/NetworkManager/system-connections/",
  );

  info!("Installation log file copied to /var/log/dory.log");
  files_eval(
    files::create_directory("/mnt/var/log"),
    "Create /mnt/var/log",
  );
  files::copy_file(log_path, "/mnt/var/log/dory.log");

  partition::umount("/mnt"); // Recursive umount

  // The closing of LUKS must be after unmount
  for p in config
    .partition
    .content
    .partitions
    .iter()
    .filter(|p| p.has_flag("encrypt"))
  {
    // p.blockdevice is the *underlying* partition (e.g., /dev/vda2)
    partition::close_luks_best_effort(&p.blockdevice);
  }

  exit_code
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn redaction_keeps_json_valid() {
    let input = r#"{"users":[{"username":"a","password_hash":"$6$abc$def"}],"root_passwd_hash":"$6$x$y","hostname":"h"}"#;
    let out = redacted(input);
    assert!(out.contains(r#""password_hash": "*REDACTED*""#));
    assert!(out.contains(r#""root_passwd_hash": "*REDACTED*""#));
    assert!(!out.contains("$6$"));
    serde_json::from_str::<Value>(&out).expect("redacted text must still be JSON");
  }

  #[test]
  fn roots_are_unwrapped() {
    let mut v = serde_json::json!({"config": {"hostname": "x"}, "drives": {"device": "/dev/sda"}});
    unwrap_known_roots(&mut v);
    assert_eq!(v["hostname"], "x");
    assert_eq!(v["device"], "/dev/sda");
    assert!(v.get("config").is_none());
  }
}
