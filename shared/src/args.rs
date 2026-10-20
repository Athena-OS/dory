use clap::Parser;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::OnceLock;

pub trait ExtendIntoString {
    fn extend_into<I>(&mut self, src: I)
    where
        I: IntoIterator,
        I::Item: Into<String>;
}

impl ExtendIntoString for Vec<String> {
    fn extend_into<I>(&mut self, src: I)
    where
        I: IntoIterator,
        I::Item: Into<String>,
    {
        self.extend(src.into_iter().map(Into::into));
    }
}

#[derive(Debug, Parser)]
#[command(name = "dory")]
#[command(author = env!("CARGO_PKG_AUTHORS"))]
#[command(version = env!("CARGO_PKG_VERSION"))]
#[command(about = env!("CARGO_PKG_DESCRIPTION"), long_about = None)]
pub struct Cli {
    /// Increase log verbosity. Pass multiple times for more detail
    /// (`-v` debug, `-vv` trace). Logs are written to /tmp/dory.log.
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Use an existing system config JSON file (TUI will pass this to the installer)
    #[arg(long = "system-file", short = 's')]
    pub system_file: Option<PathBuf>,

    /// Use an existing drives/partition JSON file (TUI will pass this to the installer)
    #[arg(long = "drives-file", short = 'd')]
    pub drives_file: Option<PathBuf>,

    /// Dry-run: validate the configuration only; do not install
    #[arg(long = "dry", short = 'n', visible_alias = "dry-run")]
    pub dry: bool,

    /// Set the keyboard layout for the current live environment and exit.
    ///
    /// With no value (`--set-keymap`), launches the interactive TUI selector.
    /// With a value (`--set-keymap=de`), applies the layout immediately and
    /// exits without entering the installer TUI.
    #[arg(long = "set-keymap", value_name = "KEYMAP",
          num_args = 0..=1, default_missing_value = "")]
    pub set_keymap: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub enum ExecMode<'a> {
    Direct,
    Chroot { root: &'a str },
}

#[derive(Clone, Copy, Debug)]
pub enum OnFail {
    /// Non-zero exit status => return Err
    Error,
    /// Non-zero exit status => log and return Ok(status)
    Continue,
}

#[derive(Clone, Debug)]
pub enum ConfigInput {
    File(PathBuf),
    JsonString(String),
}

#[derive(Serialize, Deserialize)]
pub struct Config {
    // ----- system config (first JSON) -----
    pub base: String,
    pub design: String,

    #[serde(rename = "desktop_environment")]
    pub desktop: String,

    #[serde(rename = "display_manager")]
    pub displaymanager: String,

    pub hostname: String,

    #[serde(default)]
    pub keyboard_layout: Option<String>,

    /// Locale line as found in locale.gen, e.g. "en_US.UTF-8 UTF-8"
    pub locale: String,

    pub timezone: String,

    #[serde(rename = "root_passwd_hash")]
    pub rootpass: String,

    pub users: Vec<User>,

    #[serde(default)]
    pub extra_packages: Vec<String>,

    // ----- disk/partition config (second JSON, top-level) -----
    #[serde(flatten)]
    pub partition: Disk,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum PackageManager {
    Pacman,
    Pacstrap,
    Nix,
}

#[derive(Clone, Debug)]
pub struct MountSpec {
    pub device: String,
    pub mountpoint: String,
    pub options: String,
    pub fstype: String,
    pub is_swap: bool,
}

// ---- Disk / Partitions match the second JSON file ----
#[derive(Serialize, Deserialize)]
pub struct Disk {
    #[serde(rename = "type")]
    pub disk_type: String,
    pub mode: String,
    pub device: String,
    pub content: DiskContent,
}

#[derive(Serialize, Deserialize)]
pub struct DiskContent {
    #[serde(rename = "type")]
    pub table_type: String,
    pub partitions: Vec<Partition>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Partition {
    pub action: String,
    pub mountpoint: Option<String>,
    pub blockdevice: String,
    pub start: String,
    pub end: String,
    pub filesystem: Option<String>,
    pub flags: Vec<String>,
    /// Path of the file holding the LUKS passphrase for this partition
    /// (only meaningful when `flags` contains "encrypt").
    #[serde(default)]
    pub luks_keyfile: Option<String>,
}

impl Partition {
    pub fn has_flag(&self, flag: &str) -> bool {
        self.flags.iter().any(|f| f.eq_ignore_ascii_case(flag))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Base {
    AthenaArch,
    AthenaNix,
    Other,
}

pub static BASE: OnceLock<Base> = OnceLock::new();

pub fn set_base(s: &str) {
    let b = match s {
        "Athena Arch" => Base::AthenaArch,
        "Athena Nix" => Base::AthenaNix,
        _ => Base::Other,
    };
    let _ = BASE.set(b); // ignore if already set
}

pub fn distro_base() -> Base {
    *BASE.get().expect("BASE not initialized")
}

pub fn is_arch() -> bool {
    distro_base() == Base::AthenaArch
}
pub fn is_nix() -> bool {
    distro_base() == Base::AthenaNix
}

#[derive(Serialize, Deserialize)]
pub struct User {
    #[serde(rename = "username")]
    pub name: String,

    #[serde(rename = "password_hash")]
    pub password: String,

    pub shell: String,

    #[serde(default)]
    pub groups: Vec<String>,
}

/// Error returned when a config string does not name a known option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownOption(pub String);

impl std::fmt::Display for UnknownOption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown option '{}'", self.0)
    }
}

fn norm(s: &str) -> String {
    s.trim().to_lowercase()
}

#[derive(Debug, Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Serialize, Deserialize)]
pub enum DesktopSetup {
    Onyx,
    Gnome,
    Kde,
    Budgie,
    Cinnamon,
    Mate,
    XfceRefined,
    XfcePicom,
    Enlightenment,
    Lxqt,
    Sway,
    I3,
    Herbstluftwm,
    Awesome,
    Bspwm,
    Hyprland,
    None,
}

impl FromStr for DesktopSetup {
    type Err = UnknownOption;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match norm(s).as_str() {
            "onyx" => Self::Onyx,
            "gnome" => Self::Gnome,
            "kde plasma" | "kde" | "plasma" => Self::Kde,
            "budgie" => Self::Budgie,
            "cinnamon" => Self::Cinnamon,
            "mate" => Self::Mate,
            "xfce" => Self::XfceRefined,
            "xfce picom" => Self::XfcePicom,
            "enlightenment" => Self::Enlightenment,
            "lxqt" => Self::Lxqt,
            "sway" => Self::Sway,
            "i3" => Self::I3,
            "herbstluftwm" => Self::Herbstluftwm,
            "awesome" => Self::Awesome,
            "bspwm" => Self::Bspwm,
            "hyprland" => Self::Hyprland,
            "none" => Self::None,
            _ => return Err(UnknownOption(s.to_string())),
        })
    }
}

#[derive(Debug, Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Serialize, Deserialize)]
pub enum ThemeSetup {
    Cyborg,
    Frost,
    Graphite,
    HackTheBox,
    RedMoon,
    Samurai,
    Sweet,
    Temple,
    None,
}

impl FromStr for ThemeSetup {
    type Err = UnknownOption;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match norm(s).as_str() {
            "cyborg" => Self::Cyborg,
            "frost" => Self::Frost,
            "graphite" => Self::Graphite,
            "hackthebox" => Self::HackTheBox,
            "redmoon" => Self::RedMoon,
            "samurai" => Self::Samurai,
            "sweet" => Self::Sweet,
            "temple" => Self::Temple,
            "none" => Self::None,
            _ => return Err(UnknownOption(s.to_string())),
        })
    }
}

/// Display manager choice. SDDM carries the selected astronaut theme variant.
#[derive(Debug, Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Serialize, Deserialize)]
pub enum DMSetup {
    Gdm,
    LightDMNeon,
    Ly,
    Sddm(SddmTheme),
    None,
}

#[derive(Debug, Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Serialize, Deserialize)]
pub enum SddmTheme {
    Astronaut,
    BlackHole,
    Cyberpunk,
    Cyborg,
    JakeTheDog,
    Kath,
    PixelSakura,
    PostApocalypse,
    PurpleLeaves,
}

impl SddmTheme {
    /// Theme config name as used both by the sddm-astronaut-theme package
    /// (`Themes/<name>.conf`) and the Athena Nix `sddmtheme` option.
    pub fn conf_name(self) -> &'static str {
        match self {
            Self::Astronaut => "astronaut",
            Self::BlackHole => "black_hole",
            Self::Cyberpunk => "cyberpunk",
            Self::Cyborg => "japanese_aesthetic",
            Self::JakeTheDog => "jake_the_dog",
            Self::Kath => "hyprland_kath",
            Self::PixelSakura => "pixel_sakura",
            Self::PostApocalypse => "post-apocalyptic_hacker",
            Self::PurpleLeaves => "purple_leaves",
        }
    }
}

impl FromStr for DMSetup {
    type Err = UnknownOption;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match norm(s).as_str() {
            "gdm" => Self::Gdm,
            "lightdm neon" => Self::LightDMNeon,
            "ly" => Self::Ly,
            "astronaut" => Self::Sddm(SddmTheme::Astronaut),
            "black hole" => Self::Sddm(SddmTheme::BlackHole),
            "cyberpunk" => Self::Sddm(SddmTheme::Cyberpunk),
            "cyborg" => Self::Sddm(SddmTheme::Cyborg),
            "jake the dog" => Self::Sddm(SddmTheme::JakeTheDog),
            "kath" => Self::Sddm(SddmTheme::Kath),
            "pixel sakura" => Self::Sddm(SddmTheme::PixelSakura),
            "post-apocalypse" => Self::Sddm(SddmTheme::PostApocalypse),
            "purple leaves" => Self::Sddm(SddmTheme::PurpleLeaves),
            "none" => Self::None,
            _ => return Err(UnknownOption(s.to_string())),
        })
    }
}

#[derive(Debug, Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Serialize, Deserialize)]
pub enum ShellSetup {
    Bash,
    Fish,
    Zsh,
}

impl FromStr for ShellSetup {
    type Err = UnknownOption;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match norm(s).as_str() {
            "bash" => Self::Bash,
            "fish" => Self::Fish,
            "zsh" => Self::Zsh,
            _ => return Err(UnknownOption(s.to_string())),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tui_strings() {
        assert_eq!("KDE Plasma".parse::<DesktopSetup>().unwrap(), DesktopSetup::Kde);
        assert_eq!(" xfce ".parse::<DesktopSetup>().unwrap(), DesktopSetup::XfceRefined);
        assert_eq!("Jake the Dog".parse::<DMSetup>().unwrap(), DMSetup::Sddm(SddmTheme::JakeTheDog));
        assert_eq!("Lightdm Neon".parse::<DMSetup>().unwrap(), DMSetup::LightDMNeon);
        assert!("plasmoid".parse::<DesktopSetup>().is_err());
    }
}
