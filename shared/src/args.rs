use clap::{Args, Parser, Subcommand, ValueEnum};
use log::info;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
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
#[command(author=env!("CARGO_PKG_AUTHORS"))]
#[command(version = env!("CARGO_PKG_VERSION"))]
#[command(about = env!("CARGO_PKG_DESCRIPTION"), long_about = None)]
pub struct Cli {
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Use an existing system config JSON file (TUI will pass this to dory-core)
    #[arg(long = "system-file", short = 's')]
    pub system_file: Option<std::path::PathBuf>,

    /// Use an existing drives/partition JSON file (TUI will pass this to dory-core)
    #[arg(long = "drives-file", short = 'd')]
    pub drives_file: Option<std::path::PathBuf>,

    /// Additional JSON fragments to merge (forwarded to dory-core or used to build temps)
    #[arg(long = "json", short = 'j', value_name = "JSON")]
    pub json: Vec<String>,

    /// Dry-run: validate only; do not install (TUI forwards this to dory-core)
    #[arg(long = "dry", short = 'n', visible_alias = "dry-run")]
    pub dry: bool,
}

#[derive(Clone, Copy, Debug)]
pub enum ExecMode<'a> {
    Direct,
    Chroot { root: &'a str },
}

#[derive(Clone, Copy, Debug)]
pub enum OnFail {
    /// Non-zero exit status => return Err (current behavior)
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

    // Flat hostname (old code used Networking{ hostname })
    pub hostname: String,

    // Optional extras coming from system JSON
    #[serde(default)]
    pub keyboard_layout: Option<String>,

    // New schema has locale as a single string (old code expected Vec<String>)
    pub locale: String,

    pub timezone: String,

    #[serde(rename = "root_passwd_hash")]
    pub rootpass: String,

    pub users: Vec<User>,

    #[serde(default)]
    pub extra_packages: Vec<String>,

    // ----- disk/partition config (second JSON, top-level) -----
    // The disk JSON lives at the top level, so flatten its fields into Config.
    #[serde(flatten)]
    pub partition: Disk,
}

#[derive(Debug, ValueEnum, Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Serialize, Deserialize)]
pub enum PackageManager {
    #[value(name = "dnf")]
    Dnf,

    #[value(name = "rpmostree")]
    RpmOSTree,

    #[value(name = "pacman")]
    Pacman,

    #[value(name = "pacstrap")]
    Pacstrap,

    #[value(name = "nix")]
    Nix,

    #[value(name = "None")]
    None,
}

pub enum InstallMode {
    Install,
    Remove,
}

#[derive(Clone, Debug)]
pub struct MountSpec {
    pub device: String,
    pub mountpoint: String,
    pub options: String,
    pub is_swap: bool,
}

// ---- Disk / Partitions match the second JSON file ----
#[derive(Serialize, Deserialize)]
pub struct Disk {
    // second JSON has: { "type": "disk", "device": "/dev/nvme0n1", "content": { ... } }
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

    // "partitions" is an object map: { "BOOT": { ... }, "ROOT": { ... }, ... }
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
}

impl Partition {
    pub fn new(action: String, mountpoint: Option<String>, blockdevice: String, start: String, end: String, filesystem: Option<String>, flags: Vec<String>) -> Self {
        Self {
            action,
            mountpoint,
            blockdevice,
            start,
            end,
            filesystem,
            flags,
        }
    }
}

pub fn parse_partitions(s: &str) -> Result<Partition, &'static str> {
    info!("{s}");
    let parts: Vec<&str> = s.split(':').collect();

    if parts.len() < 6 {
        return Err("Partition spec requires at least 6 fields: action:mount:blockdev:start:end:fs[:flags]");
    }

    let action       = parts[0].to_string();
    let mountpoint   = match parts[1].trim() {
        "" | "-" => None,
        v => Some(v.to_string()),
    };
    let blockdevice  = parts[2].to_string();
    let start        = parts[3].to_string();
    let end          = parts[4].to_string();
    let filesystem   = match parts[5].trim() {
        "" | "-" => None,
        v => Some(v.to_string()),
    };

    let flags = if parts.len() >= 7 && !parts[6].trim().is_empty() {
        parts[6]
            .split(',')
            .map(|f| f.trim().to_string())
            .filter(|f| !f.is_empty())
            .collect()
    } else {
        Vec::new()
    };

    Ok(Partition::new(
        action,
        mountpoint,
        blockdevice,
        start,
        end,
        filesystem,
        flags,
    ))
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
        "Athena Arch"   => Base::AthenaArch,
        "Athena Nix"    => Base::AthenaNix,
        _               => Base::Other,
    };
    let _ = BASE.set(b); // ignore if already set
}

pub fn distro_base() -> Base {
    *BASE.get().expect("BASE not initialized")
}

pub fn is_arch() -> bool   { distro_base() == Base::AthenaArch }
pub fn is_nix() -> bool    { distro_base() == Base::AthenaNix }

#[derive(Debug, Args)]
pub struct LocaleArgs {
    /// The keyboard layout to use
    pub virtkeyboard: String,
    pub x11keyboard: String,

    /// The timezone to use
    pub timezone: String,

    /// The locales to set
    pub locales: Vec<String>,
}

#[derive(Debug, Args)]
pub struct NetworkingArgs {
    /// The hostname to assign to the system
    pub hostname: String,
}

#[derive(Serialize, Deserialize)]
pub struct User {
    // New schema uses "username" and "password_hash"
    #[serde(rename = "username")]
    pub name: String,

    #[serde(rename = "password_hash")]
    pub password: String,

    pub shell: String,

    #[serde(default)]
    pub groups: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub enum UsersSubcommand {
    /// Create a new user
    #[command(name="new-user", aliases=&["newUser"])]
    NewUser(NewUserArgs),

    /// Set the password of the root user
    #[command(name="root-password", aliases=&["root-pass", "rootPass"])]
    RootPass {
        /// The password to set. NOTE: Takes hashed password, use `mkpasswd <password>` to generate the hash.
        password: String,
    },
}

#[derive(Debug, Args)]
pub struct NewUserArgs {
    /// The name of the user to create
    pub username: String,

    /// The password to set. NOTE: Takes hashed password, use `mkpasswd <password>` to generate the hash.
    /// When not providing a password mkpasswd jumps into an interactive masked input mode allowing you to hide your password
    /// from the terminal history.
    pub password: String,

    /// The shell to use for the user. The current options are bash, csh, fish, tcsh, and zsh.
    /// If a shell is not specified or unknown, it defaults to fish.
    pub shell: String,
}

#[derive(Debug, ValueEnum, Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Serialize, Deserialize)]
pub enum DesktopSetup {
    #[value(name = "onyx")]
    Onyx,

    #[value(name = "gnome")]
    Gnome,

    #[value(name = "kde", aliases = ["plasma"])]
    Kde,

    #[value(name = "budgie")]
    Budgie,

    #[value(name = "cinnamon")]
    Cinnamon,

    #[value(name = "mate")]
    Mate,

    #[value(name = "xfce-refined")]
    XfceRefined,

    #[value(name = "xfce-picom")]
    XfcePicom,

    #[value(name = "enlightenment")]
    Enlightenment,

    #[value(name = "lxqt")]
    Lxqt,

    #[value(name = "sway")]
    Sway,

    #[value(name = "i3")]
    I3,

    #[value(name = "herbstluftwm")]
    Herbstluftwm,

    #[value(name = "awesome")]
    Awesome,

    #[value(name = "bspwm")]
    Bspwm,

    #[value(name = "hyprland")]
    Hyprland,

    #[value(name = "None")]
    None,
}

#[derive(Debug, ValueEnum, Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Serialize, Deserialize)]
pub enum ThemeSetup {
    #[value(name = "cyborg")]
    Cyborg,

    #[value(name = "frost")]
    Frost,

    #[value(name = "graphite")]
    Graphite,

    #[value(name = "hackthebox")]
    HackTheBox,

    #[value(name = "redmoon")]
    RedMoon,

    #[value(name = "samurai")]
    Samurai,

    #[value(name = "sweet")]
    Sweet,

    #[value(name = "temple")]
    Temple,

    #[value(name = "None")]
    None,
}

#[derive(Debug, ValueEnum, Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Serialize, Deserialize)]
pub enum DMSetup {
    #[value(name = "gdm")]
    Gdm,

    #[value(name = "lightdm-neon")]
    LightDMNeon,

    #[value(name = "ly")]
    Ly,

    #[value(name = "sddm")]
    Sddm,

    #[value(name = "None")]
    None,
}

#[derive(Debug, ValueEnum, Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Serialize, Deserialize)]
pub enum ShellSetup {
    #[value(name = "bash")]
    Bash,

    #[value(name = "fish")]
    Fish,

    #[value(name = "zsh")]
    Zsh,

    #[value(name = "None")]
    None,
}