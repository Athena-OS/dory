use crate::exec::exec_output;
use log::info;
use std::fs;
use std::path::Path;
use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub struct RootLuks {
  pub luks_device: String, // e.g. /dev/vda3 (device that holds the LUKS header)
  pub luks_uuid: String,   // luks UUID
  pub mapper_dev: String,  // e.g. vda3crypted
}

/// Find the LUKS container backing the filesystem mounted at `/mnt`.
pub fn find_target_root_luks() -> Option<RootLuks> {
  // NAME is absolute with -p; TYPE helps for logging.
  let out = exec_output("lsblk", ["-nrp", "-o", "NAME,TYPE,MOUNTPOINT,PKNAME"]).ok()?;
  let listing = String::from_utf8_lossy(&out.stdout);

  // /mnt/home covers the BTRFS case where <dev>crypted is shown mounted on
  // /mnt/home in lsblk (last mount on it).
  let (mapper_dev, parent_dev) = listing.lines().find_map(|line| {
    let mut it = line.split_whitespace();
    let name = it.next()?;
    let typ = it.next()?;
    let mp = it.next().unwrap_or("");
    let pk = it.next().unwrap_or("");
    ((mp == "/mnt" || mp.starts_with("/mnt/")) && typ == "crypt")
      .then(|| (name.to_string(), pk.to_string()))
  })?;

  if parent_dev.is_empty() || parent_dev == "-" {
    info!("Mapper {mapper_dev} has no PKNAME (maybe a loop device?)");
    return None;
  }
  info!("find_target_root_luks: start device {mapper_dev}");

  let uuid_out = exec_output("cryptsetup", ["luksUUID", parent_dev.as_str()]).ok()?;
  let uuid = String::from_utf8_lossy(&uuid_out.stdout).trim().to_string();
  if uuid.is_empty() {
    info!("cryptsetup luksUUID returned empty for {parent_dev}");
    return None;
  }

  let mapper_dev = mapper_dev
    .strip_prefix("/dev/mapper/")
    .unwrap_or(&mapper_dev)
    .to_string();

  Some(RootLuks {
    luks_device: parent_dev,
    luks_uuid: uuid,
    mapper_dev,
  })
}

/// Whether a TPM 2.0 device is present. Evaluated once and cached.
///
/// Reads sysfs instead of opening an ESAPI context: no C library dependency,
/// no device open, and the answer cannot change during an install.
pub fn tpm2_available() -> bool {
  static TPM2: OnceLock<bool> = OnceLock::new();
  *TPM2.get_or_init(|| {
    let has_node = Path::new("/dev/tpmrm0").exists() || Path::new("/dev/tpm0").exists();
    if !has_node {
      info!("No TPM device nodes found. TPM 2 not available.");
      return false;
    }
    let is_v2 = fs::read_dir("/sys/class/tpm")
      .ok()
      .into_iter()
      .flatten()
      .flatten()
      .any(|e| {
        fs::read_to_string(e.path().join("tpm_version_major"))
          .map(|v| v.trim() == "2")
          .unwrap_or(false)
      });
    if is_v2 {
      info!("TPM 2.0 available");
    } else {
      info!("TPM device present but not version 2.0");
    }
    is_v2
  })
}
