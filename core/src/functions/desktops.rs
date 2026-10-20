use log::debug;
use shared::args::{DesktopSetup, is_arch, is_nix};
use shared::files;
use shared::returncode_eval::files_eval;

const NIX_CONFIG: &str = "/mnt/etc/nixos/configuration.nix";
const NIX_XFCE: &str = "/mnt/etc/nixos/home-manager/desktops/xfce/default.nix";

struct Desktop {
  /// Arch packages.
  packages: &'static [&'static str],
  /// Value of `desktop = "…";` in the Athena Nix configuration, if the
  /// desktop is supported there.
  nix_name: Option<&'static str>,
}

const LIGHTDM: &[&str] = &[
  "lightdm",
  "lightdm-gtk-greeter",
  "lightdm-gtk-greeter-settings",
];
const PIPEWIRE: &[&str] = &[
  "pipewire",
  "pipewire-pulse",
  "pipewire-alsa",
  "pipewire-jack",
  "wireplumber",
];

fn desktop(setup: DesktopSetup) -> Desktop {
  use DesktopSetup::{
    Awesome, Bspwm, Budgie, Cinnamon, Enlightenment, Gnome, Herbstluftwm, Hyprland, I3, Kde, Lxqt,
    Mate, Onyx, Sway, XfcePicom, XfceRefined,
  };
  let (packages, nix_name): (&'static [&'static str], Option<&'static str>) = match setup {
    Onyx => (&["xorg", "onyx", "sushi", "gdm"], None),
    Gnome => (&["athena-gnome-config"], Some("gnome")),
    Kde => (&["athena-plasma-base"], None),
    Budgie => (
      &[
        "xorg",
        "budgie-desktop",
        "gnome",
        "xdg-desktop-portal",
        "xdg-desktop-portal-gtk",
        "xdg-utils",
      ],
      None,
    ),
    Cinnamon => (&["athena-cinnamon-base"], Some("cinnamon")),
    Mate => (&["athena-mate-base"], Some("mate")),
    XfceRefined => (&["athena-xfce-refined"], Some("xfce")),
    XfcePicom => (&["athena-xfce-picom"], Some("xfce")),
    Enlightenment => (&["xorg", "enlightenment", "terminology"], None),
    Lxqt => (
      &[
        "xorg",
        "lxqt",
        "breeze-icons",
        "nm-tray",
        "xscreensaver",
        "sddm",
      ],
      None,
    ),
    Sway => (
      &[
        "xorg-xwayland",
        "sway",
        "bemenu",
        "foot",
        "mako",
        "polkit",
        "swaybg",
        "sddm",
        "xdg-user-dirs",
        "dex",
        "polkit-gnome",
      ],
      None,
    ),
    I3 => (
      &[
        "xorg",
        "i3-wm",
        "dmenu",
        "i3lock",
        "i3status",
        "rxvt-unicode",
        "xdg-user-dirs",
        "dex",
        "polkit-gnome",
      ],
      None,
    ),
    Herbstluftwm => (
      &[
        "xorg",
        "herbstluftwm",
        "dmenu",
        "dzen2",
        "xorg-xsetroot",
        "xdg-user-dirs",
      ],
      None,
    ),
    Awesome => (
      &[
        "xorg",
        "awesome",
        "dex",
        "rlwrap",
        "vicious",
        "xdg-user-dirs",
      ],
      None,
    ),
    Bspwm => (&["athena-bspwm-config"], None),
    Hyprland => (&["hyprland"], None),
    DesktopSetup::None => (&[], Some("none")),
  };
  Desktop { packages, nix_name }
}

/// Extra package groups some desktops need on top of their own list.
fn extras(setup: DesktopSetup) -> &'static [&'static [&'static str]] {
  use DesktopSetup::{Awesome, Budgie, Enlightenment, Herbstluftwm, I3, Lxqt, Onyx, Sway};
  match setup {
    Onyx | Sway | Lxqt => &[PIPEWIRE],
    Enlightenment | Budgie => &[PIPEWIRE, LIGHTDM],
    I3 | Herbstluftwm | Awesome => &[LIGHTDM],
    _ => &[],
  }
}

pub fn install_desktop_setup(desktop_setup: DesktopSetup) -> Vec<&'static str> {
  debug!("Selecting {desktop_setup:?}");
  let d = desktop(desktop_setup);

  if is_nix() {
    if let Some(name) = d.nix_name {
      files_eval(
        files::sed_file(
          NIX_CONFIG,
          "  desktop =.*",
          &format!("  desktop = \"{name}\";"),
        ),
        &format!("Set {name} desktop"),
      );
    }
    if matches!(
      desktop_setup,
      DesktopSetup::XfceRefined | DesktopSetup::XfcePicom
    ) {
      let refined = desktop_setup == DesktopSetup::XfceRefined;
      files_eval(
        files::sed_file(
          NIX_XFCE,
          "athena.desktops.xfce.refined =.*",
          &format!("athena.desktops.xfce.refined = {refined};"),
        ),
        "Set XFCE variant",
      );
    }
  }

  let mut pkgs: Vec<&'static str> = d.packages.to_vec();
  for group in extras(desktop_setup) {
    pkgs.extend_from_slice(group);
  }
  pkgs
}

/**********************************/

/// Post-install tweaks for the chosen desktop (Arch only).
pub fn configure(desktop: DesktopSetup) {
  match desktop {
    // GNOME Classic session is not themed
    DesktopSetup::Gnome if is_arch() => disable_wsession("gnome-classic.desktop"),
    // Cinnamon Wayland session freezes and does not apply theme (tested on VM)
    DesktopSetup::Cinnamon => disable_wsession("cinnamon-wayland.desktop"),
    // XFCE Wayland session produces black screen after login (tested on VM)
    DesktopSetup::XfceRefined | DesktopSetup::XfcePicom => disable_wsession("xfce-wayland.desktop"),
    // Hyprland UWSM does not work well
    DesktopSetup::Hyprland => disable_wsession("hyprland-uwsm.desktop"),
    _ => log::info!("No desktop configuration needed."),
  }
}

pub fn disable_wsession(session: &str) {
  debug!("Disabling {session}");
  let path = format!("/mnt/usr/share/wayland-sessions/{session}");
  files::rename_file(&path, &format!("{path}.disable"));
}
