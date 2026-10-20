use log::info;
use shared::args::is_nix;
use shared::files;
use shared::returncode_eval::files_eval;

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

#[cfg(test)]
mod tests {
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
