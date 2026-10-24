use log::{info, warn};
use shared::args::is_nix;
use shared::files;
use shared::returncode_eval::files_eval;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

pub fn set_hostname(hostname: &str) {
  info!("Setting hostname to {hostname}");
  let sanitized_hostname = hostname.replace(' ', "");
  if !is_nix() {
    files_eval(
      files::write_file("/mnt/etc/hostname", &format!("{sanitized_hostname}\n")),
      "set hostname",
    );
  } else {
    files_eval(
      files::sed_file(
        "/mnt/etc/nixos/configuration.nix",
        "  hostname =.*",
        &format!("  hostname = \"{sanitized_hostname}\";"),
      ),
      "Set Hostname",
    );
  }
}

/// Contents of /etc/hosts for the target system.
///
/// The machine's own name must resolve without asking a DNS server: with a
/// resolver that drops queries for `<host>.localdomain` (VMware NAT, some
/// routers), anything that looks up the local hostname, Firefox's profile
/// lock at startup for one, otherwise waits for several 5 s DNS timeouts.
pub fn hosts_file(hostname: &str) -> String {
  let host = hostname.replace(' ', "");
  let lines = [
    "127.0.0.1   localhost".to_string(),
    "::1         localhost".to_string(),
    format!("127.0.1.1   {host}.localdomain {host}"),
  ];
  lines.join("\n") + "\n"
}

pub fn create_hosts(hostname: &str) {
  files_eval(
    files::write_file("/mnt/etc/hosts", &hosts_file(hostname)),
    "create /etc/hosts",
  );
}

/// Carry the live environment's NetworkManager connection profiles (Wi-Fi
/// networks and their passwords) over to the installed system, so the user
/// does not have to enter them again after the first boot.
///
/// NetworkManager ignores profile files that are readable by anyone other
/// than root ("file permissions are insecure"), so the copies are made 0600
/// explicitly; a plain copy inherits the umask and produces 0644 files that
/// NetworkManager silently skips.
pub fn copy_network_profiles() {
  copy_network_profiles_between(
    Path::new("/etc/NetworkManager/system-connections"),
    Path::new("/mnt/etc/NetworkManager/system-connections"),
  );
}

fn copy_network_profiles_between(src: &Path, dst: &Path) {
  let Ok(entries) = fs::read_dir(src) else {
    info!("No NetworkManager profiles to copy");
    return;
  };
  if let Err(e) = fs::create_dir_all(dst) {
    warn!("Cannot create {}: {e}", dst.display());
    return;
  }

  let mut copied = 0;
  for entry in entries.flatten() {
    let path = entry.path();
    if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("nmconnection") {
      continue;
    }
    let target = dst.join(entry.file_name());
    match fs::copy(&path, &target)
      .and_then(|_| fs::set_permissions(&target, fs::Permissions::from_mode(0o600)))
    {
      Ok(()) => {
        copied += 1;
        info!(
          "Copied network profile {}",
          entry.file_name().to_string_lossy()
        );
      }
      Err(e) => warn!("Could not copy network profile {}: {e}", path.display()),
    }
  }
  info!("{copied} NetworkManager profile(s) copied to the target system");
}

#[cfg(test)]
mod tests {
  use std::os::unix::fs::PermissionsExt;

  #[test]
  fn network_profiles_are_copied_root_only() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    let dst = dir.path().join("dst");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
      src.join("Home.nmconnection"),
      "[wifi-security]\npsk=secret\n",
    )
    .unwrap();
    std::fs::write(src.join("README"), "not a profile").unwrap();
    super::copy_network_profiles_between(&src, &dst);
    let copied = dst.join("Home.nmconnection");
    assert_eq!(
      std::fs::read_to_string(&copied).unwrap(),
      "[wifi-security]\npsk=secret\n"
    );
    assert_eq!(
      std::fs::metadata(&copied).unwrap().permissions().mode() & 0o777,
      0o600
    );
    assert!(!dst.join("README").exists());
  }

  #[test]
  fn hosts_file_maps_the_hostname() {
    let h = super::hosts_file("athenaos");
    assert_eq!(
      h,
      "127.0.0.1   localhost\n::1         localhost\n127.0.1.1   athenaos.localdomain athenaos\n"
    );
    assert!(
      h.lines().all(|l| !l.starts_with(' ')),
      "no indented lines: {h:?}"
    );
  }
}
