use crate::internal::hardware;
use crate::internal::install::install;
use crate::internal::kernels::{KERNELS, default_kernel};
use crate::internal::packages;
use crate::internal::services::enable_services;
use log::{error, info};
use shared::args::{ExecMode, ExtendIntoString, OnFail, PackageManager};
use shared::encrypt::{find_target_root_luks, tpm2_available};
use shared::exec::{exec, exec_output};
use shared::files;
use shared::returncode_eval::{exec_eval, files_eval};
use std::{fs, path::Path, sync::OnceLock, thread};

const CHROOT: ExecMode<'static> = ExecMode::Chroot { root: "/mnt" };
const MKINITCPIO_CONF: &str = "/mnt/etc/mkinitcpio.conf";
const SECUREBOOT_KEY_DIR: &str = "/etc/secureboot/keys";

pub fn install_packages(mut packages: Vec<String>) -> i32 {
  let arch_base_pkg: Vec<String> = packages::to_strings(&[
    // Base Arch
    "pacman",
    "mkinitcpio",
    "glibc-locales", // Prebuilt locales to prevent locale warnings during pacstrap
    // Repositories
    "athena-mirrorlist",
    "blackarch-mirrorlist",
    "chaotic-mirrorlist",
    "rate-mirrors",
    "archlinux-keyring",
    "athena-keyring",
    "blackarch-keyring",
    "chaotic-keyring",
  ]);

  packages.extend(
    KERNELS
      .iter()
      .flat_map(|k| [k.pkg.to_string(), k.headers()]),
  );

  if tpm2_available() {
    packages.push("tpm2-tools".into());
  }

  let (fstype, _fs_uuid) = root_fs_info();
  if fstype == "btrfs" {
    packages.push("btrfs-progs".into());
  }
  info!("Root partition is {fstype}");

  let (virt_packages, virt_services, virt_modules) = hardware::virt_check();
  packages.extend_into(virt_packages);
  packages.extend_into(hardware::cpu_check());
  packages.extend_into(hardware::gpu_check());

  // Keyrings must be initialised before installing the base package group,
  // otherwise pacstrap fails with keyring errors.
  init_keyrings_mirrors();
  // Must be done before installing any Athena and Chaotic AUR package.
  files::copy_file("/etc/pacman.conf", "/mnt/etc/pacman.conf");

  // Installing mkinitcpio first lets us edit its config before the kernel
  // packages build their initramfs images.
  let code = install(PackageManager::Pacstrap, &arch_base_pkg);
  if code != 0 {
    error!("mkinitcpio installation failed with exit code {code}");
    return code;
  }

  // Must run after the pacman-mirrorlist package (in base) is installed.
  for list in [
    "mirrorlist",
    "athena-mirrorlist",
    "blackarch-mirrorlist",
    "chaotic-mirrorlist",
  ] {
    files::copy_file(
      &format!("/etc/pacman.d/{list}"),
      &format!("/mnt/etc/pacman.d/{list}"),
    );
  }

  hardware::set_cores();
  files_eval(
    files::sed_file(
      MKINITCPIO_CONF,
      "(?m)^HOOKS=.*",
      "HOOKS=(base systemd autodetect modconf kms keyboard sd-vconsole block sd-encrypt lvm2 filesystems fsck)",
    ),
    "Set mkinitcpio hooks.",
  );
  files_eval(
    // systemd-stub (and therefore UKI) expects an initrd compressed with gzip
    files::sed_file(
      MKINITCPIO_CONF,
      "#COMPRESSION=\"lz4\"",
      "COMPRESSION=\"gzip\"",
    ),
    "Set compression algorithm.",
  );

  match sys_vendor() {
    Some(vendor) if vendor.to_lowercase().contains("apple") => {
      info!("Detected Apple hardware (sys_vendor='{vendor}'). Setting Mac-specific MODULES.");
      files_eval(
        hardware::add_mkinitcpio_modules(hardware::APPLE_MODULES),
        "Set mkinitcpio MODULES for Apple computer.",
      );
    }
    Some(_) => {}
    None => info!("Could not read /sys/class/dmi/id/sys_vendor. Leaving MODULES as default."),
  }
  for modules in virt_modules {
    files_eval(
      hardware::add_mkinitcpio_modules(modules),
      "Set mkinitcpio MODULES for hypervisor",
    );
  }

  // The kernel packages' hooks (athena-uki) build UKIs with ukify, which
  // reads /etc/kernel/cmdline. Everything the cmdline needs is known now, so
  // write it before the kernels are installed instead of failing the hook.
  write_kernel_cmdline_file(&generate_kernel_cmdline());

  let code = install(PackageManager::Pacman, &packages);
  if code != 0 {
    error!("Package installation failed with exit code {code}");
    return code;
  }
  files::copy_file(
    "/mnt/usr/local/share/athena/release/os-release-athena",
    "/mnt/usr/lib/os-release",
  );
  files::copy_file("/etc/skel/.bashrc", "/mnt/etc/skel/.bashrc");

  enable_services(virt_services);

  files_eval(
    files::sed_file(
      "/mnt/etc/nsswitch.conf",
      "hosts:.*",
      // `myhostname` before `dns`: the machine's own name never goes to a DNS
      // server (which can take 5 s per timeout when the upstream drops
      // `<host>.localdomain`). `mdns_minimal` only answers `.local` names and
      // is skipped by glibc when nss-mdns is not installed.
      "hosts: mymachines resolve [!UNAVAIL=return] files myhostname mdns_minimal [NOTFOUND=return] dns wins",
    ),
    "Set nsswitch configuration",
  );
  code
}

fn generate_kernel_cmdline() -> String {
  let (fstype, fs_uuid) = root_fs_info();
  let tpm = tpm2_available();
  let mut cmdline = String::new();

  if let Some(root) = find_target_root_luks() {
    // Encrypted root: use the *live* mapper name so rd.luks.name matches reality.
    cmdline.push_str(&format!(
      "rd.luks.name={}={} root=/dev/mapper/{} ",
      root.luks_uuid, root.mapper_dev, root.mapper_dev
    ));
    if tpm {
      cmdline.push_str("rd.luks.options=tpm2-device=auto ");
    }
  } else if !fs_uuid.is_empty() {
    cmdline.push_str(&format!("root=UUID={fs_uuid} "));
  } else {
    error!("Could not determine root UUID for unencrypted root");
  }

  if fstype == "btrfs" {
    cmdline.push_str("rootflags=subvol=@ ");
  }
  if tpm {
    cmdline.push_str("systemd-measure=yes ");
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
    // 1920x1080 is the maximum: https://wiki.archlinux.org/title/Hyper-V#Setting_resolution
    params.push("video=hyperv_fb:1920x1080");
  }

  cmdline.push_str(&params.join(" "));
  cmdline
}

/// Write cmdline to /etc/kernel/cmdline inside the target system, so future
/// kernel/UKI regeneration tools know what to embed.
fn write_kernel_cmdline_file(cmdline: &str) {
  files_eval(
    files::create_directory("/mnt/etc/kernel"),
    "Create /mnt/etc/kernel",
  );
  files_eval(
    files::write_file("/mnt/etc/kernel/cmdline", &format!("{cmdline}\n")),
    "Write /etc/kernel/cmdline",
  );
}

/// TPM2 PCR signing key pairs embedded in the UKIs, paths inside the chroot.
///
/// Two policies, following the layout recommended by systemd's ukify(1):
///  * `initrd`: valid only while in the initrd (`enter-initrd`). This is the
///    key to enrol for LUKS auto-unlock (`systemd-cryptenroll --tpm2-public-key
///    … --tpm2-public-key-pcrs=11`): the volume key can be released only in
///    the initrd phase, never once the full system is running.
///  * `system`: valid for all later phases (after switch-root). Use it for
///    secrets that should stay available to the running system.
struct PcrKeyPair {
  name: &'static str,
  private: &'static str,
  public: &'static str,
  /// Comma-separated list of colon-separated phase paths (ukify syntax).
  phases: &'static str,
}

const PCR_KEYS: &[PcrKeyPair] = &[
  PcrKeyPair {
    name: "initrd",
    private: "/etc/systemd/tpm2-pcr-initrd-private-key.pem",
    public: "/etc/systemd/tpm2-pcr-initrd-public-key.pem",
    phases: "enter-initrd",
  },
  PcrKeyPair {
    name: "system",
    private: "/etc/systemd/tpm2-pcr-private-key-system.pem",
    public: "/etc/systemd/tpm2-pcr-public-key-system.pem",
    phases: "enter-initrd:leave-initrd,enter-initrd:leave-initrd:sysinit,enter-initrd:leave-initrd:sysinit:ready",
  },
];

fn ensure_pcr_keys_in_chroot() {
  if !tpm2_available() {
    info!("TPM2 not available; skipping PCR key generation.");
    return;
  }
  fs::create_dir_all("/mnt/etc/systemd").expect("Failed to create /mnt/etc/systemd");

  for key in PCR_KEYS {
    let present = Path::new(&format!("/mnt{}", key.private)).exists()
      && Path::new(&format!("/mnt{}", key.public)).exists();
    if present {
      info!(
        "PCR {} keys already present; skipping ukify genkey.",
        key.name
      );
    } else {
      // ukify genkey requires that the outputs do not exist yet.
      exec_eval(
        exec(
          CHROOT,
          "ukify",
          [
            "genkey",
            "--pcr-private-key",
            key.private,
            "--pcr-public-key",
            key.public,
          ],
          OnFail::Error,
        ),
        &format!("Generate {} PCR signing keypair", key.name),
      );
    }
    let path = format!("/mnt{}", key.private);
    exec_eval(
      fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o400)),
      &format!("Restrict {} PCR private key", key.name),
    );
  }
}

/// Everything that is the same for every kernel's UKI; computed once.
struct UkiContext<'a> {
  /// ESP mount path inside the chroot, e.g. "/efi".
  esp: &'a str,
  cmdline: &'a str,
  ucode_initrd: Option<&'static str>,
  /// Secure Boot key dir inside the chroot, when signing is enabled.
  secureboot_key_dir: Option<&'a str>,
  measure_pcr: bool,
}

/// Build (and sign) one Unified Kernel Image per entry of [`KERNELS`], write
/// the systemd-boot loader entries and loader.conf.
fn build_and_sign_ukis(ctx: &UkiContext) {
  let athena_dir = format!("/mnt{}/EFI/Athena", ctx.esp);
  let entries_dir = format!("/mnt{}/loader/entries", ctx.esp);
  fs::create_dir_all(&athena_dir).expect("Failed to create ESP/EFI/Athena");
  fs::create_dir_all(&entries_dir).expect("Failed to create loader/entries");

  for k in KERNELS {
    let mut args: Vec<String> = vec![
      "build".into(),
      "--linux".into(),
      format!("/boot/vmlinuz-{}", k.pkg),
    ];
    // microcode first
    if let Some(ucode) = ctx.ucode_initrd {
      args.extend(["--initrd".into(), ucode.into()]);
    }
    args.extend([
      "--initrd".into(),
      format!("/boot/initramfs-{}.img", k.pkg),
      "--cmdline".into(),
      ctx.cmdline.into(),
      "--os-release".into(),
      "/usr/lib/os-release".into(),
      "--uname".into(),
      k.pkg.into(),
    ]);
    if let Some(dir) = ctx.secureboot_key_dir {
      args.extend([
        "--signtool=sbsign".into(),
        "--secureboot-private-key".into(),
        format!("{dir}/MOK.key"),
        "--secureboot-certificate".into(),
        format!("{dir}/MOK.crt"),
      ]);
    }
    if ctx.measure_pcr {
      args.push("--measure".into());
      for key in PCR_KEYS {
        args.extend([
          "--pcr-private-key".into(),
          key.private.into(),
          "--pcr-public-key".into(),
          key.public.into(),
          "--phases".into(),
          key.phases.into(),
        ]);
      }
    }
    let uki_out = format!("{}/EFI/Athena/{}", ctx.esp, k.uki_name());
    args.extend(["--output".into(), uki_out.clone()]);

    exec_eval(
      exec(CHROOT, "ukify", &args, OnFail::Error),
      &format!("Create+sign UKI for {}", k.pkg),
    );

    let entry_path = format!("{entries_dir}/{}", k.entry_name());
    files_eval(
      files::write_file(
        &entry_path,
        &format!(
          "title   Athena OS ({})\nefi     /EFI/Athena/{}\n",
          k.title,
          k.uki_name()
        ),
      ),
      &format!("Write systemd-boot entry for {}", k.pkg),
    );
    info!(
      "UKI for {} created at {uki_out} and loader entry {entry_path}",
      k.pkg
    );
  }

  files_eval(
    files::write_file(
      &format!("/mnt{}/loader/loader.conf", ctx.esp),
      &format!(
        "default {}\ntimeout 3\nconsole-mode keep\neditor no\n",
        default_kernel().entry_name()
      ),
    ),
    "Write loader.conf",
  );
}

pub fn configure_bootloader_systemd_boot_shim(esp: &str) {
  info!("Configuring systemd-boot + UKI + shim Secure Boot in {esp}");

  let sb_supported = secure_boot_supported();

  ensure_pcr_keys_in_chroot();

  // Same content as written before the package install; kept in sync here in
  // case anything changed in between (e.g. TPM keys).
  let cmdline = generate_kernel_cmdline();
  write_kernel_cmdline_file(&cmdline);

  let boot_is_mount = exec_output("findmnt", ["-n", "-M", "/mnt/boot"]).is_ok();
  let mut bootctl_args: Vec<&str> = vec!["--esp-path", esp];
  if boot_is_mount {
    info!("/boot is a separate partition; passing --boot-path to bootctl");
    bootctl_args.extend(["--boot-path", "/boot"]);
  } else {
    info!("/boot is not a separate partition; installing bootloader without --boot-path");
  }
  bootctl_args.push("install");
  exec_eval(
    exec(CHROOT, "bootctl", bootctl_args, OnFail::Error),
    "Install systemd-boot",
  );

  if sb_supported {
    setup_secure_boot_shim(esp);
  }

  let ctx = UkiContext {
    esp,
    cmdline: &cmdline,
    ucode_initrd: hardware::cpu_vendor().ucode_initrd(),
    secureboot_key_dir: sb_supported.then_some(SECUREBOOT_KEY_DIR),
    measure_pcr: tpm2_available(),
  };
  build_and_sign_ukis(&ctx);
}

/// Generate the Athena MOK keypair, sign systemd-boot with it and install shim
/// as stage 0 so Secure Boot works out of the box: firmware runs shim
/// (Microsoft-signed), shim loads our signed systemd-boot under the name
/// `grubx64.efi`, and MOK Manager asks the user to enrol the Athena cert on
/// first boot.
fn setup_secure_boot_shim(esp: &str) {
  let dir = SECUREBOOT_KEY_DIR;
  let key = format!("{dir}/MOK.key");
  let crt = format!("{dir}/MOK.crt");
  let cer = format!("{dir}/MOK.cer");
  fs::create_dir_all(format!("/mnt{dir}")).expect("Failed to create secureboot key dir");

  exec_eval(
    exec(
      CHROOT,
      "openssl",
      [
        "req",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-keyout",
        &key,
        "-new",
        "-x509",
        "-sha256",
        "-days",
        "3650",
        "-subj",
        "/CN=Athena OS Secure Boot Key/",
        "-out",
        &crt,
      ],
      OnFail::Error,
    ),
    "Generate Athena Secure Boot keypair",
  );
  exec_eval(
    fs::set_permissions(
      format!("/mnt{key}"),
      std::os::unix::fs::PermissionsExt::from_mode(0o400),
    ),
    "Restrict Secure Boot private key permissions",
  );
  exec_eval(
    exec(
      CHROOT,
      "openssl",
      ["x509", "-outform", "DER", "-in", &crt, "-out", &cer],
      OnFail::Error,
    ),
    "Generate DER (.cer) version of Athena Secure Boot cert",
  );

  let sdboot = format!("{esp}/EFI/systemd/systemd-bootx64.efi");
  exec_eval(
    exec(
      CHROOT,
      "sbsign",
      ["--key", &key, "--cert", &crt, "--output", &sdboot, &sdboot],
      OnFail::Error,
    ),
    "Sign systemd-boot with Athena key",
  );

  //   ESP/EFI/BOOT/BOOTX64.EFI  <- shimx64.efi (MS-signed, from shim-signed pkg)
  //   ESP/EFI/BOOT/grubx64.efi  <- signed systemd-bootx64.efi
  //   ESP/EFI/BOOT/mmx64.efi    <- MokManager so shim can start enrolment UI
  fs::create_dir_all(format!("/mnt{esp}/EFI/BOOT"))
    .expect("Failed to create ESP/EFI/BOOT directory");
  files::copy_file(
    "/mnt/usr/share/shim-signed/shimx64.efi",
    &format!("/mnt{esp}/EFI/BOOT/BOOTX64.EFI"),
  );
  files::copy_file(
    "/mnt/usr/share/shim-signed/mmx64.efi",
    &format!("/mnt{esp}/EFI/BOOT/mmx64.efi"),
  );
  files::copy_file(
    &format!("/mnt{sdboot}"),
    &format!("/mnt{esp}/EFI/BOOT/grubx64.efi"),
  );

  // Public cert somewhere obvious on the ESP, and pre-registered with
  // mokutil so first boot asks "Enroll this key?" without touching firmware setup.
  fs::create_dir_all(format!("/mnt{esp}/EFI/Athena"))
    .expect("Failed to create ESP/EFI/Athena directory");
  files::copy_file(
    &format!("/mnt{cer}"),
    &format!("/mnt{esp}/EFI/Athena/AthenaSecureBoot.cer"),
  );
  exec_eval(
    exec(CHROOT, "mokutil", ["--import", &cer, "-P"], OnFail::Error),
    "Schedule AthenaSecureBoot.cer enrollment in MOK Manager at first boot",
  );

  info!(
    "systemd-boot + UKI + shim configured. On first boot, MOK Manager will ask to enroll AthenaSecureBoot.cer."
  );
}

/// Rank mirrors for the three repositories in parallel (independent output
/// files on the live system; they are copied into the target afterwards), then
/// rebuild the host keyring.
fn init_keyrings_mirrors() {
  info!("Getting fastest mirrors for your location");
  let jobs: [(&str, &str); 3] = [
    ("arch", "/etc/pacman.d/mirrorlist"),
    ("blackarch", "/etc/pacman.d/blackarch-mirrorlist"),
    ("chaotic-aur", "/etc/pacman.d/chaotic-mirrorlist"),
  ];
  thread::scope(|s| {
    for (repo, out) in jobs {
      s.spawn(move || {
        exec_eval(
          exec(
            ExecMode::Direct,
            "rate-mirrors",
            [
              "--concurrency",
              "40",
              "--disable-comments",
              "--allow-root",
              "--save",
              out,
              repo,
            ],
            OnFail::Continue,
          ),
          &format!("Set fastest {repo} mirrors on the host"),
        );
      });
    }
  });

  info!("Upgrade keyrings on the host");
  if Path::new("/etc/pacman.d/gnupg").exists() {
    exec_eval(fs::remove_dir_all("/etc/pacman.d/gnupg"), "Removing keys");
  }
  exec_eval(
    exec(ExecMode::Direct, "pacman-key", ["--init"], OnFail::Error),
    "Initialize keys",
  );
  exec_eval(
    exec(
      ExecMode::Direct,
      "pacman-key",
      ["--populate"],
      OnFail::Error,
    ),
    "Populate keys",
  );
}

pub fn genfstab() {
  let out = exec_eval(exec_output("genfstab", ["-U", "/mnt"]), "Generate fstab");
  files_eval(
    files::append_file(
      "/mnt/etc/fstab",
      String::from_utf8_lossy(&out.stdout).trim_end(),
    ),
    "Write /mnt/etc/fstab",
  );
}

pub fn install_nix_config(device: &str) {
  info!("Set nix channels.");
  // nixos-unstable (not nixpkgs-unstable) has the extra tests that ensure
  // kernel and bootloaders actually work.
  exec_eval(
    exec(
      ExecMode::Direct,
      "nix-channel",
      [
        "--add",
        "https://nixos.org/channels/nixos-unstable",
        "nixpkgs",
      ],
      OnFail::Error,
    ),
    "Set nixpkgs nix channel on the host",
  );
  exec_eval(
    exec(ExecMode::Direct, "nix-channel", ["--update"], OnFail::Error),
    "Update nix channels on the host",
  );
  fs::create_dir_all("/mnt/etc/nixos").expect("Failed to create /mnt/etc/nixos");
  info!("Generate hardware configuration.");
  exec_eval(
    exec(
      ExecMode::Direct,
      "nix-shell",
      [
        "-p",
        "nixos-install-tools",
        "--command",
        "nixos-generate-config --root /mnt",
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
      [
        "-fsSL",
        "-o",
        "/tmp/athena-nix.zip",
        "https://codeload.github.com/Athena-OS/athena-nix/zip/refs/heads/main",
      ],
      OnFail::Error,
    ),
    "Getting latest Athena OS configuration.",
  );
  exec_eval(
    exec(
      ExecMode::Direct,
      "unzip",
      ["-o", "/tmp/athena-nix.zip", "-d", "/tmp/"],
      OnFail::Error,
    ),
    "Extract Athena OS configuration archive.",
  );
  info!("Install Athena OS configuration.");
  exec_eval(
    exec(
      ExecMode::Direct,
      "cp",
      [
        "-rf",
        "/tmp/athena-nix-main/nixos/home-manager",
        "/tmp/athena-nix-main/nixos/hosts",
        "/tmp/athena-nix-main/nixos/modules",
        "/tmp/athena-nix-main/nixos/pkgs",
        "/tmp/athena-nix-main/nixos/configuration.nix",
        "/tmp/athena-nix-main/nixos/default.nix",
        "/mnt/etc/nixos/",
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
  hardware::gpu_check_nix();
  hardware::virt_check();
}

/// (fstype, UUID) of the filesystem mounted at /mnt. Cached: both callers run
/// after the root has been mounted and before it is unmounted.
fn root_fs_info() -> (String, String) {
  static INFO: OnceLock<(String, String)> = OnceLock::new();
  INFO
    .get_or_init(|| {
      let output = exec_eval(
        exec_output("findmnt", ["-n", "-o", "FSTYPE,UUID", "/mnt"]),
        "Detect filesystem type and UUID for /mnt",
      );
      let stdout = String::from_utf8_lossy(&output.stdout);
      let mut parts = stdout.split_whitespace();
      let fstype = parts.next().unwrap_or("").to_string();
      let uuid = parts.next().unwrap_or("").to_string();
      if fstype.is_empty() {
        error!("Failed to detect filesystem type for /mnt");
      }
      if uuid.is_empty() {
        error!("Failed to detect filesystem UUID for /mnt");
      }
      (fstype, uuid)
    })
    .clone()
}

/// Whether the firmware implements Secure Boot, judged by the presence of the
/// `SecureBoot` EFI variable under the global vendor GUID. No efivar crate
/// needed: efivarfs exposes it as a plain file.
fn secure_boot_supported() -> bool {
  if !Path::new("/sys/firmware/efi").exists() {
    info!("No /sys/firmware/efi; treating Secure Boot as unsupported.");
    return false;
  }
  let var = Path::new("/sys/firmware/efi/efivars/SecureBoot-8be4df61-93ca-11d2-aa0d-00e098032b8c");
  match fs::read(var) {
    // 4 bytes of attributes followed by the value byte.
    Ok(data) => {
      let enabled = data.get(4).copied().unwrap_or(0) == 1;
      info!(
        "SecureBoot supported (currently {}).",
        if enabled { "enabled" } else { "disabled" }
      );
      true
    }
    Err(e) => {
      info!("SecureBoot EFI variable not readable ({e}); treating Secure Boot as unsupported.");
      false
    }
  }
}

fn sys_vendor() -> Option<String> {
  fs::read_to_string("/sys/class/dmi/id/sys_vendor")
    .ok()
    .map(|v| v.trim().to_string())
    .filter(|v| !v.is_empty())
}

pub fn enable_system_services() {
  enable_services([
    "ananicy",
    "apparmor",
    "auditd",
    "bluetooth",
    "cronie",
    "docker",
    "irqbalance",
    "NetworkManager",
    "podman",
    "scx_loader",
    "systemd-timesyncd",
    "vnstat",
  ]);
}
