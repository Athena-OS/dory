use log::{error, info};
use shared::args::{ExecMode, OnFail, is_nix};
use shared::exec::{exec, exec_secret};
use shared::files;
use shared::returncode_eval::{exec_eval, files_eval};

const CHROOT: ExecMode<'static> = ExecMode::Chroot { root: "/mnt" };
const NIX_CONFIG: &str = "/mnt/etc/nixos/configuration.nix";

/// Set an already-hashed password for `user` inside the target system.
///
/// The hash is fed to `chpasswd -e` on stdin: it never appears on a command
/// line (visible in `ps`) nor in the error message logged on failure.
fn set_password_hash(user: &str, hash: &str) {
  let line = format!("{user}:{}", hash.trim());
  exec_eval(
    exec_secret(
      CHROOT,
      "chpasswd",
      ["-e"],
      Some(line.as_bytes()),
      OnFail::Error,
    ),
    &format!("Set password for {user}"),
  );
}

pub fn new_user(username: &str, password_hash: &str, groups: &[String], shell: &str) {
  // Username cannot contain any space
  let sanitized_username = username.replace(' ', "");

  if !is_nix() {
    let shell_path = match shell {
      "bash" => "/bin/bash",
      "csh" => "/usr/bin/csh",
      "fish" => "/usr/bin/fish",
      "tcsh" => "/usr/bin/tcsh",
      "zsh" => "/usr/bin/zsh",
      _ => "/usr/bin/bash",
    };
    exec_eval(
      exec(
        CHROOT,
        "useradd",
        ["-m", "-s", shell_path, &sanitized_username],
        OnFail::Error,
      ),
      &format!("Create user {sanitized_username}"),
    );
    set_password_hash(&sanitized_username, password_hash);
    if !groups.is_empty() {
      exec_eval(
        exec(
          CHROOT,
          "usermod",
          ["-aG", &groups.join(","), &sanitized_username],
          OnFail::Error,
        ),
        &format!("Add user {sanitized_username} to specified groups"),
      );
    }
    files_eval(
      files::create_directory("/mnt/var/lib/AccountsService/users/"),
      "Create /mnt/var/lib/AccountsService",
    );
    files_eval(
      files::write_file(
        &format!("/mnt/var/lib/AccountsService/users/{sanitized_username}"),
        "[User]\nSession=gnome-xorg\nIcon=/usr/share/pixmaps/faces/hackmyavatar.jpg\n",
      ),
      &format!("Populate AccountsService user file for {sanitized_username}"),
    );
  } else {
    let user_line = format!("  username = \"{sanitized_username}\";");
    match files::replace_line_in_file(NIX_CONFIG, "  username = \"", &user_line) {
      Ok(_) => info!("Set username"),
      Err(e) => error!("Set username ERROR: {e}"),
    }

    let hash_line = format!("  hashed = \"{password_hash}\";");
    match files::replace_line_in_file(NIX_CONFIG, "  hashed = \"", &hash_line) {
      Ok(_) => info!("Set user password hash"),
      Err(e) => error!("Set user password hash ERROR: {e}"),
    }

    let groups_nix = groups
      .iter()
      .map(|g| format!(r#""{g}""#))
      .collect::<Vec<_>>()
      .join(" ");
    // Replace the whole extraGroups assignment, preserving indentation
    files_eval(
      files::sed_file(
        NIX_CONFIG,
        r"(?ms)^(\s*)extraGroups\s*=\s*\[.*?\];",
        &format!(r#"${{1}}extraGroups = [ {groups_nix} ];"#),
      ),
      "Set user extraGroups",
    );
  }
}

/// Allow members of `wheel` to use sudo. Done once, not per user.
pub fn enable_wheel_sudo() {
  files_eval(
    files::sed_file(
      "/mnt/etc/sudoers",
      "# %wheel ALL=\\(ALL:ALL\\) ALL",
      "%wheel ALL=(ALL:ALL) ALL",
    ),
    "Add wheel group to sudoers",
  );
}

pub fn root_pass(root_pass: &str) {
  if !is_nix() {
    set_password_hash("root", root_pass);
  } else {
    let hash_line = format!("  hashedRoot = \"{root_pass}\";");
    match files::replace_line_in_file(NIX_CONFIG, "  hashedRoot = \"", &hash_line) {
      Ok(_) => info!("Set root password hash"),
      Err(e) => error!("Set root password hash ERROR: {e}"),
    }
  }
}
