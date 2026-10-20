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

pub fn create_hosts() {
    files_eval(
        files::write_file("/mnt/etc/hosts", "127.0.0.1     localhost\n"),
        "create /etc/hosts",
    );
}
