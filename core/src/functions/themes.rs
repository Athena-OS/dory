use log::debug;
use shared::args::{ThemeSetup, is_nix};
use shared::files;
use shared::returncode_eval::files_eval;

struct Theme {
    /// Arch package.
    package: &'static str,
    /// Value of `theme = "…";` in the Athena Nix configuration.
    nix_name: &'static str,
    /// VSCodium workbench color theme.
    vscodium: &'static str,
    /// tmux power theme.
    tmux: &'static str,
}

fn theme(setup: ThemeSetup) -> Option<Theme> {
    use ThemeSetup::{Cyborg, Frost, Graphite, HackTheBox, RedMoon, Samurai, Sweet, Temple};
    Some(match setup {
        Cyborg => Theme { package: "athena-cyborg-design", nix_name: "cyborg", vscodium: "Gruvbox Material Dark", tmux: "gold" },
        Frost => Theme { package: "athena-frost-design", nix_name: "frost", vscodium: "Tokyo Night Storm", tmux: "sky" },
        Graphite => Theme { package: "athena-graphite-design", nix_name: "graphite", vscodium: "Just Black", tmux: "snow" },
        HackTheBox => Theme { package: "athena-htb-design", nix_name: "hackthebox", vscodium: "Hack The Box", tmux: "forest" },
        RedMoon => Theme { package: "athena-redmoon-design", nix_name: "redmoon", vscodium: "red-blood", tmux: "redwine" },
        Samurai => Theme { package: "athena-samurai-design", nix_name: "samurai", vscodium: "Tokyo Night Storm", tmux: "sky" },
        Sweet => Theme { package: "athena-sweetdark-design", nix_name: "sweet", vscodium: "Radical", tmux: "violet" },
        Temple => Theme { package: "athena-temple-design", nix_name: "temple", vscodium: "Tokyo Night Storm", tmux: "sky" },
        ThemeSetup::None => return None,
    })
}

pub fn install_theme_setup(theme_setup: ThemeSetup) -> Vec<&'static str> {
    debug!("Selecting {theme_setup:?}");
    let Some(t) = theme(theme_setup) else {
        debug!("No theme setup selected");
        return Vec::new();
    };
    if is_nix() {
        files_eval(
            files::sed_file("/mnt/etc/nixos/configuration.nix", "  theme =.*", &format!("  theme = \"{}\";", t.nix_name)),
            &format!("Set {} design", t.nix_name),
        );
    }
    vec![t.package]
}

/// Apply the design to the skeleton configs of the target system (Arch only).
pub fn configure(theme_setup: ThemeSetup) {
    let Some(t) = theme(theme_setup) else {
        log::info!("No design configuration needed.");
        return;
    };
    files_eval(
        files::sed_file(
            "/mnt/etc/skel/.config/VSCodium/User/settings.json",
            "\"workbench.colorTheme\":.*",
            &format!("\"workbench.colorTheme\": \"{}\",", t.vscodium),
        ),
        &format!("Apply {} VSCodium theme", t.vscodium),
    );
    files_eval(
        files::sed_file(
            "/mnt/etc/skel/.tmux.conf",
            "set -g @tmux_power_theme.*",
            &format!("set -g @tmux_power_theme '{}'", t.tmux),
        ),
        &format!("Apply {} Tmux theme", t.tmux),
    );
}
