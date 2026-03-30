use crate::internal::hardware;
use crate::internal::install::install;
use crate::internal::services::enable_service;
use efivar::{self, efi, system};
use log::{info, error};
use shared::args::{ExecMode, ExtendIntoString, OnFail, PackageManager, is_arch};
use shared::exec::{exec, exec_output};
use shared::encrypt::{find_target_root_luks, tpm2_available_esapi};
use shared::files;
use shared::returncode_eval::{exec_eval, exec_eval_result, files_eval};
use std::{fs, path::{Path, PathBuf}};

pub fn install_packages(mut packages: Vec<String>, kernel: &str) -> i32 {
    let kernels = vec![
        kernel.to_string(),              // the user-selected kernel (e.g. "linux-aegis-offensive")
        "linux-aegis-hardened".to_string(),    // always install hardened too
    ];
    let kernel_headers: Vec<String> =
        kernels.iter().map(|k| format!("{}-headers", k)).collect();

    let arch_base_pkg: Vec<String> = vec![
        // Base Arch
        "pacman".into(),
        "mkinitcpio".into(),
        "glibc-locales".into(), // Prebuilt locales to prevent locales warning message during the pacstrap install of base metapackage
        // Repositories
        "athena-mirrorlist".into(),
        "blackarch-mirrorlist".into(),
        "chaotic-mirrorlist".into(),
        "rate-mirrors".into(),
        "archlinux-keyring".into(),
        "athena-keyring".into(),
        "blackarch-keyring".into(),
        "chaotic-keyring".into(),
    ];

    packages.extend_into(&kernels);
    packages.extend_into(kernel_headers);

    if tpm2_available_esapi() {
        packages.extend_into(["tpm2-tools"]);
    }

    /***** CHECK IF BTRFS *****/
    let (fstype, _fs_uuid) = detect_root_fs_info();
    if fstype == "btrfs" {
        packages.extend_into(["btrfs-progs"]);
    }
    info!("Root partition is {fstype}");

    let (virt_packages, virt_services, virt_params) = hardware::virt_check();
    let cpu_packages = hardware::cpu_check();
    let gpu_packages = hardware::gpu_check(kernel); // linux-aegis-offensive
    packages.extend_into(virt_packages);
    packages.extend_into(cpu_packages);
    packages.extend_into(gpu_packages);

    if is_arch() {
        init_keyrings_mirrors(); // Need to initialize keyrings before installing base package group otherwise get keyring errors. It uses rate-mirrors for Arch and Chaotic AUR on the host
        files::copy_file("/etc/pacman.conf", "/mnt/etc/pacman.conf"); // It must be done before installing any Athena and Chaotic AUR package
    }

    let code = install(
        PackageManager::Pacstrap,
        arch_base_pkg,
        None
    ); // By installing it as first package, we can work on its config files without building the initramfs image. It must be done only one time at the end of the kernel package installation
    if code != 0 {
        // Log if you want
        error!("mkinitcpio installation failed with exit code {code}");
        return code; // or just `code` if you don't want early return
    }
    
    files::copy_file("/etc/pacman.d/mirrorlist", "/mnt/etc/pacman.d/mirrorlist"); // It must run after "pacman-mirrorlist" pkg install, that is in base package group
    files::copy_file("/etc/pacman.d/athena-mirrorlist", "/mnt/etc/pacman.d/athena-mirrorlist");
    files::copy_file("/etc/pacman.d/blackarch-mirrorlist", "/mnt/etc/pacman.d/blackarch-mirrorlist");
    files::copy_file("/etc/pacman.d/chaotic-mirrorlist", "/mnt/etc/pacman.d/chaotic-mirrorlist");
    
    hardware::set_cores();
    exec_eval(
        exec( // Using direct exec instead of ArchChroot exec because in the latter, these sed arguments need some chars to be escaped
            ExecMode::Direct,
            "sed",
            vec![
                "-i".into(),
                "-e".into(),
                "s/^HOOKS=.*/HOOKS=(base systemd autodetect modconf kms keyboard sd-vconsole block sd-encrypt lvm2 filesystems fsck)/g".into(),
                "/mnt/etc/mkinitcpio.conf".into(),
            ],
            OnFail::Error,
        ),
        "Set mkinitcpio hooks.",
    );
    files_eval(
        files::sed_file(
            "/mnt/etc/mkinitcpio.conf",
            "#COMPRESSION=\"lz4\"",
            "COMPRESSION=\"gzip\"", // systemd-stub (and therefore UKI) expects an initrd compressed with gzip
        ),
        "Set compression algorithm.",
    );
    /*
    let preset_dir = "/mnt/etc/mkinitcpio.d";
    std::fs::create_dir_all(preset_dir).unwrap();
    // By creating our custom presets (with UKI entry), the kernel installation won't create its default preset files
    for k in &kernels {
        let preset_path = format!("{}/{}.preset", preset_dir, k);
        let content = format!(
r#"ALL_config="/etc/mkinitcpio.conf"
ALL_kver="/boot/vmlinuz-{kernel}"
                
PRESETS=('default')
                
default_uki="/efi/EFI/Athena/{kernel}.efi"
default_options=""
"#,
            kernel = k,
        );
    
        std::fs::write(&preset_path, content)
            .expect("Failed to write mkinitcpio preset.");
        info!("Created mkinitcpio preset: {}", preset_path);
    }
    */

    if let Some(vendor) = sys_vendor() {
        let vendor_lc = vendor.to_lowercase();

        if vendor_lc.contains("apple") {
            info!("Detected Apple hardware (sys_vendor='{vendor}'). Setting Mac-specific MODULES.");

            exec_eval(
                exec(
                    ExecMode::Direct,
                    "sed",
                    vec![
                        "-i".into(),
                        "-e".into(),
                        r"/^MODULES=()/ s/()/(usb_storage uas xhci_hcd ahci libahci sd_mod usbhid hid_apple xhci_pci ehci_pci ohci_hcd uhci_hcd)/".into(),
                        "-e".into(),
                        r"/^MODULES=([^)]*)/ { /usb_storage uas xhci_hcd ahci libahci sd_mod usbhid hid_apple xhci_pci ehci_pci ohci_hcd uhci_hcd/! s/)/ usb_storage uas xhci_hcd ahci libahci sd_mod usbhid hid_apple xhci_pci ehci_pci ohci_hcd uhci_hcd)/ }".into(),
                        "/mnt/etc/mkinitcpio.conf".into(),
                    ],
                    OnFail::Error,
                ),
                "Set mkinitcpio MODULES for Apple computer.",
            );
        }
    } else {
        info!("Could not read /sys/class/dmi/id/sys_vendor. Leaving MODULES as default.");
    }

    // Apply sed commands for virt service for mkinitcpio.conf
    for (description, args) in virt_params {
        exec_eval(
            exec(ExecMode::Direct, "sed", args, OnFail::Error),  // Apply each file change via `sed`
            &description,       // Log the description of the file change
        );
    }

    let code = install(PackageManager::Pacman, packages, None);
    if code != 0 {
        error!("Package installation failed with exit code {code}");
        return code;
    }
    files::copy_file("/mnt/usr/local/share/athena/release/os-release-athena", "/mnt/usr/lib/os-release");
    files::copy_file("/etc/skel/.bashrc", "/mnt/etc/skel/.bashrc");

    // Enable the necessary services after installation
    for service in virt_services {
        enable_service(service);
    }

    files_eval(
        files::sed_file(
            "/mnt/etc/nsswitch.conf",
            "hosts:.*",
            "hosts: mymachines resolve [!UNAVAIL=return] files dns mdns wins myhostname",
        ),
        "Set nsswitch configuration",
    );
    code
}

fn generate_kernel_cmdline() -> String {
    let (fstype, fs_uuid) = detect_root_fs_info();
    let is_btrfs_root = fstype == "btrfs";

    let mut early_root_param = String::new();

    if let Some(root) = find_target_root_luks() {
        // Encrypted root (the one backing /mnt)
        // Use the *live* mapper name if we have it; that ensures rd.luks.name matches reality.
        early_root_param.push_str(&format!("rd.luks.name={}={} ", root.luks_uuid, root.mapper_dev));
        early_root_param.push_str(&format!("root=/dev/mapper/{} ", root.mapper_dev));
        if tpm2_available_esapi() {
            early_root_param.push_str("rd.luks.options=tpm2-device=auto ");
        }
    } else {
        // Unencrypted root
        if !fs_uuid.is_empty() {
            early_root_param.push_str(&format!("root=UUID={fs_uuid} "));
        } else {
            error!("Could not determine root UUID for unencrypted root");
        }
    }

    if is_btrfs_root {
        early_root_param.push_str("rootflags=subvol=@ ");
    }

    if tpm2_available_esapi() {
        early_root_param.push_str("systemd-measure=yes ");
    } 

    let mut params: Vec<&str> = vec![
        "lsm=landlock,lockdown,yama,integrity,apparmor,bpf",
        "quiet",
        "loglevel=3",
        "nvme_load=yes",
        "zswap.enabled=0",
        "nowatchdog",
    ];

    if hardware::is_hyperv_guest() {
        params.push("video=hyperv_fb:1920x1080"); // 1920x1080 is the maximum: https://wiki.archlinux.org/title/Hyper-V#Setting_resolution
    }

    format!("{early_root_param}{}", params.join(" "))
}

/// Optional: write cmdline to /etc/kernel/cmdline inside target system,
/// so future kernel/UKI regen tools know what to embed.
fn write_kernel_cmdline_file(cmdline: &str) {
    files_eval(files::create_directory("/mnt/etc/kernel"), "Create /mnt/etc/kernel");
    files::create_file("/mnt/etc/kernel/cmdline");
    files_eval(
        files::append_file("/mnt/etc/kernel/cmdline", cmdline),
        "Write /etc/kernel/cmdline",
    );
}

fn ensure_pcr_keys_in_chroot() {
    if !tpm2_available_esapi() {
        info!("TPM2 not available; skipping PCR key generation.");
        return;
    }

    // Where we keep PCR keys inside the target system
    let sysd_dir = "/etc/systemd";
    let sys_priv  = format!("{sysd_dir}/tpm2-pcr-private-key-system.pem");
    let sys_pub   = format!("{sysd_dir}/tpm2-pcr-public-key-system.pem");
    let initrd_priv = format!("{sysd_dir}/tpm2-pcr-initrd-private-key.pem");
    let initrd_pub  = format!("{sysd_dir}/tpm2-pcr-initrd-public-key.pem");

    // Check existence inside /mnt
    let need_system  = !std::path::Path::new(&format!("/mnt{sys_priv}")).exists()
                    || !std::path::Path::new(&format!("/mnt{sys_pub}")).exists();
    let need_initrd  = !std::path::Path::new(&format!("/mnt{initrd_priv}")).exists()
                    || !std::path::Path::new(&format!("/mnt{initrd_pub}")).exists();

    if !(need_system || need_initrd) {
        info!("PCR keys already present; skipping ukify genkey.");
        return;
    }

    // Make sure directory exists
    std::fs::create_dir_all(format!("/mnt{sysd_dir}"))
        .expect("Failed to create /mnt/etc/systemd");

    // ukify genkey: generate only the missing pairs (ukify requires that outputs don't exist).
    if need_system {
        exec_eval(
            exec(
                ExecMode::Chroot { root: "/mnt" },
                "ukify",
                vec![
                    "genkey".into(),
                    "--pcr-private-key".into(), sys_priv.clone(),
                    "--pcr-public-key".into(),  sys_pub.clone(),
                ],
                OnFail::Error,
            ),
            "Generate system PCR signing keypair",
        );
    }
    if need_initrd {
        exec_eval(
            exec(
                ExecMode::Chroot { root: "/mnt" },
                "ukify",
                vec![
                    "genkey".into(),
                    "--pcr-private-key".into(), initrd_priv.clone(),
                    "--pcr-public-key".into(),  initrd_pub.clone(),
                ],
                OnFail::Error,
            ),
            "Generate initrd PCR signing keypair",
        );
    }

    // Permissions are up to you; these are reasonable defaults
    exec_eval(
        exec(ExecMode::Chroot { root: "/mnt" }, "chmod", vec!["400".into(), sys_priv], OnFail::Error),
        "Restrict system PCR private key",
    );
    exec_eval(
        exec(ExecMode::Chroot { root: "/mnt" }, "chmod", vec!["400".into(), initrd_priv], OnFail::Error),
        "Restrict initrd PCR private key",
    );
}

/// Build and sign a Unified Kernel Image (UKI) for one kernel flavor
/// using Arch's ukify syntax.
///
/// kname: "linux-aegis-offensive" or "linux-aegis-hardened"
/// pretty: "LTS" or "Hardened" (for boot menu entry title)
/// esp_str: ESP mount path *inside chroot* (usually "/efi")
/// secureboot_key_dir: path *inside chroot* to the key dir ("/etc/secureboot/keys")
fn build_and_sign_uki(
    kname: &str,
    pretty: &str,
    esp_str: &str,
    secureboot_key_dir: Option<&str>,
    cmdline: &str,
) {
    let uki_out = format!("{esp_str}/EFI/Athena/{kname}.efi");

    // ensure ESP/EFI/Athena exists on target fs
    let athena_efi_dir = format!("/mnt{esp_str}/EFI/Athena");
    fs::create_dir_all(&athena_efi_dir)
        .expect("Failed to create /mnt<esp>/EFI/Athena");

    let cpu = hardware::cpu_detect();

    let mut args: Vec<String> = Vec::new();
    args.push("build".into());

    args.push("--linux".into());
    args.push(format!("/boot/vmlinuz-{kname}"));

    // microcode first, conditionally
    if cpu.contains("Intel") {
        args.push("--initrd".into());
        args.push("/boot/intel-ucode.img".into());
    } else if cpu.contains("AMD") {
        args.push("--initrd".into());
        args.push("/boot/amd-ucode.img".into());
    }

    // normal initramfs
    args.push("--initrd".into());
    args.push(format!("/boot/initramfs-{kname}.img"));

    args.push("--cmdline".into());
    args.push(cmdline.to_string());

    args.push("--os-release".into());
    args.push("/usr/lib/os-release".into());

    args.push("--uname".into());
    args.push(kname.to_string());

    if let Some(dir) = secureboot_key_dir {
        args.push("--signtool=sbsign".into());

        args.push("--secureboot-private-key".into());
        args.push(format!("{dir}/MOK.key"));

        args.push("--secureboot-certificate".into());
        args.push(format!("{dir}/MOK.crt"));
    }

    if tpm2_available_esapi() {
        // Make sure keys exist (safe if called already)
        // (Nop here if you call ensure_pcr_keys_in_chroot() earlier.)
        // ensure_pcr_keys_in_chroot();

        let sys_priv = "/etc/systemd/tpm2-pcr-private-key-system.pem";
        let sys_pub  = "/etc/systemd/tpm2-pcr-public-key-system.pem";
        let init_priv = "/etc/systemd/tpm2-pcr-initrd-private-key.pem";
        let init_pub  = "/etc/systemd/tpm2-pcr-initrd-public-key.pem";

        args.push("--measure".into());

        // Pair #1: "system" policy → full boot phases
        args.push("--pcr-private-key".into());
        args.push(sys_priv.into());
        args.push("--pcr-public-key".into());
        args.push(sys_pub.into());
        args.push("--phases".into());
        args.push("enter-initrd".into()); // default full chain :contentReference[oaicite:4]{index=4}

        // Pair #2: "initrd-only" policy → only up to switch-root
        args.push("--pcr-private-key".into());
        args.push(init_priv.into());
        args.push("--pcr-public-key".into());
        args.push(init_pub.into());
        args.push("--phases".into());
        args.push("enter-initrd".into());               // initrd-only policy :contentReference[oaicite:5]{index=5}. Btw this allows us to autounlock LUKS when we enroll PCR11
    }

    args.push("--output".into());
    args.push(uki_out.clone());

    exec_eval(
        exec(ExecMode::Chroot { root: "/mnt" }, "ukify", args, OnFail::Error),
        &format!("Create+sign UKI for {kname}"),
    );

    let entries_dir = format!("/mnt{esp_str}/loader/entries");
    fs::create_dir_all(&entries_dir)
        .expect("Failed to create loader/entries dir");
    let entry_path = format!("{entries_dir}/athena-{kname}.conf");

    files::create_file(&entry_path);
    exec_eval(
        files::append_file(
            &entry_path,
            &format!(
                "title   Athena OS ({pretty})\nefi     /EFI/Athena/{kname}.efi\n",
            ),
        ),
        &format!("Write systemd-boot entry for {kname}"),
    );

    info!("UKI for {kname} created at {uki_out} and loader entry {entry_path}");
}

pub fn configure_bootloader_systemd_boot_shim(espdir: PathBuf) {
    let esp_str = espdir.to_str().unwrap();
    info!("Configuring systemd-boot + UKI + shim Secure Boot in {esp_str}");

    let sb_supported = secure_boot_supported();
    let secureboot_key_dir = "/etc/secureboot/keys";

    ensure_pcr_keys_in_chroot();

    let cmdline = generate_kernel_cmdline();
    write_kernel_cmdline_file(&cmdline);

    let boot_is_mount = match exec_output(
        "findmnt",
        vec!["-n".into(), "-M".into(), "/mnt/boot".into()],
    ) {
        Ok(out) => out.status.success(),
        Err(_)  => false,
    };

    let mut bootctl_args: Vec<String> = vec![
        "--esp-path".into(),
        esp_str.to_string(),
    ];
    if boot_is_mount {
        info!("/boot is a separate partition; passing --boot-path to bootctl");
        bootctl_args.push("--boot-path".into());
        bootctl_args.push("/boot".into());
    } else {
        info!("/boot is not a separate partition; installing bootloader without --boot-path");
    }
    bootctl_args.push("install".into());

    exec_eval(
        exec(ExecMode::Chroot { root: "/mnt" }, "bootctl", bootctl_args, OnFail::Error),
        "Install systemd-boot",
    );

    if sb_supported {
        // 2. Generate Secure Boot keypair (MOK.key / MOK.crt / MOK.cer)
        std::fs::create_dir_all(format!("/mnt{secureboot_key_dir}"))
            .expect("Failed to create secureboot key dir");

        exec_eval(
            exec(
                ExecMode::Chroot { root: "/mnt" },
                "openssl",
                vec![
                    "req".into(),
                    "-newkey".into(), "rsa:2048".into(),
                    "-nodes".into(),
                    "-keyout".into(), format!("{secureboot_key_dir}/MOK.key"),
                    "-new".into(),
                    "-x509".into(),
                    "-sha256".into(),
                    "-days".into(), "3650".into(),
                    "-subj".into(), "/CN=Athena OS Secure Boot Key/".into(),
                    "-out".into(), format!("{secureboot_key_dir}/MOK.crt"),
                ],
                OnFail::Error,
            ),
            "Generate Athena Secure Boot keypair",
        );

        exec_eval(
            exec(
                ExecMode::Chroot { root: "/mnt" },
                "chmod",
                vec![
                    "400".into(),
                    format!("{secureboot_key_dir}/MOK.key"),
                ],
                OnFail::Error,
            ),
            "Restrict Secure Boot private key permissions",
        );

        exec_eval(
            exec(
                ExecMode::Chroot { root: "/mnt" },
                "openssl",
                vec![
                    "x509".into(),
                    "-outform".into(), "DER".into(),
                    "-in".into(),  format!("{secureboot_key_dir}/MOK.crt"),
                    "-out".into(), format!("{secureboot_key_dir}/MOK.cer"),
                ],
                OnFail::Error,
            ),
            "Generate DER (.cer) version of Athena Secure Boot cert",
        );

        // 3. Sign systemd-boot itself
        exec_eval(
            exec(
                ExecMode::Chroot { root: "/mnt" },
                "sbsign",
                vec![
                    "--key".into(),  format!("{secureboot_key_dir}/MOK.key"),
                    "--cert".into(), format!("{secureboot_key_dir}/MOK.crt"),
                    "--output".into(), format!("{esp_str}/EFI/systemd/systemd-bootx64.efi"),
                    format!("{esp_str}/EFI/systemd/systemd-bootx64.efi"),
                ],
                OnFail::Error,
            ),
            "Sign systemd-boot with Athena key",
        );

        // 4. Set up shim as stage0 so Secure Boot works out-of-the-box,
        //    and so first boot triggers MOK Manager instead of forcing firmware DB enrollment.
        //
        // Firmware will execute BOOTX64.EFI. We want that to be shim (Microsoft-signed),
        // so Secure Boot allows it immediately.
        //
        // Then shim will try to load "grubx64.efi". We give it *our signed systemd-boot*
        // under that name, and after the user enrolls AthenaSecureBoot.cer in MOK Manager,
        // shim will allow it.
        //
        // So:
        //   ESP/EFI/BOOT/BOOTX64.EFI      <- shimx64.efi (MS-signed, from shim-signed pkg)
        //   ESP/EFI/BOOT/grubx64.efi      <- signed systemd-bootx64.efi
        //
        std::fs::create_dir_all(format!("/mnt{esp_str}/EFI/BOOT"))
            .expect("Failed to create ESP/EFI/BOOT directory");

        // Copy shim
        files::copy_file(
            "/mnt/usr/share/shim-signed/shimx64.efi",
            &format!("/mnt{esp_str}/EFI/BOOT/BOOTX64.EFI"),
        );

        // Copy MokManager so shim can start MOK enrollment UI at first boot
        files::copy_file(
            "/mnt/usr/share/shim-signed/mmx64.efi",
            &format!("/mnt{esp_str}/EFI/BOOT/mmx64.efi"),
        );

        // Copy our signed systemd-boot binary where shim expects "grubx64.efi"
        files::copy_file(
            &format!("/mnt{esp_str}/EFI/systemd/systemd-bootx64.efi"),
            &format!("/mnt{esp_str}/EFI/BOOT/grubx64.efi"),
        );

        // 5. Copy Athena public cert somewhere obvious on ESP.
        //    Shim/MOK Manager will ask to enroll it on first boot (after mokutil below).
        std::fs::create_dir_all(format!("/mnt{esp_str}/EFI/Athena"))
            .expect("Failed to create ESP/EFI/Athena directory");
        files::copy_file(
            &format!("/mnt{secureboot_key_dir}/MOK.cer"),
            &format!("/mnt{esp_str}/EFI/Athena/AthenaSecureBoot.cer"),
        );

        // 6. Pre-register the Athena key with mokutil so first boot asks user
        //    "Enroll this key?" in MOK Manager. This avoids making them open BIOS UI.
        exec_eval(
            exec(
                ExecMode::Chroot { root: "/mnt" },
                "mokutil",
                vec![
                    "--import".into(),
                    format!("{secureboot_key_dir}/MOK.cer"),
                    "-P".into(), // no password prompt path. If you prefer pwd-confirm flow,
                                 // remove -P and handle mokutil --password instead.
                ],
                OnFail::Error,
            ),
            "Schedule AthenaSecureBoot.cer enrollment in MOK Manager at first boot",
        );

        info!("systemd-boot + UKI + shim configured. On first boot, MOK Manager will ask to enroll AthenaSecureBoot.cer; accept it to boot securely without touching firmware setup.");
    }

    // 7. Build + sign UKIs for BOTH kernels using the SAME cmdline string
    /*
    build_and_sign_uki(
        "linux",
        "Vanilla",
        esp_str,
        if sb_supported { Some(secureboot_key_dir) } else { None },
        &cmdline,
    );
    */
    build_and_sign_uki(
        "linux-aegis-offensive",
        "Offensive",
        esp_str,
        if sb_supported { Some(secureboot_key_dir) } else { None },
        &cmdline,
    );
    build_and_sign_uki(
        "linux-aegis-hardened",
        "Hardened",
        esp_str,
        if sb_supported { Some(secureboot_key_dir) } else { None },
        &cmdline,
    );

    // 8. Write loader.conf, pick linux-aegis-offensive as default
    let loader_dir = format!("/mnt{esp_str}/loader");
    let entries_dir = format!("{loader_dir}/entries");
    fs::create_dir_all(&entries_dir).expect("Failed to create loader/entries dir");

    files::create_file(&format!("{loader_dir}/loader.conf"));
    files_eval(
        files::append_file(
            &format!("{loader_dir}/loader.conf"),
            "default athena-linux-aegis-offensive.conf\ntimeout 3\nconsole-mode keep\neditor no\n",
        ),
        "Write loader.conf",
    );
    /*
    files_eval(
        files::append_file(
            &format!("{loader_dir}/loader.conf"),
            "default athena-linux.conf\ntimeout 3\nconsole-mode keep\neditor no\n",
        ),
        "Write loader.conf",
    );
    */
}

fn init_keyrings_mirrors() {
    info!("Getting fastest mirrors for your location");
    exec_eval(
        exec( // It is done on the live system
            ExecMode::Direct,
            "rate-mirrors",
            vec![
                String::from("--concurrency"),
                String::from("40"),
                String::from("--disable-comments"),
                String::from("--allow-root"),
                String::from("--save"),
                String::from("/etc/pacman.d/mirrorlist"), // It must be saved not in the chroot environment but on the host machine of Live Environment. Next, it will be copied automatically on the target system.
                String::from("arch"),
            ],
            OnFail::Continue,
        ),
        "Set fastest Arch Linux mirrors on the host",
    );

    exec_eval(
        exec(
            ExecMode::Direct,
            "rate-mirrors",
            vec![
                String::from("--concurrency"),
                String::from("40"),
                String::from("--disable-comments"),
                String::from("--allow-root"),
                String::from("--save"),
                String::from("/etc/pacman.d/blackarch-mirrorlist"),
                String::from("blackarch"),
            ],
            OnFail::Continue,
        ),
        "Set fastest mirrors from BlackArch on the target system",
    );
    
    exec_eval(
        exec(
            ExecMode::Direct,
            "rate-mirrors",
            vec![
                String::from("--concurrency"),
                String::from("40"),
                String::from("--disable-comments"),
                String::from("--allow-root"),
                String::from("--save"),
                String::from("/etc/pacman.d/chaotic-mirrorlist"), //In chroot we don't need to specify /mnt
                String::from("chaotic-aur"),
            ],
            OnFail::Continue,
        ),
        "Set fastest mirrors from Chaotic AUR on the target system",
    );
    
    info!("Upgrade keyrings on the host");
    exec_eval(
        exec(
            ExecMode::Direct,
            "rm",
            vec![
                String::from("-rf"),
                String::from("/etc/pacman.d/gnupg"),
            ],
            OnFail::Error,
        ),
        "Removing keys",
    );
    exec_eval(
        exec(
            ExecMode::Direct,
            "pacman-key",
            vec![
                String::from("--init"),
            ],
            OnFail::Error,
        ),
        "Initialize keys",
    );
    exec_eval(
        exec(
            ExecMode::Direct,
            "pacman-key",
            vec![
                String::from("--populate"),
            ],
            OnFail::Error,
        ),
        "Populate keys",
    );
}

pub fn genfstab() {
    exec_eval(
        exec(
            ExecMode::Direct,
            "bash",
            vec![
                String::from("-c"),
                String::from("genfstab -U /mnt >> /mnt/etc/fstab"),
            ],
            OnFail::Error,
        ),
        "Generate fstab",
    );
}

pub fn install_nix_config(device: &str) {
    info!("Set nix channels.");
    // As channel we use nixos-unstable instead of nixpkgs-unstable because 'nixos-' has additional tests that ensure kernel and bootloaders actually work. And some other critical packages.
    exec_eval(
        exec(
            ExecMode::Direct,
            "nix-channel",
            vec![
                String::from("--add"),
                String::from("https://nixos.org/channels/nixos-unstable"),
                String::from("nixpkgs"),
            ],
            OnFail::Error,
        ),
        "Set nixpkgs nix channel on the host",
    );
    // This update is done on the host, not on the target system
    exec_eval(
        exec(
            ExecMode::Direct,
            "nix-channel",
            vec![
                String::from("--update"),
            ],
            OnFail::Error,
        ),
        "Update nix channels on the host",
    );
    std::fs::create_dir_all("/mnt/etc/nixos").unwrap();
    info!("Generate hardware configuration.");
    // nix-shell seems to work as non-sudo only by using --run; --command works only as sudo
    exec_eval(
        exec(
            ExecMode::Direct,
            "nix-shell",
            vec![
                String::from("-p"),
                String::from("nixos-install-tools"),
                String::from("--command"),
                String::from("nixos-generate-config --root /mnt"),
            ],
            OnFail::Error,
        ),
        "Run nixos-generate-config",
    );
    info!("Download latest Athena OS configuration.");
    exec_eval(
        exec(
            ExecMode::Direct,
            "curl",
            vec![
                String::from("-o"),
                String::from("/tmp/athena-nix.zip"),
                String::from("https://codeload.github.com/Athena-OS/athena-nix/zip/refs/heads/main"),
            ],
            OnFail::Error,
        ),
        "Getting latest Athena OS configuration.",
    );
    exec_eval(
        exec(
            ExecMode::Direct,
            "unzip",
            vec![
                String::from("/tmp/athena-nix.zip"),
                String::from("-d"),
                String::from("/tmp/"),
            ],
            OnFail::Error,
        ),
        "Extract Athena OS configuration archive.",
    );
    info!("Install Athena OS configuration.");
    exec_eval(
        exec(
            ExecMode::Direct,
            "cp",
            vec![
                String::from("-rf"),
                String::from("/tmp/athena-nix-main/nixos/home-manager"),
                String::from("/tmp/athena-nix-main/nixos/hosts"),
                String::from("/tmp/athena-nix-main/nixos/modules"),
                String::from("/tmp/athena-nix-main/nixos/pkgs"),
                String::from("/tmp/athena-nix-main/nixos/configuration.nix"),
                String::from("/tmp/athena-nix-main/nixos/default.nix"),
                String::from("/mnt/etc/nixos/"),
            ],
            OnFail::Error,
        ),
        "Move Athena OS configuration to /mnt/etc/nixos/.",
    );
    files_eval(
        files::sed_file(
            "/mnt/etc/nixos/configuration.nix",
            "/etc/nixos/hardware-configuration.nix",
            "./hardware-configuration.nix",
        ),
        "Set hardware-configuration path",
    );
    files_eval(
        files::sed_file(
            "/mnt/etc/nixos/modules/boot/grub/default.nix",
            "/dev/sda",
            device,
        ),
        "Set bootloader device",
    ); 
    hardware::cpu_check();
    hardware::virt_check();
}

fn detect_root_fs_info() -> (String, String) {
    // Ask findmnt for both filesystem type and UUID of /mnt in one go.
    let output = exec_eval_result(
        exec_output(
            "findmnt",
            vec![
                "-n".into(),
                "-o".into(),
                "FSTYPE,UUID".into(),
                "/mnt".into(),
            ],
        ),
        "Detect filesystem type and UUID for /mnt",
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parts: Vec<&str> = stdout.split_whitespace().collect();

    let fstype = parts.first().unwrap_or(&"").to_string();
    let uuid = parts.get(1).unwrap_or(&"").to_string();

    if fstype.is_empty() {
        error!("Failed to detect filesystem type for /mnt");
    }

    if uuid.is_empty() {
        error!("Failed to detect filesystem UUID for /mnt");
    }

    (fstype, uuid)
}

fn secure_boot_supported() -> bool {
    // If we’re not on a UEFI system (or efivarfs isn’t exposed), SB is effectively “unsupported”.
    if !Path::new("/sys/firmware/efi").exists() {
        info!("No /sys/firmware/efi; treating Secure Boot as unsupported.");
        return false;
    }

    let vm = match std::panic::catch_unwind(system) {
        Ok(vm) => vm,
        Err(_) => {
            info!("efivar::system() panicked; treating Secure Boot as unsupported.");
            return false;
        }
    };

    // "SecureBoot" under the standard EFI global vendor GUID
    let var = efi::Variable::new("SecureBoot");

    match vm.exists(&var) {
        Ok(true) => {
            // SecureBoot variable exists → firmware implements Secure Boot
            info!("SecureBoot supported.");
            true
        }
        Ok(false) => {
            // Variable simply not there → very strong sign SB is not implemented
            info!("SecureBoot EFI variable not found; treating Secure Boot as unsupported.");
            false
        }
        Err(e) => {
            // Any error talking to efivarfs / firmware → fail closed
            info!("Error querying SecureBoot EFI variable ({e}); treating Secure Boot as unsupported.");
            false
        }
    }
}

fn sys_vendor() -> Option<String> {
    match fs::read_to_string("/sys/class/dmi/id/sys_vendor") {
        Ok(vendor) => {
            let v = vendor.trim().to_string();
            if v.is_empty() {
                None
            } else {
                Some(v)
            }
        }
        Err(_) => None,
    }
}

pub fn enable_system_services() {
    enable_service("ananicy");
    enable_service("apparmor");
    enable_service("auditd");
    enable_service("bluetooth");
    enable_service("cronie");
    enable_service("docker");
    enable_service("irqbalance");
    enable_service("NetworkManager");
    enable_service("podman");
    enable_service("scx_loader");
    enable_service("systemd-timesyncd");
    enable_service("vnstat");
}
