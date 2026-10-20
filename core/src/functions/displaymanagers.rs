use crate::functions::desktops;
use crate::internal::services::enable_service;
use log::debug;
use shared::args::{DMSetup, DesktopSetup, SddmTheme, is_arch, is_nix};
use shared::files;
use shared::returncode_eval::files_eval;

const NIX_CONFIG: &str = "/mnt/etc/nixos/configuration.nix";

pub fn install_dm_setup(dm_setup: DMSetup) -> Vec<&'static str> {
  debug!("Selecting {dm_setup:?}");
  let (packages, nix_name): (&[&'static str], Option<&str>) = match dm_setup {
    DMSetup::Gdm => (&["athena-gdm-config"], Some("gdm")),
    DMSetup::LightDMNeon => (&["athena-lightdm-neon-theme"], Some("lightdm")),
    DMSetup::Ly => (&["ly"], Some("ly")),
    DMSetup::Sddm(_) => (&["sddm-astronaut-theme"], Some("sddm")),
    DMSetup::None => (&[], None),
  };
  if is_nix()
    && let Some(name) = nix_name
  {
    files_eval(
      files::sed_file(
        NIX_CONFIG,
        "  dmanager =.*",
        &format!("  dmanager = \"{name}\";"),
      ),
      &format!("Set {name} display manager"),
    );
  }
  packages.to_vec()
}

/**********************************/

pub fn configure(dm: DMSetup, desktop: DesktopSetup) {
  match dm {
    DMSetup::Gdm => configure_gdm(desktop),
    DMSetup::LightDMNeon => configure_lightdm_neon(desktop),
    DMSetup::Ly => configure_ly(),
    DMSetup::Sddm(theme) => configure_sddm(theme),
    DMSetup::None => log::info!("No display manager configuration needed."),
  }
}

fn configure_gdm(desktop: DesktopSetup) {
  if desktop != DesktopSetup::Gnome {
    files::rename_file(
      "/mnt/usr/lib/udev/rules.d/61-gdm.rules",
      "/mnt/usr/lib/udev/rules.d/61-gdm.rules.bak",
    );
    if is_arch() {
      desktops::disable_wsession("gnome-classic.desktop");
    }
  }
  enable_service("gdm");
}

fn configure_lightdm_neon(desktop: DesktopSetup) {
  let session = match desktop {
    DesktopSetup::Gnome => Some("gnome-wayland"),
    DesktopSetup::XfceRefined | DesktopSetup::XfcePicom => Some("xfce"),
    DesktopSetup::Hyprland => Some("Hyprland"),
    _ => None,
  };
  if let Some(session) = session {
    files_eval(
      files::sed_file(
        "/mnt/etc/lightdm/lightdm.conf",
        "#user-session=.*",
        &format!("user-session={session}"),
      ),
      &format!("Apply {session} user session on LightDM"),
    );
  }
  enable_service("lightdm");
}

fn configure_ly() {
  if is_nix() {
    return;
  }
  let edits = [
    ("animation =.*", "animation = matrix", "Set Ly theme"),
    (
      "brightness_down_key =.*",
      "brightness_down_key = null",
      "Disable Ly Brightness Down key setting",
    ),
    (
      "brightness_up_key =.*",
      "brightness_up_key = null",
      "Disable Ly Brightness Up key setting",
    ),
    (
      "hide_version_string =.*",
      "hide_version_string = true",
      "Hide Ly version setting",
    ),
  ];
  for (find, replace, what) in edits {
    files_eval(
      files::sed_file("/mnt/etc/ly/config.ini", find, replace),
      what,
    );
  }
  enable_service("ly@tty1");
}

fn configure_sddm(theme: SddmTheme) {
  let name = theme.conf_name();
  if is_nix() {
    files_eval(
      files::sed_file(
        NIX_CONFIG,
        "  sddmtheme =.*",
        &format!("  sddmtheme = \"{name}\";"),
      ),
      "Set SDDM theme",
    );
    return;
  }

  files_eval(
    files::create_directory("/mnt/etc/sddm.conf.d"),
    "Create /mnt/etc/sddm.conf.d",
  );
  files_eval(
    files::write_file(
      "/mnt/etc/sddm.conf",
      "[Theme]\nCurrent=sddm-astronaut-theme\n[XDisplay]\nDisplayCommand=/usr/share/sddm/scripts/Xsetup\n",
    ),
    "Add astronaut theme",
  );
  files_eval(
    files::write_file(
      "/mnt/etc/sddm.conf.d/virtualkbd.conf",
      "[General]\nInputMethod=qtvirtualkeyboard\n",
    ),
    "Add virtual keyboard support",
  );
  files_eval(
    files::append_file(
      "/mnt/usr/share/sddm/scripts/Xsetup",
      "current=\"\"\nfor next in $(xrandr --listmonitors | grep -E \" *[0-9]+:.*\" | cut -d\" \" -f6); do\n  [ -z \"$current\" ] && current=$next && continue\n  xrandr --output \"$current\" --auto --output \"$next\" --auto --right-of \"$current\"\n  current=$next\ndone",
    ),
    "Add multimonitor support",
  );
  files_eval(
    files::sed_file(
      "/mnt/usr/share/sddm/themes/sddm-astronaut-theme/metadata.desktop",
      "ConfigFile=.*",
      &format!("ConfigFile=Themes/{name}.conf"),
    ),
    "Set SDDM theme",
  );
  enable_service("sddm");
}
