use crate::args::{self, ExecMode, MountSpec, OnFail};
use crate::exec::{exec, exec_output, exec_workdir};
use crate::returncode_eval::exec_eval;
use crate::strings::crash;
use log::{debug, info};
use std::collections::HashSet;
use std::fs::{self, create_dir_all};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const MNT: &str = "/mnt";

/// Trailing digits of a block device path, e.g. "/dev/nvme0n1p3" -> "3".
fn extract_partition_number(bdevice: &str) -> &str {
  bdevice
    .rfind(|c: char| !c.is_ascii_digit())
    .map(|pos| &bdevice[pos + 1..])
    .unwrap_or("")
}

/// Mapper name used for an encrypted partition, e.g. "/dev/sda3" -> "sda3crypted".
pub fn crypt_mapper_name(blockdevice: &str) -> String {
  format!("{}crypted", blockdevice.trim_start_matches("/dev/"))
}

fn encrypt_blockdevice(blockdevice: &str, cryptlabel: &str, keyfile: &str) {
  if !Path::new(keyfile).is_file() {
    crash(
      format!("LUKS key file {keyfile} for {blockdevice} does not exist"),
      1,
    );
  }
  exec_eval(
    exec(
      ExecMode::Direct,
      "cryptsetup",
      ["luksFormat", "-q", blockdevice, "-d", keyfile],
      OnFail::Error,
    ),
    "Format LUKS partition",
  );
  exec_eval(
    exec(
      ExecMode::Direct,
      "cryptsetup",
      ["luksOpen", blockdevice, cryptlabel, "-d", keyfile],
      OnFail::Error,
    ),
    "Open LUKS format",
  );
  // Key material must not outlive the step that needs it.
  if let Err(e) = fs::remove_file(keyfile) {
    crash(format!("Failed to remove LUKS key file {keyfile}: {e}"), 1);
  }
  info!("Removed LUKS key file {keyfile}");
}

pub fn partition(device: PathBuf, table_type: &str, mode: &str, partitions: &[args::Partition]) {
  let is_none_label = table_type.eq_ignore_ascii_case("none");
  let is_msdos_label = table_type.eq_ignore_ascii_case("msdos");
  let is_auto_mode = mode.eq_ignore_ascii_case("auto");

  if !device.exists() {
    crash(format!("The device {device:?} doesn't exist"), 1);
  }
  debug!("Partitioning process");
  let dev = device.to_string_lossy();

  // No partition table, or msdos in auto mode: create a fresh label.
  if is_none_label || (is_msdos_label && is_auto_mode) {
    let (label, what) = if is_uefi() {
      ("gpt", "GPT")
    } else {
      ("msdos", "MSDOS (MBR)")
    };
    exec_eval(
      exec(
        ExecMode::Direct,
        "parted",
        ["-s", &dev, "--", "mklabel", label],
        OnFail::Error,
      ),
      &format!("Create a {what} partition table on {dev}"),
    );
  }

  // --- Phase A: deletes first ---
  for p in partitions.iter().filter(|p| p.action == "delete") {
    info!("Deleting {}", p.blockdevice);
    delete_partition(
      &device,
      &p.blockdevice,
      p.mountpoint.as_deref().unwrap_or(""),
    );
  }

  // --- Phase B: creates / modifies ---
  let mut plan: Vec<MountSpec> = Vec::new();
  for p in partitions {
    if p.action == "create" {
      info!("Creating {}", p.blockdevice);
      create_partition(
        &device,
        &p.blockdevice,
        &p.start,
        &p.end,
        p.filesystem.as_deref().unwrap_or(""),
        &p.flags,
        table_type,
      );
    }
    if p.action == "create" || p.action == "modify" {
      plan.extend(fmt_mount(p));
    }
  }

  mount_queue(plan);
}

/// mkfs command and flags per filesystem name accepted in the config.
fn mkfs_cmd(filesystem: &str) -> Option<(&'static str, &'static [&'static str])> {
  Some(match filesystem {
    "vfat" | "fat32" => ("mkfs.vfat", &["-F32"]),
    "fat" => ("mkfs.fat", &[]),
    "msdos" => ("mkfs.msdos", &[]),
    "bfs" => ("mkfs.bfs", &[]),
    "cramfs" => ("mkfs.cramfs", &[]),
    "minix" => ("mkfs.minix", &[]),
    "ext2" => ("mkfs.ext2", &["-F"]),
    "ext3" => ("mkfs.ext3", &["-F"]),
    "ext4" => ("mkfs.ext4", &["-F"]),
    "xfs" => ("mkfs.xfs", &["-f"]),
    "f2fs" => ("mkfs.f2fs", &["-f"]),
    "ntfs" => ("mkfs.ntfs", &["-F"]),
    _ => return None,
  })
}

fn fmt_mount(p: &args::Partition) -> Vec<MountSpec> {
  let mountpoint = p.mountpoint.as_deref().unwrap_or("");
  let filesystem = p.filesystem.as_deref().unwrap_or("");
  let mut plan = Vec::new();
  let mut bdevice = p.blockdevice.clone();

  if p.has_flag("encrypt") {
    if filesystem == "don't format" {
      // luksFormat would wipe a partition the user asked us to keep.
      crash(
        format!(
          "Partition {bdevice} is flagged 'encrypt' but also 'don't format'; refusing to overwrite it"
        ),
        1,
      );
    }
    let keyfile = p.luks_keyfile.clone().unwrap_or_else(|| {
      crash(
        format!("Partition {bdevice} is flagged 'encrypt' but has no luks_keyfile"),
        1,
      )
    });
    let cryptlabel = crypt_mapper_name(&bdevice);
    encrypt_blockdevice(&bdevice, &cryptlabel, &keyfile);
    bdevice = format!("/dev/mapper/{cryptlabel}");
  }

  match filesystem {
    "btrfs" => {
      exec_eval(
        exec(
          ExecMode::Direct,
          "mkfs.btrfs",
          ["-f", &bdevice],
          OnFail::Error,
        ),
        &format!("Formatting {bdevice} as btrfs"),
      );

      // Create subvolumes in a temporary, device-specific staging mount (not /mnt).
      let stage = format!(
        "{MNT}/.staging-btrfs-{}",
        bdevice.trim_start_matches("/dev/").replace('/', "_")
      );
      exec_eval(
        create_dir_all(&stage),
        &format!("Create staging dir {stage}"),
      );
      mount(&bdevice, &stage, "", "btrfs");
      for sub in ["@", "@home"] {
        exec_eval(
          exec_workdir("btrfs", &stage, ["subvolume", "create", sub]),
          &format!("Create btrfs subvolume {sub}"),
        );
      }
      umount(&stage);
      let _ = fs::remove_dir(&stage);

      // Final btrfs mounts are queued (mounted later in correct order).
      if mountpoint == "/" {
        for (mp, opts) in [("/", "subvol=@"), ("/home", "subvol=@home")] {
          plan.push(MountSpec {
            device: bdevice.clone(),
            mountpoint: mp.into(),
            options: opts.into(),
            fstype: "btrfs".into(),
            is_swap: false,
          });
        }
      } else if !mountpoint.is_empty() {
        plan.push(MountSpec {
          device: bdevice,
          mountpoint: mountpoint.to_string(),
          options: String::new(),
          fstype: "btrfs".into(),
          is_swap: false,
        });
      }
      return plan;
    }
    "linux-swap" | "swap" => {
      exec_eval(
        exec(
          ExecMode::Direct,
          "mkswap",
          ["-L", "swap", &bdevice],
          OnFail::Error,
        ),
        &format!("Formatting {bdevice} as linux-swap"),
      );
      plan.push(MountSpec {
        device: bdevice,
        mountpoint: String::new(),
        options: String::new(),
        fstype: String::new(),
        is_swap: true,
      });
      return plan;
    }
    "don't format" => info!("Not formatting {bdevice}"),
    other => match mkfs_cmd(other) {
      Some((cmd, flags)) => exec_eval(
        exec(
          ExecMode::Direct,
          cmd,
          flags.iter().copied().chain([bdevice.as_str()]),
          OnFail::Error,
        ),
        &format!("Formatting {bdevice} as {other}"),
      ),
      None => crash(
        format!("Unknown filesystem {other}, used in partition {bdevice}"),
        1,
      ),
    },
  }

  if !mountpoint.is_empty() {
    plan.push(MountSpec {
      device: bdevice,
      mountpoint: mountpoint.to_string(),
      options: String::new(),
      fstype: fs_to_mount_type(filesystem).to_string(),
      is_swap: false,
    });
  }

  plan
}

/// Map a requested filesystem to the type string the kernel's `mount -t`
/// expects. Returns "" when we can't/shouldn't specify it (e.g. "don't format",
/// where the on-disk fs is pre-existing and safe to auto-detect).
fn fs_to_mount_type(filesystem: &str) -> &'static str {
  match filesystem {
    "vfat" | "fat32" | "fat" | "msdos" => "vfat",
    "ext2" => "ext2",
    "ext3" => "ext3",
    "ext4" => "ext4",
    "btrfs" => "btrfs",
    "xfs" => "xfs",
    "f2fs" => "f2fs",
    "ntfs" => "ntfs3",
    "minix" => "minix",
    "bfs" => "bfs",
    "cramfs" => "cramfs",
    _ => "",
  }
}

pub fn mount(partition: &str, mountpoint: &str, options: &str, fstype: &str) {
  // Always pass an explicit `-t <fstype>` when we know it. Right after mkfs,
  // libblkid auto-detection can return a stale/wrong type (a just-deleted
  // partition's old signature still in cache), which makes a bare `mount`
  // fail with "wrong fs type, bad superblock" even though the fs is valid.
  let mut args = vec![partition, mountpoint];
  if !fstype.is_empty() {
    args.extend(["-t", fstype]);
  }
  if !options.is_empty() {
    args.extend(["-o", options]);
  }
  exec_eval(
    exec(ExecMode::Direct, "mount", args, OnFail::Error),
    &format!(
      "Mount {partition} ({}) at {mountpoint}",
      if fstype.is_empty() { "auto" } else { fstype }
    ),
  );
}

fn is_mounted(mountpoint: &str) -> bool {
  fs::read_to_string("/proc/mounts")
    .map(|m| {
      m.lines()
        .any(|line| line.split_whitespace().nth(1) == Some(mountpoint))
    })
    .unwrap_or(false)
}

pub fn umount(mountpoint: &str) {
  if is_mounted(mountpoint) {
    exec_eval(
      exec(
        ExecMode::Direct,
        "umount",
        ["-R", mountpoint],
        OnFail::Error,
      ),
      &format!("Unmount command processed on {mountpoint}"),
    );
  } else {
    info!("Skipping umount: {mountpoint} is not mounted");
  }
}

/// Whether the live system booted in UEFI mode. Cached; cannot change at runtime.
pub fn is_uefi() -> bool {
  static UEFI: OnceLock<bool> = OnceLock::new();
  *UEFI.get_or_init(|| Path::new("/sys/firmware/efi").is_dir())
}

pub fn partition_info() {
  exec_eval(
    exec(
      ExecMode::Direct,
      "lsblk",
      ["-o", "NAME,SIZE,FSTYPE,UUID,MOUNTPOINT"],
      OnFail::Error,
    ),
    "Show lsblk",
  );
}

fn fs_to_parted_fs(filesystem: &str) -> Option<&'static str> {
  match filesystem {
    "vfat" | "fat32" => Some("fat32"), // parted wants "fat32" (you still mkfs.vfat later)
    "ext4" => Some("ext4"),
    "ext3" => Some("ext3"),
    "ext2" => Some("ext2"),
    "btrfs" => Some("btrfs"),
    "xfs" => Some("xfs"),
    "ntfs" => Some("ntfs"),
    "linux-swap" | "swap" => Some("linux-swap"),
    "don't format" => Some("don't format"),
    _ => None,
  }
}

fn parted(dev: &str, args: &[&str], what: &str) {
  exec_eval(
    exec(
      ExecMode::Direct,
      "parted",
      ["-s", dev, "--"].into_iter().chain(args.iter().copied()),
      OnFail::Error,
    ),
    what,
  );
}

/// Create a partition in sectors and set flags appropriately:
/// - if flags contain "esp": create ESP (mkpart with fat32 + `set esp on`)
/// - else if flags contain "bls_boot": XBOOTLDR partition
/// - else if flags contain "boot" (but NOT "esp"): BIOS/GRUB partition (`set bios_grub on`)
/// - else if filesystem is swap: create Linux swap (mkpart linux-swap)
/// - else: create a regular data/root partition
fn create_partition(
  device: &Path,
  blockdevice: &str, // e.g. "/dev/sda2" (used to extract the number)
  start_sector: &str,
  end_sector: &str,
  filesystem: &str, // e.g. "ext4" | "btrfs" | "vfat" | "swap" | "don't format"
  flags: &[String],
  disklabel: &str,
) {
  info!(
    "Filesystem: {filesystem}, block device: {blockdevice}, sectors {start_sector}-{end_sector}, flags {flags:?}"
  );

  let dev = device.to_string_lossy();
  let partnum = extract_partition_number(blockdevice);
  if partnum.is_empty() {
    crash(
      format!("Cannot derive partition number from {blockdevice}. It must be like /dev/sda3."),
      1,
    );
  }

  let is_none_label = disklabel.eq_ignore_ascii_case("none");
  let is_gpt = disklabel.eq_ignore_ascii_case("gpt") || (is_none_label && is_uefi());
  let is_mbr = disklabel.eq_ignore_ascii_case("msdos") || (is_none_label && !is_uefi());

  let has = |f: &str| flags.iter().any(|x| x.eq_ignore_ascii_case(f));
  let has_esp = has("esp");
  let has_xbld = has("bls_boot");
  let has_boot = has("boot");
  let is_swap =
    filesystem.eq_ignore_ascii_case("swap") || filesystem.eq_ignore_ascii_case("linux-swap");

  // Human label, set after mkpart with `name N <LABEL>`.
  let label = if has_esp {
    "EFI"
  } else if has_xbld {
    "XBOOTLDR"
  } else if has_boot {
    "BIOS_GRUB"
  } else if is_swap {
    "SWAP"
  } else {
    ""
  };

  let mut args: Vec<&str> = vec!["unit", "s", "mkpart"];
  if is_gpt {
    if has_esp {
      args.extend(["ESP", "fat32"]);
    } else if has_xbld {
      args.extend(["XBOOTLDR", "fat32"]);
    } else if is_swap {
      args.extend(["swap", "linux-swap"]); // GUID becomes 8200
    } else if let Some(pfs) = fs_to_parted_fs(filesystem) {
      args.push(pfs); // regular data partition (8300 on GPT)
    }
  } else if is_mbr {
    args.push("primary");
    if is_swap {
      args.push("linux-swap");
    } else if let Some(pfs) = fs_to_parted_fs(filesystem) {
      args.push(pfs);
    }
  }
  args.extend([start_sector, end_sector]);

  parted(
    &dev,
    &args,
    &format!("Create partition {blockdevice} from {start_sector} to {end_sector}"),
  );

  if is_gpt && !label.is_empty() {
    parted(
      &dev,
      &["name", partnum, label],
      &format!("Name partition #{partnum} as {label}"),
    );
  }

  let flag = if is_gpt && has_esp {
    Some(("esp", "Enable ESP"))
  } else if is_gpt && has_xbld {
    Some(("bls_boot", "Enable XBOOTLDR"))
  } else if is_gpt && has_boot {
    Some(("bios_grub", "Enable BIOS_GRUB"))
  } else if is_mbr && has_boot {
    Some(("boot", "Enable boot flag"))
  } else {
    None
  };
  if let Some((f, what)) = flag {
    parted(
      &dev,
      &["set", partnum, f, "on"],
      &format!("{what} on partition #{partnum}"),
    );
  }

  // Inform the kernel of the new table and wait for udev to recreate the /dev
  // nodes BEFORE we mkfs/mount. Without this, a stale partition node (from a
  // previous table) can still map to the old offsets. Best-effort (Continue),
  // since partprobe can return EBUSY and must not abort the install.
  exec_eval(
    exec(
      ExecMode::Direct,
      "partprobe",
      [dev.as_ref()],
      OnFail::Continue,
    ),
    &format!("Inform kernel of new partition on {dev}"),
  );
  exec_eval(
    exec(ExecMode::Direct, "udevadm", ["settle"], OnFail::Continue),
    "Wait for udev to settle",
  );
  exec_eval(
    exec(
      ExecMode::Direct,
      "udevadm",
      ["wait", "--timeout=10", blockdevice],
      OnFail::Continue,
    ),
    &format!("Wait for {blockdevice} to appear"),
  );
}

fn delete_partition(device: &Path, blockdevice: &str, mountpoint: &str) {
  info!("Delete partition: block device {blockdevice}, mount point '{mountpoint}'");

  // Best-effort teardown; each step is non-fatal if the resource is not active.
  exec_eval(
    exec(ExecMode::Direct, "swapoff", [blockdevice], OnFail::Continue),
    &format!("Swapoff {blockdevice} (if active)"),
  );
  if !mountpoint.is_empty() && is_mounted(mountpoint) {
    exec_eval(
      exec(
        ExecMode::Direct,
        "umount",
        ["-R", mountpoint],
        OnFail::Continue,
      ),
      &format!("Umount {mountpoint} (best-effort)"),
    );
  }
  if let Some(name) = blockdevice.strip_prefix("/dev/mapper/") {
    exec_eval(
      exec(
        ExecMode::Direct,
        "cryptsetup",
        ["luksClose", name],
        OnFail::Continue,
      ),
      &format!("Close LUKS mapper for {blockdevice} (if open)"),
    );
  }

  let partnum = extract_partition_number(blockdevice);
  if partnum.is_empty() {
    crash(
      format!(
        "Cannot extract partition number from {blockdevice}. \
                 Provide a real partition path like /dev/sda3 (not a /dev/mapper/* node)."
      ),
      1,
    );
  }

  let dev = device.to_string_lossy();
  parted(
    &dev,
    &["rm", partnum],
    &format!("Delete partition #{partnum} on {dev}"),
  );

  exec_eval(
    exec(
      ExecMode::Direct,
      "partprobe",
      [dev.as_ref()],
      OnFail::Continue,
    ),
    &format!("Inform kernel of partition table changes on {dev}"),
  );
  exec_eval(
    exec(ExecMode::Direct, "udevadm", ["settle"], OnFail::Continue),
    "Wait for udev to settle after delete",
  );
}

fn path_depth(mp: &str) -> usize {
  if mp == "/" || mp.is_empty() {
    return 0;
  }
  mp.split('/').filter(|s| !s.is_empty()).count()
}

fn sort_mount_queue(plan: &mut [MountSpec]) {
  // order: swap first, then root, then shallower paths
  plan.sort_by_key(|m| (!m.is_swap, m.mountpoint != "/", path_depth(&m.mountpoint)));
}

fn mount_queue(mut plan: Vec<MountSpec>) {
  sort_mount_queue(&mut plan);

  if plan.is_empty() {
    return;
  }

  // The filesystems were just created by mkfs. The kernel's block-device
  // buffer cache can still hold pre-mkfs content, so mounting may read a
  // stale superblock. Flush once before the first mount (sync + udev
  // settle are system-wide), and invalidate the cache per device below.
  exec_eval(
    exec(
      ExecMode::Direct,
      "sync",
      Vec::<&str>::new(),
      OnFail::Continue,
    ),
    "Flush pending writes before mounting",
  );
  exec_eval(
    exec(ExecMode::Direct, "udevadm", ["settle"], OnFail::Continue),
    "Wait for udev to settle before mounting",
  );

  let mut created: HashSet<String> = HashSet::new();
  for m in plan {
    if m.is_swap {
      exec_eval(
        exec(ExecMode::Direct, "swapon", [&m.device], OnFail::Error),
        &format!("Activate swap {}", m.device),
      );
      continue;
    }

    let target = if m.mountpoint == "/" {
      MNT.to_string()
    } else {
      format!("{MNT}{}", m.mountpoint.trim_end_matches('/'))
    };

    if created.insert(target.clone()) {
      exec_eval(
        create_dir_all(&target),
        &format!("{target} directory created"),
      );
    }

    exec_eval(
      exec(
        ExecMode::Direct,
        "blockdev",
        ["--flushbufs", &m.device],
        OnFail::Continue,
      ),
      &format!("Invalidate stale buffer cache on {}", m.device),
    );

    mount(&m.device, &target, &m.options, &m.fstype);
  }
}

pub fn close_luks_best_effort(blockdevice: &str) {
  // 1) Conventional name first: /dev/mapper/<basename>crypted
  let conventional = crypt_mapper_name(blockdevice);
  exec_eval(
    exec(
      ExecMode::Direct,
      "cryptsetup",
      ["luksClose", &conventional],
      OnFail::Continue,
    ),
    &format!("Close LUKS mapper (conventional) {conventional}"),
  );

  // 2) Also scan lsblk for any crypt children that reference this blockdevice
  if let Ok(out) = exec_output("lsblk", ["-rno", "NAME,TYPE,PKNAME", "-p"]) {
    let s = String::from_utf8_lossy(&out.stdout);
    for line in s.lines() {
      // Example line: "/dev/mapper/vda2crypt crypt /dev/vda2"
      let mut it = line.split_whitespace();
      let name = it.next().unwrap_or("");
      let typ = it.next().unwrap_or("");
      let pk = it.next().unwrap_or("");
      if typ == "crypt"
        && pk == blockdevice
        && let Some(mapper) = name.strip_prefix("/dev/mapper/")
      {
        exec_eval(
          exec(
            ExecMode::Direct,
            "cryptsetup",
            ["luksClose", mapper],
            OnFail::Continue,
          ),
          &format!("Close LUKS mapper {mapper}"),
        );
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn partition_numbers() {
    assert_eq!(extract_partition_number("/dev/sda3"), "3");
    assert_eq!(extract_partition_number("/dev/nvme0n1p12"), "12");
    assert_eq!(extract_partition_number("/dev/sda"), "");
    assert_eq!(extract_partition_number("/dev/mapper/sda3crypted"), "");
  }

  #[test]
  fn mount_order() {
    let mk = |mp: &str, swap: bool| MountSpec {
      device: String::new(),
      mountpoint: mp.into(),
      options: String::new(),
      fstype: String::new(),
      is_swap: swap,
    };
    let mut plan = vec![
      mk("/home/user", false),
      mk("/", false),
      mk("", true),
      mk("/boot", false),
    ];
    sort_mount_queue(&mut plan);
    let order: Vec<_> = plan.iter().map(|m| m.mountpoint.as_str()).collect();
    assert_eq!(order, ["", "/", "/boot", "/home/user"]);
  }
}
