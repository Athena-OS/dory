use log::debug;
use shared::args::{ShellSetup, is_arch, is_nix};
use shared::files;
use shared::returncode_eval::files_eval;

pub fn install_shell_setup(shell_setup: ShellSetup) -> Vec<&'static str> {
  debug!("Selecting {shell_setup:?}");
  let (mut pkgs, nix_name): (Vec<&'static str>, &str) = match shell_setup {
    ShellSetup::Bash => (vec!["bash", "bash-completion"], "bash"),
    ShellSetup::Fish => (vec!["athena-fish"], "fish"),
    ShellSetup::Zsh => (vec!["athena-zsh"], "zsh"),
  };
  if shell_setup == ShellSetup::Bash && is_arch() {
    pkgs.push("blesh-git");
  }
  if is_nix() {
    files_eval(
      files::sed_file(
        "/mnt/etc/nixos/configuration.nix",
        "  mainShell =.*",
        &format!("  mainShell = \"{nix_name}\";"),
      ),
      &format!("Set {nix_name} shell"),
    );
  }
  pkgs
}

/**********************************/

/// Make the chosen shell the one launched by the "Shell" desktop entry and
/// exported as $SHELL (Arch only). Bash needs nothing.
pub fn configure(shell: ShellSetup) {
  let (desktop_name, bin) = match shell {
    ShellSetup::Fish => ("Fish", "fish"),
    ShellSetup::Zsh => ("Zsh", "zsh"),
    ShellSetup::Bash => {
      log::info!("No shell configuration needed.");
      return;
    }
  };
  files_eval(
    files::sed_file(
      "/mnt/usr/share/applications/shell.desktop",
      "Bash",
      desktop_name,
    ),
    &format!("Apply {desktop_name} shell on .desktop shell file"),
  );
  files_eval(
    files::sed_file(
      "/mnt/etc/skel/.bashrc",
      "export SHELL=.*",
      &format!("export SHELL=$(which {bin})"),
    ),
    &format!("Apply {desktop_name} shell"),
  );
}
