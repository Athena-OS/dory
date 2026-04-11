use log::debug;
use shared::args::{DesktopSetup, is_arch, is_nix};
use shared::files;
use shared::returncode_eval::files_eval;

pub fn install_desktop_setup(desktop_setup: DesktopSetup) -> Vec<&'static str> {
    debug!("Selecting {desktop_setup:?}");

    match desktop_setup {
        DesktopSetup::Onyx => install_onyx(),
        DesktopSetup::Gnome => install_gnome(),
        DesktopSetup::Kde => install_kde(),
        DesktopSetup::Budgie => install_budgie(),
        DesktopSetup::Cinnamon => install_cinnamon(),
        DesktopSetup::Mate => install_mate(),
        DesktopSetup::XfceRefined => install_xfce_refined(),
        DesktopSetup::XfcePicom => install_xfce_picom(),
        DesktopSetup::Enlightenment => install_enlightenment(),
        DesktopSetup::Lxqt => install_lxqt(),
        DesktopSetup::Sway => install_sway(),
        DesktopSetup::I3 => install_i3(),
        DesktopSetup::Herbstluftwm => install_herbstluftwm(),
        DesktopSetup::Awesome => install_awesome(),
        DesktopSetup::Bspwm => install_bspwm(),
        DesktopSetup::Hyprland => install_hyprland(),
        DesktopSetup::None => {
            if is_nix() {
                files_eval(
                    files::sed_file(
                        "/mnt/etc/nixos/configuration.nix",
                        "  desktop =.*",
                        "  desktop = \"none\";",
                    ),
                    "Set Cinnamon",
                );        
            }
            debug!("No desktop setup selected");
            Vec::new() // Return empty vector for "None"
        }
    }
}

fn install_hyprland() -> Vec<&'static str> {
    vec![
        "hyprland",
    ]
}

fn install_bspwm() -> Vec<&'static str> {
    vec![
        "athena-bspwm-config",
    ]
}

fn install_awesome() -> Vec<&'static str> {
    vec![
        "xorg",
        "awesome",
        "dex",
        "rlwrap",
        "vicious",
        "lightdm",
        "lightdm-gtk-greeter",
        "lightdm-gtk-greeter-settings",
        "xdg-user-dirs",
    ]
}

fn install_herbstluftwm() -> Vec<&'static str> {
    vec![
        "xorg",
        "herbstluftwm",
        "dmenu",
        "dzen2",
        "xorg-xsetroot",
        "lightdm",
        "lightdm-gtk-greeter",
        "lightdm-gtk-greeter-settings",
        "xdg-user-dirs",
    ]
}

fn install_i3() -> Vec<&'static str> {
    vec![
        "xorg",
        "i3-wm",
        "dmenu",
        "i3lock",
        "i3status",
        "rxvt-unicode",
        "lightdm",
        "lightdm-gtk-greeter",
        "lightdm-gtk-greeter-settings",
        "xdg-user-dirs",
        "dex",
        "polkit-gnome",
    ]
}

fn install_sway() -> Vec<&'static str> {
    vec![
        "xorg-xwayland",
        "sway",
        "bemenu",
        "foot",
        "mako",
        "polkit",
        "swaybg",
        "pipewire",
        "pipewire-pulse",
        "pipewire-alsa",
        "pipewire-jack",
        "wireplumber",
        "sddm",
        "xdg-user-dirs",
        "dex",
        "polkit-gnome",
    ]
}

fn install_lxqt() -> Vec<&'static str> {
    vec![
        "xorg",
        "lxqt",
        "breeze-icons",
        "nm-tray",
        "xscreensaver",
        "pipewire",
        "pipewire-pulse",
        "pipewire-alsa",
        "pipewire-jack",
        "wireplumber",
        "sddm",
    ]
}

fn install_enlightenment() -> Vec<&'static str> {
    vec![
        "xorg",
        "enlightenment",
        "terminology",
        "pipewire",
        "pipewire-pulse",
        "pipewire-alsa",
        "pipewire-jack",
        "wireplumber",
        "lightdm",
        "lightdm-gtk-greeter",
        "lightdm-gtk-greeter-settings",
    ]
}

fn install_xfce_refined() -> Vec<&'static str> {
    if is_nix() {
        files_eval(
            files::sed_file(
                "/mnt/etc/nixos/configuration.nix",
                "  desktop =.*",
                "  desktop = \"xfce\";",
            ),
            "Set XFCE",
        );
        files_eval(
            files::sed_file(
                "/mnt/etc/nixos/home-manager/desktops/xfce/default.nix",
                "athena.desktops.xfce.refined =.*",
                "athena.desktops.xfce.refined = true;",
            ),
            "Set XFCE Refined",
        );        
    }
    vec![
        "athena-xfce-refined",
    ]
}

fn install_xfce_picom() -> Vec<&'static str> {
    if is_nix() {
        files_eval(
            files::sed_file(
                "/mnt/etc/nixos/configuration.nix",
                "  desktop =.*",
                "  desktop = \"xfce\";",
            ),
            "Set XFCE",
        );
        files_eval(
            files::sed_file(
                "/mnt/etc/nixos/home-manager/desktops/xfce/default.nix",
                "athena.desktops.xfce.refined =.*",
                "athena.desktops.xfce.refined = false;",
            ),
            "Set XFCE Picom",
        );        
    }
    vec![
        "athena-xfce-picom",
    ]
}

fn install_mate() -> Vec<&'static str> {
    if is_nix() {
        files_eval(
            files::sed_file(
                "/mnt/etc/nixos/configuration.nix",
                "  desktop =.*",
                "  desktop = \"mate\";",
            ),
            "Set MATE",
        );
    }
    vec![
        "athena-mate-base",
    ]
}

fn install_cinnamon() -> Vec<&'static str> {
    if is_nix() {
        files_eval(
            files::sed_file(
                "/mnt/etc/nixos/configuration.nix",
                "  desktop =.*",
                "  desktop = \"cinnamon\";",
            ),
            "Set Cinnamon",
        );        
    }
    vec![
        "athena-cinnamon-base",
    ]
}

fn install_budgie() -> Vec<&'static str> {
    vec![
        "xorg",
        "budgie-desktop",
        "gnome",
        "pipewire",
        "pipewire-pulse",
        "pipewire-alsa",
        "pipewire-jack",
        "wireplumber",
        "lightdm",
        "lightdm-gtk-greeter",
        "lightdm-gtk-greeter-settings",
        "xdg-desktop-portal",
        "xdg-desktop-portal-gtk",
        "xdg-utils",
    ]
}

fn install_kde() -> Vec<&'static str> {
    vec![
        "athena-plasma-base",
    ]
}

fn install_gnome() -> Vec<&'static str> {
    if is_nix() {
        files_eval(
            files::sed_file(
                "/mnt/etc/nixos/configuration.nix",
                "  desktop =.*",
                "  desktop = \"gnome\";",
            ),
            "Set GNOME",
        );        
    }
    vec![
        "athena-gnome-config",
    ]
}

fn install_onyx() -> Vec<&'static str> {
    vec![
        "xorg",
        "onyx",
        "sushi",
        "pipewire",
        "pipewire-pulse",
        "pipewire-alsa",
        "pipewire-jack",
        "wireplumber",
        "gdm",
    ]
}

/**********************************/

pub fn configure_gnome() {
    if is_arch() {
        disable_wsession("gnome-classic.desktop");
        disable_wsession("gnome-wayland.desktop");
    }
}

pub fn configure_cinnamon() {
    disable_wsession("cinnamon-wayland.desktop"); //Currently Cinnamon Wayland session freezes and does not apply theme (tested on VM)
}

pub fn configure_xfce() {
    disable_wsession("xfce-wayland.desktop"); //Currently XFCE Wayland session produces black screen after login (tested on VM)
}

pub fn configure_hyprland() {
    disable_wsession("hyprland-uwsm.desktop"); //Currently Hyprland UWSM does not work well
}

pub fn disable_wsession(session: &str) {
    debug!("Disabling {session}");
    files::rename_file(&("/mnt/usr/share/wayland-sessions/".to_owned()+session), &("/mnt/usr/share/wayland-sessions/".to_owned()+session+".disable"));
}
