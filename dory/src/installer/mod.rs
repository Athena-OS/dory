use std::{
  collections::VecDeque,
  fmt::{Debug, Display},
  io::{Read, Write},
  net::TcpStream,
  path::{Path, PathBuf},
  process::{Command, Stdio},
  sync::mpsc::{self, Receiver},
  thread,
  time::{Duration, Instant},
};

use ansi_to_tui::IntoText;
use log::debug;
use ratatui::{
  Frame,
  crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
  layout::{Constraint, Direction, Layout, Rect},
  prelude::Alignment,
  style::{Color, Modifier, Style},
  text::{Line, Span},
  widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json, to_string_pretty};
use shared::keyboard::{KEYMAPS, resolve};
use tempfile::NamedTempFile;

use crate::{
  drives::{Disk, DiskItem, part_table},
  installer::users::User,
  split_hor, split_vert, styled_block, ui_back, ui_close, ui_down, ui_enter, ui_left, ui_right,
  ui_up,
  widget::{
    Button, ConfigWidget, FancyTicker, HelpModal, InfoBox, InstallSteps, LineEditor, LogBox,
    StrList, WidgetBox, WidgetBoxBuilder,
  },
};

const HIGHLIGHT: Option<(Color, Modifier)> = Some((Color::Yellow, Modifier::BOLD));
/// Required by `mokutil -P` (see `RootPassword::mkpasswd`).
pub const ROOT_HASH_METHOD: &str = "sha512crypt";
/// Modern default for user accounts; nothing else needs to parse these.
pub const USER_HASH_METHOD: &str = "yescrypt";
const KEY: Option<(Color, Modifier)> = Some((Color::Yellow, Modifier::BOLD));

/// One styled line as consumed by [`styled_block`]: `(style, text)` spans.
type StyledLine = Vec<(Option<(Color, Modifier)>, String)>;

pub mod drivepages;
mod infos;
pub mod users;
use drivepages::Drives;
use users::UserAccounts;

/// Add syntax highlighting to JSON using `bat`; falls back to plain text when
/// bat is unavailable.
fn highlight_json(content: &str) -> String {
  let spawn = Command::new("bat")
    .args(["-p", "-f", "-l", "json"])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .spawn();
  let Ok(mut child) = spawn else {
    return content.to_string();
  };
  if let Some(mut stdin) = child.stdin.take() {
    let _ = stdin.write_all(content.as_bytes());
  }
  match child.wait_with_output() {
    Ok(out) if out.status.success() => {
      String::from_utf8(out.stdout).unwrap_or_else(|_| content.to_string())
    }
    _ => content.to_string(),
  }
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Installer {
  pub basesystem: Option<String>,
  pub keyboard_layout: Option<String>,
  pub locale: Option<String>,
  pub root_passwd_hash: Option<String>, // Hashed
  pub users: Vec<User>,
  pub hostname: Option<String>,
  pub desktop_environment: Option<String>,
  pub design: Option<String>,
  pub display_manager: Option<String>,
  pub timezone: Option<String>,
  pub extra_packages: Vec<String>,
  pub repo_synced: bool,
  pub cached_repo_pkgs: Option<Vec<String>>,

  pub drives: Vec<Disk>,
  pub swap: Option<u64>,

  pub drive_config: Option<Disk>,
  pub use_auto_drive_config: bool,

  pub drive_config_display: Option<Vec<DiskItem>>,
}

impl Installer {
  pub fn new() -> Self {
    Self::default()
  }

  /// Everything the backend `Config` needs that is still unset. This is the
  /// single source of truth for both the "Done" button and the requirements box.
  pub fn missing_requirements(&self) -> Vec<&'static str> {
    let mut missing = Vec::new();
    match self.basesystem.as_deref() {
      None => missing.push("Base System"),
      // systemd-boot only: there is no legacy BIOS bootloader path.
      Some("Athena Arch") if !shared::partition::is_uefi() => {
        missing.push("UEFI boot mode (Athena Arch needs UEFI; reboot the live ISO in UEFI mode)");
      }
      Some(_) => {}
    }
    if self.timezone.is_none() {
      missing.push("Timezone");
    }
    if self.keyboard_layout.is_none() {
      missing.push("Keyboard Layout");
    }
    if self.locale.is_none() {
      missing.push("Locale");
    }
    if self.drives.is_empty() || self.drive_config.is_none() {
      missing.push("Drive Configuration");
    }
    if self.hostname.is_none() {
      missing.push("Hostname");
    }
    if self.root_passwd_hash.is_none() {
      missing.push("Root Password");
    }
    if self.users.is_empty() {
      missing.push("At least one User Account");
    }
    if self.desktop_environment.is_none() {
      missing.push("Desktop Environment");
    }
    if self.display_manager.is_none() {
      missing.push("Display Manager");
    }
    if self.design.is_none() {
      missing.push("Design");
    }
    missing
  }

  pub fn has_all_requirements(&self) -> bool {
    self.missing_requirements().is_empty()
  }

  pub fn make_drive_config_display(&mut self) {
    self.drive_config_display = self.drive_config.as_ref().map(|d| d.layout().to_vec());
  }

  /// Intermediate JSON representation handed to the installer backend.
  pub fn build_config_json(&mut self) -> Value {
    let sys_config = json!({
      "base": self.basesystem,
      "hostname": self.hostname,
      "keyboard_layout": self.keyboard_layout,
      "locale": self.locale,
      "timezone": self.timezone,
      "root_passwd_hash": self.root_passwd_hash,
      "desktop_environment": self.desktop_environment.as_ref().map(|s| s.to_lowercase()),
      "design": self.design.as_ref().map(|s| s.to_lowercase()),
      "display_manager": self.display_manager.as_ref().map(|s| s.to_lowercase()),
      "users": self.users,
      "extra_packages": self.extra_packages,
      // Installed with speech: the new system speaks too.
      "accessibility": crate::a11y::speaking(),
    });
    let drv_cfg = self.drive_config.as_mut().map(|d| d.as_disko_cfg());
    json!({ "config": sys_config, "drives": drv_cfg })
  }

  /// Write the system and drive configuration to two private temp files for
  /// the backend process. The files contain password hashes, so they are
  /// created with 0600 (tempfile's default) and deleted when dropped.
  pub fn write_config_files(&mut self) -> anyhow::Result<(NamedTempFile, NamedTempFile)> {
    let Value::Object(mut map) = self.build_config_json() else {
      unreachable!()
    };
    let system_v = map.remove("config").unwrap_or(Value::Null);
    let drives_v = map.remove("drives").unwrap_or(Value::Null);

    let mut system_cfg = NamedTempFile::new()?;
    serde_json::to_writer_pretty(&mut system_cfg, &json!({ "config": system_v }))?;
    system_cfg.write_all(b"\n")?;
    system_cfg.flush()?;
    debug!("Wrote system config at {}", system_cfg.path().display());

    let mut disko_cfg = NamedTempFile::new()?;
    serde_json::to_writer_pretty(&mut disko_cfg, &json!({ "drives": drives_v }))?;
    disko_cfg.write_all(b"\n")?;
    disko_cfg.flush()?;
    debug!("Wrote drives config at {}", disko_cfg.path().display());

    Ok((system_cfg, disko_cfg))
  }
}

pub enum Signal {
  Wait,
  Push(Box<dyn Page>),
  Pop,
  PopCount(usize),
  Quit,
  WriteCfg,
  Unwind,               // Pop until we get back to the menu
  Error(anyhow::Error), // Propagates errors
}

impl Debug for Signal {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Wait => write!(f, "Signal::Wait"),
      Self::Push(_) => write!(f, "Signal::Push"),
      Self::Pop => write!(f, "Signal::Pop"),
      Self::PopCount(n) => write!(f, "Signal::PopCount({n})"),
      Self::Quit => write!(f, "Signal::Quit"),
      Self::WriteCfg => write!(f, "Signal::WriteCfg"),
      Self::Unwind => write!(f, "Signal::Unwind"),
      Self::Error(err) => write!(f, "Signal::Error({err})"),
    }
  }
}

pub trait Page {
  fn render(&mut self, installer: &mut Installer, f: &mut Frame, area: Rect);
  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal;

  /// Pages that animate or poll background work return `true` so the event
  /// loop redraws them periodically. Everything else only redraws on input.
  fn needs_tick(&self) -> bool {
    false
  }

  /// Signal emitted without user input (e.g. installation finished). Called
  /// after every redraw.
  fn signal(&self) -> Option<Signal> {
    None
  }
}

/// Help-modal line: `key - description`.
fn help_line(
  key: &'static str,
  what: &'static str,
) -> Vec<(Option<(Color, Modifier)>, &'static str)> {
  vec![(KEY, key), (None, what)]
}

/// Common help modal for list pages.
fn list_help(
  title: &str,
  noun: &'static str,
  blurb: &'static str,
  searchable: bool,
) -> HelpModal<'static> {
  let mut lines = vec![
    help_line("↑/↓, j/k", " - Navigate "),
    vec![(None, noun)],
    help_line("Enter", " - Select and return"),
  ];
  if searchable {
    lines.push(help_line("/", " - Focus search bar"));
  }
  lines.extend([
    help_line("Esc, q, ←, h", " - Cancel and return to menu"),
    help_line("?", " - Show this help"),
    vec![(None, "")],
    vec![(None, blurb)],
  ]);
  HelpModal::new(title, styled_block(lines))
}

fn current_value_box(label: &str, value: &str) -> Option<Box<dyn ConfigWidget>> {
  Some(Box::new(InfoBox::new(
    "",
    styled_block(vec![
      vec![(None, label.to_string())],
      vec![(HIGHLIGHT, value.to_string())],
    ]),
  )))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuPages {
  BaseSys,
  Timezone,
  KeyboardLayout,
  Locale,
  Drives,
  Hostname,
  RootPassword,
  UserAccounts,
  DesktopEnvironment,
  DisplayManager,
  Design,
  ExtraPackages,
}

impl MenuPages {
  pub fn supported_pages() -> &'static [MenuPages] {
    &[
      MenuPages::BaseSys,
      MenuPages::Timezone,
      MenuPages::KeyboardLayout,
      MenuPages::Locale,
      MenuPages::Drives,
      MenuPages::Hostname,
      MenuPages::RootPassword,
      MenuPages::UserAccounts,
      MenuPages::DesktopEnvironment,
      MenuPages::DisplayManager,
      MenuPages::Design,
      MenuPages::ExtraPackages,
    ]
  }
}

impl Display for MenuPages {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let s = match self {
      MenuPages::BaseSys => "Base System",
      MenuPages::Timezone => "Timezone",
      MenuPages::KeyboardLayout => "Keyboard Layout",
      MenuPages::Locale => "Locale",
      MenuPages::Drives => "Drives",
      MenuPages::Hostname => "Hostname",
      MenuPages::RootPassword => "Root Password",
      MenuPages::UserAccounts => "User Accounts",
      MenuPages::DesktopEnvironment => "Desktop Environment",
      MenuPages::DisplayManager => "Display Manager",
      MenuPages::Design => "Design",
      MenuPages::ExtraPackages => "Extra Packages",
    };
    write!(f, "{s}")
  }
}

impl MenuPages {
  /// Widget showing the current value of this option, if set.
  pub fn display_widget(self, installer: &mut Installer) -> Option<Box<dyn ConfigWidget>> {
    match self {
      MenuPages::BaseSys => ListKind::BaseSys.display_widget(installer),
      MenuPages::Timezone => ListKind::Timezone.display_widget(installer),
      MenuPages::KeyboardLayout => ListKind::KeyboardLayout.display_widget(installer),
      MenuPages::Locale => ListKind::Locale.display_widget(installer),
      MenuPages::Drives => {
        let sector_size = installer
          .drive_config
          .as_ref()
          .map(|d| d.sector_size())
          .unwrap_or(512);
        installer
          .drive_config_display
          .as_deref()
          .map(|d| Box::new(part_table(d, sector_size)) as Box<dyn ConfigWidget>)
      }
      MenuPages::Hostname => Hostname::display_widget(installer),
      MenuPages::RootPassword => RootPassword::display_widget(installer),
      MenuPages::UserAccounts => UserAccounts::display_widget(installer),
      MenuPages::DesktopEnvironment => ListKind::DesktopEnvironment.display_widget(installer),
      MenuPages::DisplayManager => ListKind::DisplayManager.display_widget(installer),
      MenuPages::Design => ListKind::Design.display_widget(installer),
      MenuPages::ExtraPackages => ExtraPackages::display_widget(installer),
    }
  }

  /// Title and description shown in the main menu for this page.
  pub fn page_info(self) -> (String, Vec<Line<'static>>) {
    match self {
      MenuPages::BaseSys => base_sys_page_info(),
      MenuPages::Timezone => timezone_page_info(),
      MenuPages::KeyboardLayout => keyboard_page_info(),
      MenuPages::Locale => locale_page_info(),
      MenuPages::Drives => (
        "Drives".to_string(),
        styled_block(vec![
          vec![(
            None,
            "Select and configure the drives for your Athena OS installation.",
          )],
          vec![(
            None,
            "This includes partitioning, formatting, and mount points.",
          )],
          vec![(
            None,
            "If you have already configured a drive, its current configuration will be shown below.",
          )],
        ]),
      ),
      MenuPages::Hostname => Hostname::page_info(),
      MenuPages::RootPassword => RootPassword::page_info(),
      MenuPages::UserAccounts => UserAccounts::page_info(),
      MenuPages::DesktopEnvironment => desktop_page_info(),
      MenuPages::DisplayManager => display_manager_page_info(),
      MenuPages::Design => design_page_info(),
      MenuPages::ExtraPackages => ExtraPackages::page_info(),
    }
  }

  /// Navigate to the page - returns a Signal to push the appropriate page
  pub fn navigate(self, installer: &mut Installer) -> Signal {
    let page: Box<dyn Page> = match self {
      MenuPages::BaseSys => Box::new(ListPage::new(ListKind::BaseSys, installer)),
      MenuPages::Timezone => Box::new(ListPage::new(ListKind::Timezone, installer)),
      MenuPages::KeyboardLayout => Box::new(ListPage::new(ListKind::KeyboardLayout, installer)),
      MenuPages::Locale => Box::new(ListPage::new(ListKind::Locale, installer)),
      MenuPages::Drives => Box::new(Drives::new()),
      MenuPages::Hostname => Box::new(Hostname::new()),
      MenuPages::RootPassword => Box::new(RootPassword::new()),
      MenuPages::UserAccounts => Box::new(UserAccounts::new(installer.users.clone())),
      MenuPages::DesktopEnvironment => {
        Box::new(ListPage::new(ListKind::DesktopEnvironment, installer))
      }
      MenuPages::DisplayManager => Box::new(ListPage::new(ListKind::DisplayManager, installer)),
      MenuPages::Design => Box::new(ListPage::new(ListKind::Design, installer)),
      MenuPages::ExtraPackages => Box::new(ExtraPackages::new()),
    };
    Signal::Push(page)
  }
}

/// The main menu page
pub struct Menu {
  menu_items: StrList,
  border_flash_timer: u32,
  button_row: WidgetBox,
  help_modal: HelpModal<'static>,
}

impl Menu {
  pub fn new() -> Self {
    let items = MenuPages::supported_pages()
      .iter()
      .map(|p| p.to_string())
      .collect::<Vec<_>>();
    let mut menu_items = StrList::new("Main Menu", items);
    let buttons: Vec<Box<dyn ConfigWidget>> = vec![
      Box::new(Button::new("Done")),
      Box::new(Button::new("Abort")),
    ];
    let button_row = WidgetBoxBuilder::new().children(buttons).build();
    menu_items.focus();
    let help_content = styled_block(vec![
      help_line("↑/↓, j/k", " - Navigate menu options"),
      help_line("Enter", " - Select and configure option"),
      help_line("Tab, End, G", " - Move to action buttons"),
      help_line("Home, g", " - Return to menu options"),
      help_line("q", " - Quit installer"),
      help_line("?", " - Show this help"),
      vec![(None, "")],
      vec![(
        None,
        "Required options are shown in red when not configured.",
      )],
      vec![(None, "Configure all required options before proceeding.")],
    ]);
    let help_modal = HelpModal::new("Main Menu", help_content);
    Self {
      menu_items,
      button_row,
      help_modal,
      border_flash_timer: 0,
    }
  }
  pub fn info_box_for_item(&mut self, installer: &mut Installer, idx: usize) -> WidgetBox {
    // Get the actual page from supported_pages using the index
    let supported_pages = MenuPages::supported_pages();
    let page = supported_pages.get(idx).copied();

    let (display_widget, title, content) = if let Some(page) = page {
      let display_widget = page.display_widget(installer);
      let (title, content) = page.page_info();
      (display_widget, title, content)
    } else {
      (
        None,
        "Unknown Option".to_string(),
        styled_block(vec![vec![(
          None,
          "No information available for this option.",
        )]]),
      )
    };
    let mut info_box = Box::new(InfoBox::new(title, content));
    if self.border_flash_timer > 0 {
      match self.border_flash_timer % 2 {
        1 => info_box.highlighted(true),
        0 => info_box.highlighted(false),
        _ => unreachable!(),
      }
      self.border_flash_timer -= 1;
    }
    if let Some(widget) = display_widget {
      WidgetBoxBuilder::new()
        .layout(
          Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)].as_ref()),
        )
        .children(vec![info_box, widget])
        .build()
    } else {
      WidgetBoxBuilder::new().children(vec![info_box]).build()
    }
  }
  pub fn remaining_requirements(
    &self,
    installer: &mut Installer,
    border_flash_timer: u32,
  ) -> InfoBox<'static> {
    let missing = installer.missing_requirements();
    let mut lines: Vec<StyledLine> = Vec::new();
    if missing.is_empty() {
      lines.push(vec![(
        Some((Color::Green, Modifier::BOLD)),
        "All required options have been configured!".into(),
      )]);
    } else {
      lines.push(vec![(
        None,
        "The following required options are not yet configured:".into(),
      )]);
      lines.extend(
        missing
          .iter()
          .map(|m| vec![(Some((Color::Red, Modifier::BOLD)), format!(" - {m}"))]),
      );
      lines.push(vec![(
        None,
        "Please configure them before proceeding.".into(),
      )]);
    }

    let mut info_box = InfoBox::new("Required Config", styled_block(lines));
    if border_flash_timer > 0 {
      match self.border_flash_timer % 2 {
        1 => info_box.highlighted(true),
        0 => info_box.highlighted(false),
        _ => unreachable!(),
      }
    }
    info_box
  }
}

impl Default for Menu {
  fn default() -> Self {
    Self::new()
  }
}

impl Page for Menu {
  fn render(&mut self, installer: &mut Installer, f: &mut Frame, area: Rect) {
    let chunks = split_hor!(
      area,
      1,
      [Constraint::Percentage(20), Constraint::Percentage(80)].as_ref()
    );

    // We use this for both the menu options and info box
    // so that it looks visually consistent :)
    let split_space = |layout: Layout, chunk: Rect| {
      layout
        .direction(Direction::Vertical)
        .constraints(
          [
            Constraint::Percentage(95), // Main content
            Constraint::Percentage(5),  // Footer
          ]
          .as_ref(),
        )
        .split(chunk)
    };

    let left_chunks = split_space(Layout::default(), chunks[0]);

    let right_chunks = split_space(Layout::default(), chunks[1]);

    self.menu_items.render(f, left_chunks[0]);
    self.button_row.render(f, left_chunks[1]);
    let border_flash_timer = self.border_flash_timer;
    let decrement_timer = border_flash_timer > 0;
    {
      let info_box: Box<dyn ConfigWidget> = if self.menu_items.is_focused() {
        Box::new(self.info_box_for_item(installer, self.menu_items.selected_idx))
          as Box<dyn ConfigWidget>
      } else {
        Box::new(self.remaining_requirements(installer, border_flash_timer))
          as Box<dyn ConfigWidget>
      };

      info_box.render(f, right_chunks[0]);

      // Render help modal on top of everything
      self.help_modal.render(f, area);
    }
    {
      if decrement_timer {
        self.border_flash_timer -= 1;
      }
    }
  }

  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Char('?') => {
        self.help_modal.toggle();
        Signal::Wait
      }
      ui_close!() if self.help_modal.visible => {
        self.help_modal.hide();
        Signal::Wait
      }
      _ if self.help_modal.visible => {
        // Help modal is open, don't process other inputs
        Signal::Wait
      }
      KeyCode::Char('q') => Signal::Quit,
      KeyCode::Home | KeyCode::Char('g') => {
        if self.menu_items.is_focused() {
          self.menu_items.first_item();
          Signal::Wait
        } else {
          self.menu_items.first_item();
          self.menu_items.focus();
          self.button_row.unfocus();
          Signal::Wait
        }
      }
      KeyCode::End | KeyCode::Char('G') => {
        if self.menu_items.is_focused() {
          self.button_row.focus();
          self.menu_items.unfocus();
        }
        Signal::Wait
      }
      ui_up!() => {
        if self.menu_items.is_focused() {
          if !self.menu_items.previous_item() {
            self.menu_items.unfocus();
            self.button_row.focus();
          }
          Signal::Wait
        } else {
          self.menu_items.last_item();
          self.menu_items.focus();
          self.button_row.unfocus();
          Signal::Wait
        }
      }
      ui_down!() => {
        if self.menu_items.is_focused() {
          if !self.menu_items.next_item() {
            self.menu_items.unfocus();
            self.button_row.focus();
          }
          Signal::Wait
        } else {
          self.menu_items.first_item();
          self.menu_items.focus();
          self.button_row.unfocus();
          Signal::Wait
        }
      }
      #[allow(unreachable_patterns)]
      ui_enter!() if self.menu_items.is_focused() => {
        let idx = self.menu_items.selected_idx;
        // Get the actual page from supported_pages using the index
        let supported_pages = MenuPages::supported_pages();
        if let Some(page) = supported_pages.get(idx).copied() {
          page.navigate(installer)
        } else {
          Signal::Wait
        }
      }
      // Button row
      ui_right!() => {
        if self.button_row.is_focused() {
          self.button_row.next_child();
        }
        Signal::Wait
      }
      ui_left!() => {
        if self.button_row.is_focused() {
          self.button_row.prev_child();
        }
        Signal::Wait
      }
      KeyCode::Enter => {
        if self.button_row.is_focused() {
          match self.button_row.selected_child() {
            Some(0) => {
              // Done - Show config preview
              if installer.has_all_requirements() {
                Signal::Push(Box::new(ConfigPreview::new(installer)))
              } else {
                self.border_flash_timer = 6;
                // The refusal is only a flashing border on screen.
                crate::a11y::alert(|| {
                  format!(
                    "Cannot continue. Not configured yet: {}",
                    installer.missing_requirements().join(", ")
                  )
                });
                Signal::Wait
              }
            }
            Some(1) => Signal::Quit, // Abort
            _ => Signal::Wait,
          }
        } else {
          self.menu_items.focus();
          Signal::Wait
        }
      }
      _ => Signal::Wait,
    }
  }
}

/// Which option a [`ListPage`] edits. Everything page-specific lives here, the
/// rendering and key handling are shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListKind {
  BaseSys,
  Timezone,
  KeyboardLayout,
  Locale,
  DesktopEnvironment,
  DisplayManager,
  Design,
}

impl ListKind {
  fn title(self) -> &'static str {
    match self {
      ListKind::BaseSys => "Select Base System",
      ListKind::Timezone => "Select Timezone",
      ListKind::KeyboardLayout => "Select Keyboard Layout",
      ListKind::Locale => "Select Locale",
      ListKind::DesktopEnvironment => "Select Desktop Environment",
      ListKind::DisplayManager => "Select Display Manager",
      ListKind::Design => "Select Design",
    }
  }

  /// Long lists get a fuzzy search bar; short ones get an info box instead.
  fn searchable(self) -> bool {
    matches!(
      self,
      ListKind::Timezone | ListKind::KeyboardLayout | ListKind::Locale
    )
  }

  fn items(self, installer: &Installer) -> Vec<String> {
    let strs: Vec<&str> = match self {
      ListKind::BaseSys => vec!["Athena Arch", "Athena Nix"],
      ListKind::Timezone => return list_timezones(),
      ListKind::KeyboardLayout => KEYMAPS.iter().map(|k| k.label).collect(),
      ListKind::Locale => infos::LOCALES.to_vec(),
      ListKind::DesktopEnvironment => match installer.basesystem.as_deref() {
        Some("Athena Nix") => vec!["GNOME", "MATE", "Cinnamon", "None"],
        _ => vec![
          "GNOME",
          "KDE Plasma",
          "Hyprland",
          "XFCE",
          "Cinnamon",
          "MATE",
          "Bspwm",
          "None",
        ],
      },
      ListKind::DisplayManager => vec![
        "Ly",
        "Astronaut",
        "Black Hole",
        "Cyberpunk",
        "Cyborg",
        "Kath",
        "Jake The Dog",
        "Pixel Sakura",
        "Post-Apocalypse",
        "Purple Leaves",
        "None",
      ],
      ListKind::Design => vec![
        "Cyborg",
        "Frost",
        "Graphite",
        "HackTheBox",
        "RedMoon",
        "Samurai",
        "Sweet",
        "Temple",
        "None",
      ],
    };
    strs.into_iter().map(str::to_string).collect()
  }

  /// Info box for the highlighted item (non-searchable pages only).
  fn info(self, item: &str) -> Option<InfoBox<'static>> {
    Some(match self {
      ListKind::BaseSys => infos::base_system_info(item),
      ListKind::DesktopEnvironment => infos::desktop_info(item),
      ListKind::DisplayManager => infos::display_manager_info(item),
      ListKind::Design => infos::design_info(item),
      _ => return None,
    })
  }

  fn help(self) -> HelpModal<'static> {
    let (title, noun, blurb) = match self {
      ListKind::BaseSys => (
        "Base System",
        "base system options",
        "Select the base system for your environment.",
      ),
      ListKind::Timezone => (
        "Timezone",
        "timezone options",
        "Select the timezone that matches your physical location.",
      ),
      ListKind::KeyboardLayout => (
        "Keyboard Layout",
        "keyboard layout options",
        "Choose the keyboard layout that matches your physical keyboard.",
      ),
      ListKind::Locale => (
        "Locale",
        "locale options",
        "Set the locale for your system, which determines language and regional settings.",
      ),
      ListKind::DesktopEnvironment => (
        "Desktop Environment",
        "desktop environment options",
        "Select the desktop environment for your graphical interface.",
      ),
      ListKind::DisplayManager => (
        "Display Manager",
        "display manager options",
        "Select the display manager for your graphical interface.",
      ),
      ListKind::Design => (
        "Design",
        "design options",
        "Select the design for your graphical interface.",
      ),
    };
    list_help(title, noun, blurb, self.searchable())
  }

  /// Spoken once when the page opens during a spoken install: which choices
  /// keep speech working on the installed system.
  fn speech_note(self, installer: &Installer) -> Option<&'static str> {
    let nix = installer.basesystem.as_deref() == Some("Athena Nix");
    match self {
      ListKind::BaseSys => Some(
        "Note: speech on the installed system is set up on Athena Arch. \
         Athena Nix does not speak after installation yet.",
      ),
      ListKind::DesktopEnvironment if !nix => Some(
        "Note: the Orca screen reader starts automatically after login on GNOME, \
         KDE Plasma, XFCE, Cinnamon and MATE. Hyprland and Bspwm do not start it.",
      ),
      ListKind::DisplayManager if !nix => Some(
        "Note: only Ly, a text login, is read aloud. The other login screens are \
         graphical and silent, and Orca starts after you log in.",
      ),
      _ => None,
    }
  }

  /// Store the chosen item.
  fn commit(self, installer: &mut Installer, item: &str) {
    let value = Some(item.to_string());
    match self {
      ListKind::BaseSys => {
        installer.basesystem = value;
        installer.extra_packages.clear(); // Extra packages depend on the base system
      }
      ListKind::Timezone => installer.timezone = value,
      ListKind::KeyboardLayout => {
        installer.keyboard_layout = KEYMAPS
          .iter()
          .find(|k| k.label == item)
          .map(|k| k.id.to_string());
      }
      ListKind::Locale => installer.locale = value,
      ListKind::DesktopEnvironment => installer.desktop_environment = value,
      ListKind::DisplayManager => installer.display_manager = value,
      ListKind::Design => installer.design = value,
    }
  }

  fn display_widget(self, installer: &Installer) -> Option<Box<dyn ConfigWidget>> {
    let (label, value) = match self {
      ListKind::BaseSys => ("Current base system set to:", installer.basesystem.clone()?),
      ListKind::Timezone => ("Current timezone set to:", installer.timezone.clone()?),
      ListKind::KeyboardLayout => {
        let id = installer.keyboard_layout.as_deref()?;
        // try to resolve; fall back to the raw id if not found
        (
          "Current keyboard layout set to:",
          resolve(id)
            .map(|k| k.label.to_string())
            .unwrap_or_else(|| id.to_string()),
        )
      }
      ListKind::Locale => ("Current locale set to:", installer.locale.clone()?),
      ListKind::DesktopEnvironment => (
        "Current desktop environment set to:",
        installer.desktop_environment.clone()?,
      ),
      ListKind::DisplayManager => (
        "Current display manager set to:",
        installer.display_manager.clone()?,
      ),
      ListKind::Design => ("Current design set to:", installer.design.clone()?),
    };
    current_value_box(label, &value)
  }
}

fn list_timezones() -> Vec<String> {
  match Command::new("timedatectl").arg("list-timezones").output() {
    Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
      .lines()
      .map(str::trim)
      .filter(|s| !s.is_empty())
      .map(str::to_string)
      .collect(),
    _ => Vec::new(),
  }
}

/// Generic "pick one item from a list" page, optionally with a fuzzy search
/// bar (long lists) or an info box describing the highlighted item.
pub struct ListPage {
  kind: ListKind,
  list: StrList,
  search_bar: LineEditor,
  search_focused: bool,
  help_modal: HelpModal<'static>,
}

impl ListPage {
  pub fn new(kind: ListKind, installer: &Installer) -> Self {
    let mut list = StrList::new(kind.title(), kind.items(installer));
    let mut search_bar = LineEditor::new("Search (press '/' to focus)", Some("Type to filter..."));
    let search_focused = kind.searchable();
    if search_focused {
      search_bar.focus();
    } else {
      list.focus();
    }
    if let Some(note) = kind.speech_note(installer) {
      crate::a11y::alert(|| note.to_string());
    }
    Self {
      kind,
      list,
      search_bar,
      search_focused,
      help_modal: kind.help(),
    }
  }

  fn focus_list(&mut self) {
    self.search_bar.unfocus();
    self.search_focused = false;
    self.list.focus();
  }

  fn selected(&self) -> Option<String> {
    self.list.selected_item().cloned()
  }
}

impl Page for ListPage {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    if self.kind.searchable() {
      let chunks = split_vert!(area, 0, [Constraint::Length(3), Constraint::Min(0)]);
      self.search_bar.render(f, chunks[0]);
      self.list.render(f, chunks[1]);
    } else {
      let list_pct = if self.kind == ListKind::BaseSys {
        30
      } else {
        50
      };
      let vert = split_vert!(
        area,
        0,
        [
          Constraint::Percentage(list_pct),
          Constraint::Percentage(100 - list_pct)
        ]
      );
      let hor = split_hor!(
        vert[0],
        1,
        [
          Constraint::Percentage(38),
          Constraint::Length(28),
          Constraint::Percentage(38)
        ]
      );
      self.list.render(f, hor[1]);
      if let Some(info) = self.selected().and_then(|item| self.kind.info(&item)) {
        info.render(f, vert[1]);
      }
    }
    self.help_modal.render(f, area);
  }

  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Char('?') => {
        self.help_modal.toggle();
        return Signal::Wait;
      }
      ui_close!() if self.help_modal.visible => {
        self.help_modal.hide();
        return Signal::Wait;
      }
      _ if self.help_modal.visible => return Signal::Wait,
      ui_back!() if !self.search_focused => return Signal::Pop,
      KeyCode::Char('/') if self.kind.searchable() && !self.search_focused => {
        self.search_focused = true;
        self.search_bar.focus();
        self.search_bar.clear();
        self.list.unfocus();
        return Signal::Wait;
      }
      _ => {}
    }

    if self.search_focused {
      match event.code {
        KeyCode::Esc => {
          self.search_bar.clear();
          self.list.set_filter(None::<String>);
          self.focus_list();
        }
        KeyCode::Enter | KeyCode::Tab | KeyCode::Down => self.focus_list(),
        _ => {
          self.search_bar.handle_input(event);
          self.list.set_filter(Some(self.search_bar.value()));
        }
      }
      return Signal::Wait;
    }

    match event.code {
      KeyCode::Enter => {
        if let Some(item) = self.selected() {
          self.kind.commit(installer, &item);
          return Signal::Pop;
        }
        Signal::Wait
      }
      ui_up!() => {
        if !self.list.previous_item() {
          self.list.last_item();
        }
        Signal::Wait
      }
      ui_down!() => {
        if !self.list.next_item() {
          self.list.first_item();
        }
        Signal::Wait
      }
      _ => self.list.handle_input(event),
    }
  }
}

fn base_sys_page_info() -> (String, Vec<Line<'static>>) {
  (
    "Base System".to_string(),
    styled_block(vec![
      vec![(
        None,
        "Select the base system to be installed on your environment.",
      )],
      vec![(
        None,
        "The base system defines the foundation of your environment-providing the package manager, update model, and security features that shape how your system evolves and performs over time.",
      )],
      vec![(
        None,
        "Choosing a base system lets you tailor the balance between stability, flexibility, and reproducibility-ensuring the platform matches your workflow, preferences, and long-term goals.",
      )],
    ]),
  )
}

fn timezone_page_info() -> (String, Vec<Line<'static>>) {
  (
    "Timezone".to_string(),
    styled_block(vec![
      vec![(None, "Select the timezone for your system.")],
      vec![(
        None,
        "The timezone setting determines the local time displayed on your system and is important for scheduling tasks and logging events.",
      )],
      vec![(
        None,
        "Choose a timezone that matches your physical location or the location where the system will primarily be used.",
      )],
    ]),
  )
}

fn keyboard_page_info() -> (String, Vec<Line<'static>>) {
  (
    "Keyboard Layout".to_string(),
    styled_block(vec![vec![(
      None,
      "Choose the keyboard layout that matches your physical keyboard.",
    )]]),
  )
}

fn locale_page_info() -> (String, Vec<Line<'static>>) {
  (
    "Locale".to_string(),
    styled_block(vec![vec![(
      None,
      "Set the locale for your system, which determines language and regional settings.",
    )]]),
  )
}

fn desktop_page_info() -> (String, Vec<Line<'static>>) {
  (
    "Desktop Environment".to_string(),
    styled_block(vec![
      vec![(
        None,
        "Select the desktop environment to be installed on your system.",
      )],
      vec![(
        None,
        "The desktop environment provides the graphical user interface (GUI) for your system, including the window manager, panels, and application launchers.",
      )],
      vec![(
        None,
        "Choosing a desktop environment can help tailor the user experience to your preferences and workflow.",
      )],
    ]),
  )
}

fn display_manager_page_info() -> (String, Vec<Line<'static>>) {
  (
    "Display Manager".to_string(),
    styled_block(vec![
      vec![(
        None,
        "Select the display manager to be installed on your system.",
      )],
      vec![(
        None,
        "The display manager provides the visual presentation of your login screen, including the backgrounds, branding, and session controls.",
      )],
      vec![(
        None,
        "Choosing a display manager can help tailor the login experience to your preferences and accessibility needs.",
      )],
    ]),
  )
}

fn design_page_info() -> (String, Vec<Line<'static>>) {
  (
    "Design".to_string(),
    styled_block(vec![
      vec![(None, "Select the design to be installed on your system.")],
      vec![(
        None,
        "The design defines the visual style of your system, expressed through wallpapers, colors, and artistic themes that shape its overall atmosphere and aesthetic identity.",
      )],
      vec![(
        None,
        "Choosing a design allows you to personalize the look and feel of your system, reflecting your style and creating the atmosphere you prefer.",
      )],
    ]),
  )
}

pub struct Hostname {
  input: LineEditor,
  help_modal: HelpModal<'static>,
}

impl Hostname {
  pub fn new() -> Self {
    let mut input = LineEditor::new("Set Hostname", Some("e.g. 'my-computer'"));
    input.focus();
    let help_content = styled_block(vec![
      help_line("Enter", " - Save hostname and return"),
      help_line("Esc", " - Cancel and return to menu"),
      help_line("←/→", " - Move cursor"),
      help_line("Home/End", " - Jump to beginning/end"),
      help_line("Backspace/Del", " - Delete characters"),
      help_line("?", " - Show this help"),
      vec![(None, "")],
      vec![(
        None,
        "Set a unique hostname for your computer on the network.",
      )],
    ]);
    let help_modal = HelpModal::new("Hostname", help_content);
    Self { input, help_modal }
  }
  pub fn display_widget(installer: &mut Installer) -> Option<Box<dyn ConfigWidget>> {
    current_value_box("Current hostname set to:", installer.hostname.as_deref()?)
  }

  /// RFC 1123 host label: 1-63 chars, letters/digits/hyphens, no leading or
  /// trailing hyphen. Dots are allowed to form a FQDN.
  fn validate(hostname: &str) -> Result<(), &'static str> {
    if hostname.is_empty() {
      return Err("Hostname cannot be empty");
    }
    if hostname.len() > 253 {
      return Err("Hostname is too long (max 253 characters)");
    }
    for label in hostname.split('.') {
      if label.is_empty() || label.len() > 63 {
        return Err("Each part of the hostname must be 1-63 characters");
      }
      if !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err("Only letters, digits and hyphens are allowed");
      }
      if label.starts_with('-') || label.ends_with('-') {
        return Err("Hostname parts cannot start or end with a hyphen");
      }
    }
    Ok(())
  }
  pub fn page_info() -> (String, Vec<Line<'static>>) {
    (
      "Hostname".to_string(),
      styled_block(vec![
        vec![(
          None,
          "The hostname is a unique identifier for your computer on a network.",
        )],
        vec![(
          None,
          "It is used to distinguish your computer from other devices and can be helpful for network management and troubleshooting.",
        )],
        vec![(
          None,
          "Choose a hostname that is easy to remember and reflects the purpose or identity of your computer.",
        )],
      ]),
    )
  }
}

impl Default for Hostname {
  fn default() -> Self {
    Self::new()
  }
}

impl Page for Hostname {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let chunks = split_vert!(
      area,
      1,
      [
        Constraint::Percentage(40),
        Constraint::Length(5),
        Constraint::Percentage(40),
      ]
    );
    let hor_chunks = split_hor!(
      chunks[1],
      0,
      [
        Constraint::Percentage(10),
        Constraint::Percentage(80),
        Constraint::Percentage(10),
      ]
    );

    let info_box = InfoBox::new(
      "",
      styled_block(vec![
        vec![(
          None,
          "The hostname is a unique identifier for your computer on a network.",
        )],
        vec![(
          None,
          "It is used to distinguish your computer from other devices and can be helpful for network management and troubleshooting.",
        )],
        vec![(
          None,
          "Choose a hostname that is easy to remember and reflects the purpose or identity of your computer.",
        )],
      ]),
    );

    info_box.render(f, chunks[0]);
    self.input.render(f, hor_chunks[1]);
    self.help_modal.render(f, area);
  }

  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Char('?') => {
        self.help_modal.toggle();
        Signal::Wait
      }
      ui_close!() if self.help_modal.visible => {
        self.help_modal.hide();
        Signal::Wait
      }
      _ if self.help_modal.visible => Signal::Wait,
      KeyCode::Esc => Signal::Pop,
      KeyCode::Enter => {
        let hostname = self.input.value().trim().to_string();
        match Self::validate(&hostname) {
          Ok(()) => {
            installer.hostname = Some(hostname);
            Signal::Pop
          }
          Err(msg) => {
            self.input.set_error(msg);
            Signal::Wait
          }
        }
      }
      _ => self.input.handle_input(event),
    }
  }
}

pub struct RootPassword {
  input: LineEditor,
  confirm: LineEditor,
  help_modal: HelpModal<'static>,
}

impl RootPassword {
  pub fn new() -> Self {
    let mut input =
      LineEditor::new("Set Root Password", Some("Password will be hidden")).secret(true);
    let confirm = LineEditor::new("Confirm Password", Some("Password will be hidden")).secret(true);
    input.focus();
    let help_content = styled_block(vec![
      help_line("Enter", " - Move to next field or save when complete"),
      help_line("Tab", " - Switch between password fields"),
      help_line("Esc", " - Cancel and return to menu"),
      help_line("←/→", " - Move cursor"),
      help_line("Home/End", " - Jump to beginning/end"),
      help_line("Backspace/Del", " - Delete characters"),
      help_line("?", " - Show this help"),
      vec![(None, "")],
      vec![(None, "Set a strong root password for system security.")],
    ]);
    let help_modal = HelpModal::new("Root Password", help_content);
    Self {
      input,
      confirm,
      help_modal,
    }
  }
  pub fn page_info() -> (String, Vec<Line<'static>>) {
    (
      "Root Password".to_string(),
      styled_block(vec![
        vec![(
          None,
          "The root user is the superuser account on a Unix-like operating system, including Linux.",
        )],
        vec![(
          None,
          "It has full administrative privileges and can perform any action on the system, including installing software, modifying system settings, and accessing all files and directories.",
        )],
        vec![(
          None,
          "Setting a strong password for the root user is important for system security, as it helps prevent unauthorized access to sensitive system functions and data.",
        )],
        vec![(
          None,
          "Choose a password that is difficult to guess and contains a mix of uppercase and lowercase letters, numbers, and special characters.",
        )],
      ]),
    )
  }
  pub fn display_widget(installer: &mut Installer) -> Option<Box<dyn ConfigWidget>> {
    installer.root_passwd_hash.as_ref().map(|_| {
      let ib = InfoBox::new(
        "",
        styled_block(vec![vec![(HIGHLIGHT, "Root password is set.")]]),
      );
      Box::new(ib) as Box<dyn ConfigWidget>
    })
  }
  /// Hash a password via `mkpasswd` (stdin, never argv).
  ///
  /// `method` is a crypt(5) method name. Root MUST use `sha512crypt`: at the
  /// end of the install `mokutil -P` reads root's hash from /etc/shadow to
  /// protect MokManager, and mokutil only understands md5/sha256/sha512crypt
  /// (a yescrypt `$y$` hash fails with "Failed to get root password hash").
  pub fn mkpasswd(passwd: &str, method: &str) -> anyhow::Result<String> {
    let mut child = Command::new("mkpasswd")
      .arg(format!("--method={method}"))
      .arg("--stdin")
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .spawn()?;
    {
      let stdin = child
        .stdin
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("Failed to open stdin"))?;
      stdin.write_all(passwd.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    if output.status.success() {
      let hashed = String::from_utf8_lossy(&output.stdout).trim().to_string();
      Ok(hashed)
    } else {
      Err(anyhow::anyhow!(
        "mkpasswd failed: {}",
        String::from_utf8_lossy(&output.stderr)
      ))
    }
  }
}

impl Default for RootPassword {
  fn default() -> Self {
    Self::new()
  }
}

impl Page for RootPassword {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let chunks = split_vert!(
      area,
      1,
      [
        Constraint::Percentage(40),
        Constraint::Length(12),
        Constraint::Percentage(40),
      ]
    );
    let hor_chunks = split_hor!(
      chunks[1],
      1,
      [
        Constraint::Percentage(20),
        Constraint::Percentage(60),
        Constraint::Percentage(20),
      ]
    );
    let vert_chunks = split_vert!(
      hor_chunks[1],
      0,
      [Constraint::Length(5), Constraint::Length(5)]
    );

    let info_box = InfoBox::new(
      "",
      styled_block(vec![
        vec![(
          None,
          "The root user is the superuser account on a Unix-like operating system, including Linux.",
        )],
        vec![(
          None,
          "It has full administrative privileges and can perform any action on the system, including installing software, modifying system settings, and accessing all files and directories.",
        )],
        vec![(
          None,
          "Setting a strong password for the root user is important for system security, as it helps prevent unauthorized access to sensitive system functions and data.",
        )],
        vec![(
          None,
          "Choose a password that is difficult to guess and contains a mix of uppercase and lowercase letters, numbers, and special characters.",
        )],
      ]),
    );

    info_box.render(f, chunks[0]);
    self.input.render(f, vert_chunks[0]);
    self.confirm.render(f, vert_chunks[1]);
    self.help_modal.render(f, area);
  }

  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Char('?') => {
        self.help_modal.toggle();
        Signal::Wait
      }
      ui_close!() if self.help_modal.visible => {
        self.help_modal.hide();
        Signal::Wait
      }
      _ if self.help_modal.visible => Signal::Wait,
      KeyCode::Esc => Signal::Pop,
      KeyCode::Tab => {
        if self.input.is_focused() {
          self.input.unfocus();
          self.confirm.focus();
        } else {
          self.confirm.unfocus();
          self.input.focus();
        }
        Signal::Wait
      }
      KeyCode::Enter => {
        if self.input.is_focused() {
          self.input.unfocus();
          self.confirm.focus();
          Signal::Wait
        } else {
          let passwd = self.input.value();
          let confirm = self.confirm.value();
          if passwd.is_empty() {
            Signal::Wait // Ignore empty passwords
          } else if passwd != confirm {
            self.input.clear();
            self.confirm.clear();
            self.confirm.unfocus();
            self.input.focus();
            self.input.error("Passwords do not match");
            Signal::Wait // Passwords do not match
          } else {
            match Self::mkpasswd(&passwd, ROOT_HASH_METHOD) {
              Ok(hashed) => {
                installer.root_passwd_hash = Some(hashed);
                Signal::Pop
              }
              Err(e) => {
                self.input.clear();
                self.confirm.clear();
                self.confirm.unfocus();
                self.input.focus();
                self.input.error(format!("Error hashing password: {e}"));
                Signal::Wait
              }
            }
          }
        }
      }
      _ => {
        if self.input.is_focused() {
          self.input.handle_input(event)
        } else {
          self.confirm.handle_input(event)
        }
      }
    }
  }
}

pub struct ExtraPackages {
  picker: crate::widget::PackagePicker,
  help: HelpModal<'static>,
  state: LoadState,
  rx: Option<Receiver<anyhow::Result<()>>>,
}

#[derive(Debug, Clone)]
enum LoadState {
  Idle,           // first entry into the page
  Syncing,        // pacman -Sy running
  Ready,          // repo list loaded, picker usable
  Failed(String), // sync failed
}

impl ExtraPackages {
  pub fn new() -> Self {
    // Empty picker; filled on first render (lazy init)
    let picker =
      crate::widget::PackagePicker::new("Selected Packages", "Available Packages", &[], Vec::new());
    let help = HelpModal::new(
      "Extra Packages",
      styled_block(vec![
        vec![(None, "Select extra packages to install.")],
        vec![(
          None,
          "Use '/' to search, Enter to add/remove, Tab to switch panes.",
        )],
        vec![(None, "F5 to refresh package list (re-runs pacman -Sy).")],
        vec![(None, "Press Esc to go back and save your selection.")],
      ]),
    );
    Self {
      picker,
      help,
      state: LoadState::Idle,
      rx: None,
    }
  }

  pub fn page_info() -> (String, Vec<Line<'static>>) {
    (
      "Extra Packages".to_string(),
      styled_block(vec![vec![(
        None,
        "Pick additional packages to install alongside your base selection.",
      )]]),
    )
  }

  fn is_arch(installer: &Installer) -> bool {
    installer
      .basesystem
      .as_deref()
      .is_some_and(|s| s.to_lowercase().contains("arch"))
  }

  pub fn display_widget(installer: &mut Installer) -> Option<Box<dyn ConfigWidget>> {
    let text = if Self::is_arch(installer) {
      let n = installer.extra_packages.len();
      format!("{n} selected package{}", if n == 1 { "" } else { "s" })
    } else {
      "Not applicable to this base.".to_string()
    };
    Some(Box::new(InfoBox::new(
      "",
      styled_block(vec![vec![(None, text)]]),
    )))
  }

  fn kick_off_sync(&mut self, installer: &mut Installer) {
    let (tx, rx) = mpsc::channel::<anyhow::Result<()>>();
    self.rx = Some(rx);
    self.state = LoadState::Syncing;

    if installer.repo_synced {
      // Already synced in this session: skip pacman -Sy, reuse the cached list.
      let _ = tx.send(Ok(()));
      return;
    }
    thread::spawn(move || {
      let status = Command::new("pacman")
        .arg("-Sy")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
      let _ = tx.send(match status {
        Ok(st) if st.success() => Ok(()),
        Ok(st) => Err(anyhow::anyhow!(
          "pacman -Sy failed (exit {})",
          st.code().unwrap_or(-1)
        )),
        Err(e) => Err(anyhow::anyhow!("failed to exec pacman -Sy: {e}")),
      });
    });
  }

  fn load_packages_after_sync(&mut self, installer: &mut Installer) -> anyhow::Result<()> {
    let available = match &installer.cached_repo_pkgs {
      Some(list) => list.clone(),
      None => {
        let out = Command::new("pacman").arg("-Slq").output()?;
        if !out.status.success() {
          anyhow::bail!(
            "pacman -Slq failed (exit {})",
            out.status.code().unwrap_or(-1)
          );
        }
        let names: Vec<String> = String::from_utf8(out.stdout)?
          .lines()
          .map(str::trim)
          .filter(|l| !l.is_empty())
          .map(ToOwned::to_owned)
          .collect();
        installer.cached_repo_pkgs = Some(names.clone());
        names
      }
    };
    installer.repo_synced = true;

    self
      .picker
      .set_package_manager(crate::widget::PackageManager::new(
        available,
        &installer.extra_packages,
      ));
    self.picker.focus_search();
    Ok(())
  }

  fn request_refresh(&mut self, installer: &mut Installer) {
    installer.cached_repo_pkgs = None;
    installer.repo_synced = false;
    self.state = LoadState::Idle;
    self.rx = None;
  }
}

impl Default for ExtraPackages {
  fn default() -> Self {
    Self::new()
  }
}

impl Page for ExtraPackages {
  fn render(&mut self, installer: &mut Installer, f: &mut Frame, area: Rect) {
    if !Self::is_arch(installer) {
      const NOT_ARCH: &str = "This page is available if the selected Base System is Arch.";
      crate::a11y::context(|c| c.push_str(NOT_ARCH));
      let p = Paragraph::new(NOT_ARCH).alignment(Alignment::Center).block(
        Block::default()
          .borders(Borders::ALL)
          .title("Extra Packages"),
      );
      f.render_widget(p, area);
      return;
    }

    if matches!(self.state, LoadState::Idle) {
      self.kick_off_sync(installer);
    }
    if matches!(self.state, LoadState::Syncing)
      && let Some(rx) = &self.rx
      && let Ok(result) = rx.try_recv()
    {
      self.state = match result.and_then(|()| self.load_packages_after_sync(installer)) {
        Ok(()) => LoadState::Ready,
        Err(e) => LoadState::Failed(format!("{e}")),
      };
    }

    match &self.state {
      LoadState::Idle | LoadState::Syncing => {
        const SYNCING: &str = "Syncing package list with pacman -Sy...";
        crate::a11y::context(|c| c.push_str(SYNCING));
        let msg = Paragraph::new(SYNCING).alignment(Alignment::Center).block(
          Block::default()
            .borders(Borders::ALL)
            .title("Extra Packages"),
        );
        f.render_widget(msg, area);
      }
      LoadState::Failed(err) => {
        let lines = styled_block(vec![
          vec![(
            Some((Color::Red, Modifier::BOLD)),
            "Failed to prepare package list.".to_string(),
          )],
          vec![(None, String::new())],
          vec![(
            None,
            "You can run the following manually in the live system:".to_string(),
          )],
          vec![(KEY, "  pacman -Sy".to_string())],
          vec![(None, String::new())],
          vec![(Some((Color::Indexed(245), Modifier::ITALIC)), err.clone())],
          vec![(None, String::new())],
          vec![(None, "Press Esc to go back.".to_string())],
        ]);
        crate::a11y::context_lines("Extra Packages", &lines);
        let p = Paragraph::new(lines)
          .block(
            Block::default()
              .borders(Borders::ALL)
              .title("Extra Packages"),
          )
          .wrap(Wrap { trim: false });
        f.render_widget(p, area);
      }
      LoadState::Ready => {
        self.picker.render(f, area);
        self.help.render(f, area);
      }
    }
  }

  fn needs_tick(&self) -> bool {
    matches!(self.state, LoadState::Idle | LoadState::Syncing)
  }

  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    if let KeyCode::Char('?') = event.code {
      self.help.toggle();
      return Signal::Wait;
    }
    if self.help.visible {
      if let KeyCode::Esc = event.code {
        self.help.toggle();
      }
      return Signal::Wait;
    }
    if let KeyCode::F(5) = event.code {
      self.request_refresh(installer);
      return Signal::Wait;
    }
    // If not ready, only allow leaving the page
    if !matches!(self.state, LoadState::Ready) {
      return if event.code == KeyCode::Esc {
        Signal::Pop
      } else {
        Signal::Wait
      };
    }
    // Ready: on Esc store selection and go back
    if event.code == KeyCode::Esc {
      installer.extra_packages = self.picker.get_selected_packages();
      return Signal::Pop;
    }
    self.picker.handle_input(event)
  }
}

pub struct ConfigPreview {
  /// Pre-rendered (syntax highlighted) lines for each view.
  system_lines: Vec<Line<'static>>,
  partition_lines: Vec<Line<'static>>,
  scroll_position: usize,
  /// Speech only: lines read below the top line once scrolling stops at the
  /// end, so the last page can be read line by line too.
  read_offset: usize,
  button_row: WidgetBox,
  current_view: ConfigView,
  help_modal: HelpModal<'static>,
  visible_lines: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum ConfigView {
  System,
  Partitioning,
}

impl ConfigPreview {
  /// Maximum scroll distance for config preview window
  fn current_lines(&self) -> &[Line<'static>] {
    match self.current_view {
      ConfigView::System => &self.system_lines,
      ConfigView::Partitioning => &self.partition_lines,
    }
  }

  fn get_max_scroll(&self, visible_lines: usize) -> usize {
    self.current_lines().len().saturating_sub(visible_lines)
  }

  /// Pretty-print and highlight a JSON value once, for display.
  fn render_json(v: &Value) -> Vec<Line<'static>> {
    let text = to_string_pretty(v).unwrap_or_else(|_| "null".to_string());
    highlight_json(&text)
      .into_text()
      .map(|t| t.lines)
      .unwrap_or_else(|_| text.lines().map(|l| Line::from(l.to_string())).collect())
  }

  pub fn new(installer: &mut Installer) -> Self {
    // Generate the configuration like the main app does; highlighting is done
    // here once (it used to spawn `bat` on every frame).
    let mut config_json = installer.build_config_json();
    let (system_v, drives_v) = match &mut config_json {
      Value::Object(map) => (
        map.remove("config").unwrap_or(Value::Null),
        map.remove("drives").unwrap_or(Value::Null),
      ),
      _ => (Value::Null, Value::Null),
    };
    let mut system_v = system_v;
    mask_hashes(&mut system_v);
    let system_lines = Self::render_json(&system_v);
    let partition_lines = Self::render_json(&drives_v);

    let buttons: Vec<Box<dyn ConfigWidget>> = vec![
      Box::new(Button::new("Begin Installation")),
      Box::new(Button::new("Back")),
    ];
    let button_row = WidgetBox::button_menu(buttons);
    let help_content = styled_block(vec![
      help_line("1/2", " - Switch between System/Partition config"),
      help_line("↑/↓, j/k", " - Scroll config content"),
      help_line("Page Up/Down", " - Scroll page by page"),
      help_line("Tab", " - Switch to buttons"),
      help_line("Enter", " - Activate selected button"),
      help_line("Esc", " - Go back to menu"),
      help_line("?", " - Show this help"),
      vec![(None, "")],
      vec![(
        None,
        "Review the generated Athena OS configuration before saving.",
      )],
    ]);
    let help_modal = HelpModal::new("Config Preview", help_content);

    Self {
      system_lines,
      partition_lines,
      scroll_position: 0,
      read_offset: 0,
      button_row,
      current_view: ConfigView::System,
      help_modal,
      visible_lines: 10, // Default value, will be updated during rendering
    }
  }
}

/// Password hashes carry nothing a reviewer can check, and must not be read
/// aloud: show them as "(hidden)". Only the preview is masked, never the
/// configuration handed to the backend.
fn mask_hashes(v: &mut Value) {
  match v {
    Value::Object(map) => {
      for (key, value) in map.iter_mut() {
        if key.ends_with("_hash") && value.is_string() {
          *value = Value::String("(hidden)".into());
        } else {
          mask_hashes(value);
        }
      }
    }
    Value::Array(items) => items.iter_mut().for_each(mask_hashes),
    _ => {}
  }
}

impl Page for ConfigPreview {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let chunks = split_vert!(
      area,
      1,
      [
        Constraint::Length(3), // Tab bar
        Constraint::Min(0),    // Config content
        Constraint::Length(3), // Buttons
      ]
    );

    // Tab bar for switching between system and partition config
    let tab_chunks = split_hor!(
      chunks[0],
      0,
      [Constraint::Percentage(50), Constraint::Percentage(50)]
    );

    // System config tab
    let system_tab_style = if self.current_view == ConfigView::System {
      Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
    } else {
      Style::default().fg(Color::Gray)
    };
    let system_tab = Paragraph::new("System Config [1]")
      .style(system_tab_style)
      .alignment(Alignment::Center)
      .block(Block::default().borders(Borders::ALL));
    f.render_widget(system_tab, tab_chunks[0]);

    // Partition config tab
    let partition_tab_style = if self.current_view == ConfigView::Partitioning {
      Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
    } else {
      Style::default().fg(Color::Gray)
    };
    let partition_tab = Paragraph::new("Partition Config [2]")
      .style(partition_tab_style)
      .alignment(Alignment::Center)
      .block(Block::default().borders(Borders::ALL));
    f.render_widget(partition_tab, tab_chunks[1]);

    // Config content
    let visible_lines = (chunks[1].height as usize).saturating_sub(2); // Account for borders
    self.visible_lines = visible_lines;
    let lines = self.current_lines();

    let start_line = self.scroll_position.min(lines.len());
    let end_line = (start_line + visible_lines).min(lines.len());
    let display_lines = lines[start_line..end_line].to_vec();

    let config_paragraph = Paragraph::new(display_lines)
      .block(Block::default().borders(Borders::ALL).title(format!(
        "Preview - {} Config (Scroll: {}/{})",
        match self.current_view {
          ConfigView::System => "System",
          ConfigView::Partitioning => "Partitioning",
        },
        start_line + 1,
        self.get_max_scroll(visible_lines) + 1
      )))
      .wrap(Wrap { trim: false });
    f.render_widget(config_paragraph, chunks[1]);

    // Scrolling the preview reads it line by line: the top visible line,
    // then (speech only) the lines below it once the end is reached.
    if !self.button_row.is_focused() {
      let read = start_line + self.read_offset.min(visible_lines.saturating_sub(1));
      let content = chunks[1];
      let row = (read - start_line) as u16;
      if content.width > 2 && row + 2 < content.height {
        crate::a11y::cursor(content.x + 1, content.y + 1 + row);
      }
      let region = match self.current_view {
        ConfigView::System => "System configuration preview",
        ConfigView::Partitioning => "Partition configuration preview",
      };
      crate::a11y::focus(region, || match lines.get(read) {
        Some(line) => {
          let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
          format!("{}, line {} of {}", text.trim(), read + 1, lines.len())
        }
        None => "empty".into(),
      });
    }

    // Buttons
    self.button_row.render(f, chunks[2]);

    // Help modal
    self.help_modal.render(f, area);
  }

  fn handle_input(&mut self, _installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Char('?') => {
        self.help_modal.toggle();
        Signal::Wait
      }
      ui_close!() if self.help_modal.visible => {
        self.help_modal.hide();
        Signal::Wait
      }
      _ if self.help_modal.visible => Signal::Wait,
      KeyCode::Char('1') => {
        self.button_row.unfocus();
        self.current_view = ConfigView::System;
        self.scroll_position = 0;
        self.read_offset = 0;
        Signal::Wait
      }
      KeyCode::Char('2') => {
        self.button_row.unfocus();
        self.current_view = ConfigView::Partitioning;
        self.scroll_position = 0;
        self.read_offset = 0;
        Signal::Wait
      }
      ui_up!() => {
        if self.button_row.is_focused() {
          if !self.button_row.prev_child() {
            self.button_row.unfocus();
          }
        } else if self.read_offset > 0 {
          self.read_offset -= 1;
        } else if self.scroll_position > 0 {
          self.scroll_position -= 1;
        }
        Signal::Wait
      }
      ui_down!() => {
        if self.button_row.is_focused() {
          self.button_row.next_child();
        } else {
          let max_scroll = self.get_max_scroll(self.visible_lines);
          let last = self.current_lines().len().saturating_sub(1);
          if self.scroll_position < max_scroll {
            self.scroll_position += 1;
          } else if crate::a11y::speaking() && self.scroll_position + self.read_offset < last {
            self.read_offset += 1;
          } else if !self.button_row.is_focused() {
            self.button_row.focus();
          }
        }
        Signal::Wait
      }
      ui_right!() => {
        if self.button_row.is_focused() {
          if !self.button_row.next_child() {
            self.button_row.first_child();
          }
        } else if self.current_view == ConfigView::System {
          self.current_view = ConfigView::Partitioning;
          self.scroll_position = 0;
          self.read_offset = 0;
        } else if self.current_view == ConfigView::Partitioning {
          self.current_view = ConfigView::System;
          self.scroll_position = 0;
          self.read_offset = 0;
        }

        Signal::Wait
      }
      ui_left!() => {
        if self.button_row.is_focused() {
          if !self.button_row.prev_child() {
            self.button_row.last_child();
          }
        } else if self.current_view == ConfigView::Partitioning {
          self.current_view = ConfigView::System;
          self.scroll_position = 0;
          self.read_offset = 0;
        } else if self.current_view == ConfigView::System {
          self.current_view = ConfigView::Partitioning;
          self.scroll_position = 0;
          self.read_offset = 0;
        }

        Signal::Wait
      }
      KeyCode::PageUp => {
        self.scroll_position = self.scroll_position.saturating_sub(10);
        self.read_offset = 0;
        Signal::Wait
      }
      KeyCode::PageDown => {
        let max_scroll = self.get_max_scroll(self.visible_lines);
        self.scroll_position = std::cmp::min(self.scroll_position + 10, max_scroll);
        self.read_offset = 0;
        Signal::Wait
      }
      KeyCode::Tab => {
        self.button_row.focus();
        Signal::Wait
      }
      KeyCode::Enter => {
        if self.button_row.is_focused() {
          match self.button_row.selected_child() {
            Some(0) => Signal::WriteCfg, // Save & Exit
            Some(1) => Signal::Pop,      // Back
            _ => Signal::Wait,
          }
        } else {
          Signal::Wait
        }
      }
      KeyCode::Esc => Signal::Pop,
      _ => {
        if self.button_row.is_focused() {
          self.button_row.handle_input(event)
        } else {
          Signal::Wait
        }
      }
    }
  }
}
pub struct AgeVerification {
  system_cfg: Option<NamedTempFile>,
  partition_cfg: Option<NamedTempFile>,
  log_path: PathBuf,
  started_at: Instant,
  skipped_animation: bool,
  selected_button: usize,
  button_row: WidgetBox,
  help_modal: HelpModal<'static>,
}

impl AgeVerification {
  pub fn new(system_cfg: NamedTempFile, partition_cfg: NamedTempFile, log_path: PathBuf) -> Self {
    let mut button_row = WidgetBox::button_menu(vec![
      Box::new(Button::new("I am overqualified")) as Box<dyn ConfigWidget>,
      Box::new(Button::new("I am not")) as Box<dyn ConfigWidget>,
    ]);

    let help_modal = HelpModal::new(
      "Age verification",
      styled_block(vec![
        vec![
          (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
          (None, " - continue to installation"),
        ],
        vec![
          (Some((Color::Yellow, Modifier::BOLD)), "Esc"),
          (None, " - go back to the previous page"),
        ],
        vec![
          (Some((Color::Yellow, Modifier::BOLD)), "?"),
          (None, " - show this help"),
        ],
        vec![(None, "")],
        vec![(
          None,
          "This is a serious verification process, in the sense that it has a window and a button.",
        )],
      ]),
    );

    button_row.focus();

    Self {
      system_cfg: Some(system_cfg),
      partition_cfg: Some(partition_cfg),
      log_path,
      started_at: Instant::now(),
      skipped_animation: false,
      selected_button: 0,
      button_row,
      help_modal,
    }
  }

  fn reveal_joke(&self) -> bool {
    self.skipped_animation || self.started_at.elapsed() >= Duration::from_secs(5)
  }

  fn full_joke_text() -> &'static str {
    "Just kidding.
Athena does not perform age verification.
Operating systems are not nightclubs.

If you can run this installer, you are already overqualified."
  }

  fn visible_joke_text(&self) -> String {
    if self.skipped_animation {
      return Self::full_joke_text().to_string();
    }

    if !self.reveal_joke() {
      return String::new();
    }

    let elapsed_after_delay = self
      .started_at
      .elapsed()
      .saturating_sub(Duration::from_secs(5));
    let chars_per_second = 12usize;
    let visible_chars = (elapsed_after_delay.as_millis() as usize * chars_per_second) / 1000;
    let full_text = Self::full_joke_text();
    let total_chars = full_text.chars().count();
    let take_chars = visible_chars.min(total_chars);

    full_text.chars().take(take_chars).collect()
  }

  fn joke_fully_visible(&self) -> bool {
    self.skipped_animation
      || self.visible_joke_text().chars().count() >= Self::full_joke_text().chars().count()
  }

  fn popup_area(area: Rect) -> Rect {
    let popup_width = (area.width as f32 * 0.72) as u16;
    let popup_height = 14.min(area.height.saturating_sub(2)).max(10);
    let x = area.x + area.width.saturating_sub(popup_width) / 2;
    let y = area.y + area.height.saturating_sub(popup_height) / 2;

    Rect {
      x,
      y,
      width: popup_width,
      height: popup_height,
    }
  }

  fn start_install(&mut self) -> Signal {
    let Some(system_cfg) = self.system_cfg.take() else {
      return Signal::Error(anyhow::anyhow!("System config temp file already consumed"));
    };

    let Some(partition_cfg) = self.partition_cfg.take() else {
      return Signal::Error(anyhow::anyhow!(
        "Partition config temp file already consumed"
      ));
    };

    match InstallProgress::new(system_cfg, partition_cfg, self.log_path.clone()) {
      Ok(progress) => Signal::Push(Box::new(progress)),
      Err(err) => Signal::Error(err),
    }
  }
}

impl Page for AgeVerification {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let popup_area = Self::popup_area(area);
    f.render_widget(Clear, popup_area);

    let revealed = self.reveal_joke();
    let joke_text = self.visible_joke_text();
    let mut lines = vec![
      Line::from("Please confirm you are old enough to partition a disk..."),
      Line::from(""),
    ];

    if revealed {
      if joke_text.is_empty() {
        for _ in 0..5 {
          lines.push(Line::from(" "));
        }
      } else {
        let mut split = joke_text.split('\n');

        if let Some(first_line) = split.next() {
          if !first_line.is_empty() {
            lines.push(Line::from(vec![Span::styled(
              first_line.to_string(),
              Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            )]));
          } else {
            lines.push(Line::from(""));
          }
        }

        for line in split {
          lines.push(Line::from(line.to_string()));
        }
      }
    } else {
      for _ in 0..5 {
        lines.push(Line::from(" "));
      }
    }

    let block = Block::default()
      .title("Age verification")
      .borders(Borders::ALL)
      .border_style(Style::default().fg(Color::Yellow))
      .style(Style::default().bg(Color::Black));

    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let content_chunks = Layout::default()
      .direction(Direction::Vertical)
      .constraints([Constraint::Min(0), Constraint::Length(3)])
      .split(inner);

    // Speak the prompt, then the joke once it is complete: never the
    // character-by-character reveal.
    let complete = self.joke_fully_visible();
    crate::a11y::context(|c| {
      c.push_str("Age verification\n");
      c.push_str("Please confirm you are old enough to partition a disk...");
      if complete {
        c.push('\n');
        c.push_str(Self::full_joke_text());
      }
    });

    let paragraph = Paragraph::new(lines)
      .alignment(Alignment::Center)
      .wrap(Wrap { trim: true })
      .style(Style::default().fg(Color::White).bg(Color::Black));
    f.render_widget(paragraph, content_chunks[0]);

    if self.joke_fully_visible() {
      self.button_row.render(f, content_chunks[1]);
    }

    self.help_modal.render(f, area);
  }

  /// The reveal is time-based, so keep redrawing until the buttons are shown.
  fn needs_tick(&self) -> bool {
    !self.joke_fully_visible()
  }

  fn handle_input(&mut self, _installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Char('?') => {
        self.help_modal.toggle();
        Signal::Wait
      }
      KeyCode::Esc if self.help_modal.visible => {
        self.help_modal.hide();
        Signal::Wait
      }
      _ if self.help_modal.visible => Signal::Wait,
      KeyCode::Esc => Signal::Pop,
      KeyCode::Enter if !self.joke_fully_visible() => {
        self.skipped_animation = true;
        Signal::Wait
      }
      ui_up!() | ui_left!() => {
        self.button_row.prev_child();
        self.selected_button = self.selected_button.saturating_sub(1);
        Signal::Wait
      }
      ui_down!() | ui_right!() => {
        self.button_row.next_child();
        self.selected_button = (self.selected_button + 1).min(1);
        Signal::Wait
      }
      KeyCode::Enter => {
        if self.selected_button == 0 {
          self.start_install()
        } else {
          Signal::Pop
        }
      }
      _ => Signal::Wait,
    }
  }
}

const TICKER_MESSAGES: &[&str] = &[
  "Athena OS installation in progress.",
  "The universe conspires in your favor.",
  "Patience is an art form.",
  "The Empress of Hell will back from the underworld.",
  "The stars whisper your name in binary.",
  "Forged in wisdom, tempered by code.",
  "Zeus approves this kernel.",
  "The Parthenon awaits your triumph.",
  "A new system rises from chaos.",
  "Your machine ascends to digital divinity.",
  "No demons were harmed in this ritual.",
  "The moon approves this build.",
  "Your reign over this system begins soon.",
  "Burning old partitions in sacred fire.",
  "Please don't alt+F4 destiny.",
  "We're almost done (trust me, I'm the process).",
  "Did you know? 'rm -rf /' removes anxiety.",
  "At least one thread still believes in you.",
  "Hack the planet (responsibly).",
  "It's 5 AM and I am still designing this installer.",
  "Compiling... because chaos needs structure.",
  "sudo: may the force be with you.",
  "Hacking blessed by Athena herself.",
  "Discord user be like: 'sir'",
  "I live in your CPU now. Be kind.",
  "0xDEADBEEF approves this build.",
  "Hackers gonna hack, compilers gonna complain.",
  "The Oracle predicts zero segfaults.",
  "Too many tabs open... in life.",
  "Hades formatted the underworld in ext4.",
  "99% complete... statistically speaking.",
  "Deleting Windows... emotionally.",
  "Currently overclocking your patience.",
  "Yes, I really am installing things - probably.",
  "If this works, I'll pretend it was intentional.",
  "Error 404: Patience not found.",
  "[ERROR]: Installation failed... OH F*CK!",
  "Make me a cake please :')",
  "Is it D3vil0p3r or... Luc1f3r?",
];

/// Side file where the backend publishes the still-changing progress rows.
fn live_file_path(log_file_path: &Path) -> PathBuf {
  log_file_path.with_extension("live")
}

pub struct InstallProgress<'a> {
  steps: InstallSteps<'a>,
  log_box: LogBox<'a>,
  ticker: FancyTicker,
  help_modal: HelpModal<'static>,

  // Held so the config files stay alive during installation.
  _system_cfg: NamedTempFile,
  _partition_cfg: NamedTempFile,
  log_file: PathBuf,
}

impl<'a> InstallProgress<'a> {
  pub fn new(
    system_cfg: NamedTempFile,
    partition_cfg: NamedTempFile,
    log_path: PathBuf,
  ) -> anyhow::Result<Self> {
    let install_steps = Self::install_commands(system_cfg.path(), partition_cfg.path(), &log_path)?;

    let mut steps = InstallSteps::new("Install Steps", install_steps);
    steps.log_path = Some(log_path.clone());

    let help_content = styled_block(vec![
      help_line("Esc", " - Exit installation (if failed)"),
      help_line("?", " - Show this help"),
      vec![(None, "")],
      vec![(
        None,
        "This page shows the progress of the Athena OS installation process.",
      )],
      vec![(
        None,
        "Installation steps are executed sequentially and their status is shown above.",
      )],
    ]);
    let help_modal = HelpModal::new("Installation Progress", help_content);

    // Each install starts with an empty log file.
    std::fs::write(&log_path, b"")?;
    let _ = std::fs::remove_file(live_file_path(&log_path));
    let mut log_box = LogBox::new("Logs".into());
    log_box.open_log(log_path.clone())?;
    log_box.watch_live(live_file_path(&log_path));

    Ok(Self {
      steps,
      log_box,
      ticker: FancyTicker::new(TICKER_MESSAGES),
      help_modal,
      _system_cfg: system_cfg,
      _partition_cfg: partition_cfg,
      log_file: log_path,
    })
  }

  pub fn has_error(&self) -> bool {
    self.steps.has_error()
  }

  /// The installation runs as a child process of this same binary (not via a
  /// shell): the backend terminates with `process::exit` on fatal errors, and
  /// keeping it out of process keeps the TUI alive to show the log.
  fn install_commands(
    system_cfg_path: &Path,
    disk_cfg_path: &Path,
    log_file_path: &Path,
  ) -> anyhow::Result<Vec<(Line<'static>, VecDeque<Command>)>> {
    let exe = std::env::current_exe()?;
    let log = std::fs::OpenOptions::new()
      .append(true)
      .create(true)
      .open(log_file_path)?;
    // The backend sizes the package manager's pty to the Logs pane so
    // progress bars (drawn to the full terminal width) fit on one row.
    let (cols, rows) = ratatui::crossterm::terminal::size().unwrap_or((120, 40));
    let log_cols = Self::log_pane_inner_width(cols, rows);
    let mut install = Command::new(exe);
    install
      .arg("--system-file")
      .arg(system_cfg_path)
      .arg("--drives-file")
      .arg(disk_cfg_path)
      .env(
        core::internal::install::LIVE_FILE_ENV,
        live_file_path(log_file_path),
      )
      .env(core::internal::install::PTY_COLS_ENV, log_cols.to_string())
      .stdout(Stdio::from(log.try_clone()?))
      .stderr(Stdio::from(log));
    Ok(vec![(
      Line::from("Athena OS Installation..."),
      VecDeque::from([install]),
    )])
  }
}

impl InstallProgress<'_> {
  /// Width of the Install Steps box: its longest line plus borders. The Logs
  /// box takes all the remaining width.
  const STEPS_WIDTH: u16 = 32;

  /// Page layout: [steps | logs] above the ticker. Shared by `render` and the
  /// pty-width calculation so the two can never disagree.
  fn layout(area: Rect) -> (Rect, Rect, Rect) {
    let chunks = split_vert!(area, 1, [Constraint::Min(0), Constraint::Length(3)]);
    let hor_chunks = split_hor!(
      chunks[0],
      1,
      [Constraint::Length(Self::STEPS_WIDTH), Constraint::Min(0)]
    );
    (hor_chunks[0], hor_chunks[1], chunks[1])
  }

  /// Columns available inside the Logs box for a terminal of `cols` x `rows`
  /// (the page gets everything below the one-line header).
  fn log_pane_inner_width(cols: u16, rows: u16) -> u16 {
    let page = Rect::new(0, 1, cols, rows.saturating_sub(1));
    let (_, logs, _) = Self::layout(page);
    logs.width.saturating_sub(2).max(40) // borders
  }
}

impl Page for InstallProgress<'_> {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    // Tick the steps to update animation and process commands
    let _ = self.steps.tick();
    let _ = self.log_box.poll_log();

    let (steps_area, logs_area, ticker_area) = Self::layout(area);

    self.steps.render(f, steps_area);
    self.log_box.render(f, logs_area);

    self.ticker.tick();
    self.ticker.render(f, ticker_area);

    self.help_modal.render(f, area);
  }

  fn needs_tick(&self) -> bool {
    true
  }

  /// Push the outcome page once the backend exits. `signal` is only consulted
  /// for the top page, so this fires once: afterwards the outcome page is on top.
  fn signal(&self) -> Option<Signal> {
    if self.steps.has_error() {
      Some(Signal::Push(Box::new(InstallFailed::new(
        self.log_file.clone(),
      ))))
    } else if self.steps.is_complete() {
      Some(Signal::Push(Box::new(InstallComplete::new())))
    } else {
      None
    }
  }

  fn handle_input(&mut self, _installer: &mut Installer, event: KeyEvent) -> Signal {
    if event.code == KeyCode::Char('c') && event.modifiers.contains(KeyModifiers::CONTROL) {
      // InstallSteps kills the child on drop, so quitting mid-install is safe.
      return Signal::Quit;
    }
    if self.help_modal.visible || event.code == KeyCode::Char('?') {
      self.help_modal.toggle();
      return Signal::Wait;
    }
    if self.has_error() && matches!(event.code, KeyCode::Esc | KeyCode::Char('q')) {
      return Signal::Pop;
    }
    Signal::Wait
  }
}

pub struct InstallComplete {
  text_box: InfoBox<'static>,
}

impl InstallComplete {
  pub fn new() -> Self {
    let content = styled_block(vec![
      vec![(
        None,
        "Athena OS has been successfully installed on your system!",
      )],
      vec![(None, "")],
      vec![(
        None,
        "You can now reboot your computer and remove the installation media.",
      )],
      vec![(None, "")],
      vec![(
        None,
        "The new system has been unmounted. To perform manual configuration, mount it again and use the 'athena-chroot' command.",
      )],
      vec![(None, "")],
      vec![(None, "Press any key to exit the installer.")],
    ]);
    let text_box = InfoBox::new("Installation Complete", content);
    Self { text_box }
  }
}

impl Default for InstallComplete {
  fn default() -> Self {
    Self::new()
  }
}

impl Page for InstallComplete {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let chunks = split_vert!(area, 1, [Constraint::Percentage(100)]);
    self.text_box.render(f, chunks[0]);
  }

  fn handle_input(&mut self, _installer: &mut Installer, _event: KeyEvent) -> Signal {
    Signal::Quit
  }
}

pub struct InstallFailed {
  log_path: PathBuf,
  /// Last errors the backend logged: the actual cause, shown up front.
  errors: Vec<String>,
  state: LogGenState,
  rx: Option<Receiver<Result<String, String>>>,
}

enum LogGenState {
  Idle,
  /// Waiting for the user to confirm the upload.
  Confirm,
  Running,
  Done(String),
  Error(String),
}

/// Lines that must never leave the machine, even in a failure log.
fn redact_log(contents: &str) -> String {
  contents
    .lines()
    .filter(|l| {
      !(l.contains("password_hash")
        || l.contains("root_passwd_hash")
        || l.contains("$6$")
        || l.contains("$y$"))
    })
    .collect::<Vec<_>>()
    .join("\n")
}

/// Errors shown on the failure page.
const SHOWN_ERRORS: usize = 3;

impl InstallFailed {
  pub fn new(log_path: PathBuf) -> Self {
    let errors = Self::last_errors(&log_path);
    Self {
      log_path,
      errors,
      state: LogGenState::Idle,
      rx: None,
    }
  }

  /// The last `[ ERROR ]` records of the install log, read once.
  fn last_errors(log_path: &Path) -> Vec<String> {
    use std::io::BufRead;
    let Ok(file) = std::fs::File::open(log_path) else {
      return Vec::new();
    };
    let mut last = VecDeque::with_capacity(SHOWN_ERRORS + 1);
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
      if !line.starts_with("[ ERROR ]") {
        continue;
      }
      let line = strip_ansi_escapes::strip_str(&line);
      if let Some(("ERROR", msg)) = crate::widget::split_log_prefix(&line) {
        last.push_back(msg.trim().to_string());
        if last.len() > SHOWN_ERRORS {
          last.pop_front();
        }
      }
    }
    last.into()
  }

  fn start_generation(&mut self) {
    let (tx, rx) = mpsc::channel();
    self.rx = Some(rx);
    self.state = LogGenState::Running;

    let log_path = self.log_path.clone();
    thread::spawn(move || {
      let contents = match std::fs::read_to_string(&log_path) {
        Ok(c) => redact_log(&c),
        Err(e) => {
          let _ = tx.send(Err(format!("Failed to read log file: {e}")));
          return;
        }
      };
      let result = (|| -> Result<String, String> {
        let mut stream = TcpStream::connect(("termbin.com", 9999))
          .map_err(|e| format!("Connection error: {e}"))?;
        stream.set_write_timeout(Some(Duration::from_secs(30))).ok();
        stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
        stream
          .write_all(contents.as_bytes())
          .map_err(|e| format!("Write error: {e}"))?;
        let mut response = String::new();
        // termbin closes the connection after sending the link
        stream
          .read_to_string(&mut response)
          .map_err(|e| format!("Read error: {e}"))?;
        let link = response
          .trim_matches(|c: char| c.is_whitespace() || c == '\0')
          .to_string();
        if link.is_empty() {
          Err("No response from termbin".into())
        } else {
          Ok(link)
        }
      })();
      let _ = tx.send(result);
    });
  }

  fn poll_generation(&mut self) {
    if let Some(rx) = &self.rx
      && let Ok(msg) = rx.try_recv()
    {
      self.state = match msg {
        Ok(url) => LogGenState::Done(url),
        Err(err) => LogGenState::Error(err),
      };
      self.rx = None;
    }
  }

  fn content_lines(&self) -> Vec<Line<'static>> {
    let nav = [
      help_line("Esc/Q", " - Go back"),
      help_line("Enter", " - Exit installer"),
    ];
    let mut lines: Vec<StyledLine> = match &self.state {
      LogGenState::Idle => {
        let mut lines = vec![vec![(None, "Athena OS installation FAILED.".into())]];
        if !self.errors.is_empty() {
          lines.push(vec![(None, String::new())]);
          lines.push(vec![(None, "Last errors:".into())]);
          lines.extend(
            self
              .errors
              .iter()
              .map(|e| vec![(Some((Color::Red, Modifier::BOLD)), e.clone())]),
          );
          lines.push(vec![(None, String::new())]);
        }
        lines.extend([vec![(
          None,
          format!("The full log is in {}.", self.log_path.display()),
        )]]);
        lines.extend([
          vec![(None, String::new())],
          vec![
            (KEY, "Press 'G'".into()),
            (
              None,
              " to upload the log to termbin.com and get a shareable link.".into(),
            ),
          ],
        ]);
        lines
      }
      LogGenState::Confirm => vec![
        vec![(
          Some((Color::Red, Modifier::BOLD)),
          "The log will be uploaded to termbin.com (public, unencrypted).".into(),
        )],
        vec![(
          None,
          "Password hashes are stripped; hostname, user names and package names are not.".into(),
        )],
        vec![(None, String::new())],
        vec![(KEY, "Y".into()), (None, " - Upload now".into())],
        vec![(KEY, "N".into()), (None, " - Cancel".into())],
      ],
      LogGenState::Running => vec![vec![(
        None,
        "Generating log link... contacting termbin.com".into(),
      )]],
      LogGenState::Done(url) => vec![
        vec![(None, "Log link generated successfully:".into())],
        vec![(Some((Color::Cyan, Modifier::BOLD)), url.clone())],
      ],
      LogGenState::Error(err) => vec![
        vec![(None, "Failed to generate log link.".into())],
        vec![(Some((Color::Red, Modifier::BOLD)), err.clone())],
        vec![(KEY, "G".into()), (None, " - Retry".into())],
      ],
    };
    lines.push(vec![(None, String::new())]);
    lines.extend(
      nav
        .into_iter()
        .map(|l| l.into_iter().map(|(s, t)| (s, t.to_string())).collect()),
    );
    styled_block(lines)
  }
}

impl Page for InstallFailed {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    self.poll_generation();
    let chunks = split_vert!(area, 1, [Constraint::Percentage(100)]);
    InfoBox::new("Installation Failed", self.content_lines()).render(f, chunks[0]);
  }

  fn needs_tick(&self) -> bool {
    matches!(self.state, LogGenState::Running)
  }

  fn handle_input(&mut self, _installer: &mut Installer, event: KeyEvent) -> Signal {
    match (&self.state, event.code) {
      (LogGenState::Confirm, KeyCode::Char('y') | KeyCode::Char('Y')) => self.start_generation(),
      (LogGenState::Confirm, KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc) => {
        self.state = LogGenState::Idle
      }
      (LogGenState::Idle | LogGenState::Error(_), KeyCode::Char('g') | KeyCode::Char('G')) => {
        self.state = LogGenState::Confirm;
      }
      (_, KeyCode::Esc | KeyCode::Char('q')) => return Signal::PopCount(2),
      (_, KeyCode::Enter) => return Signal::Quit,
      _ => {}
    }
    Signal::Wait
  }
}

#[cfg(test)]
mod a11y_tests {
  use super::*;
  use ratatui::{Terminal, backend::TestBackend};

  fn say(menu: &mut Menu, installer: &mut Installer) -> String {
    let mut term = Terminal::new(TestBackend::new(120, 30)).unwrap();
    term
      .draw(|f| {
        crate::a11y::begin_frame();
        menu.render(installer, f, f.area());
      })
      .unwrap();
    crate::a11y::utterance_for_tests()
  }

  fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
  }

  fn say_page(page: &mut dyn Page, installer: &mut Installer, h: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(100, h)).unwrap();
    term
      .draw(|f| {
        crate::a11y::begin_frame();
        page.render(installer, f, f.area());
      })
      .unwrap();
    crate::a11y::utterance_for_tests()
  }

  #[test]
  fn config_preview_reads_to_the_last_line_and_never_a_hash() {
    crate::a11y::enable_for_tests();
    let mut installer = Installer::new();
    installer.hostname = Some("athena".into());
    installer.root_passwd_hash = Some("$6$salt$secrethash".into());
    let mut page = ConfigPreview::new(&mut installer);
    let total = page.system_lines.len();
    let mut said = say_page(&mut page, &mut installer, 14);
    for _ in 0..total + 5 {
      page.handle_input(&mut installer, key(KeyCode::Down));
      said.push_str(&say_page(&mut page, &mut installer, 14));
      said.push('|');
    }
    assert!(
      said.contains(&format!("line {total} of {total}")),
      "last line reached: {said}"
    );
    assert!(said.contains("Begin Installation, button"), "{said}");
    assert!(
      !said.contains("secrethash") && said.contains("(hidden)"),
      "{said}"
    );
    // The configuration handed to the backend still has the hash.
    assert!(
      installer
        .build_config_json()
        .to_string()
        .contains("secrethash")
    );
  }

  #[test]
  fn failed_install_shows_and_speaks_the_last_errors() {
    crate::a11y::enable_for_tests();
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("dory.log");
    std::fs::write(
      &log,
      "[ INFO ] 10:00:00 Partitioning\n\
       [ ERROR ] 10:00:01 first\n\
       pacman noise\n\
       [ ERROR ] 10:00:02 Package installation failed with exit code 1\n",
    )
    .unwrap();
    let mut page = InstallFailed::new(log);
    let said = say_page(&mut page, &mut Installer::new(), 20);
    assert!(
      said.contains("Last errors: first. Package installation failed with exit code 1"),
      "{said}"
    );
  }

  #[test]
  fn unfocused_tables_are_read_as_description() {
    crate::a11y::enable_for_tests();
    let mut t = crate::widget::TableWidget::new(
      "Users",
      vec![Constraint::Length(10), Constraint::Length(10)],
      vec!["Name".into(), "Groups".into()],
      vec![vec!["alice".into(), "wheel".into()]],
    );
    t.unfocus();
    let mut term = Terminal::new(TestBackend::new(40, 8)).unwrap();
    term
      .draw(|f| {
        crate::a11y::begin_frame();
        t.render(f, f.area());
      })
      .unwrap();
    assert_eq!(
      crate::a11y::utterance_for_tests(),
      "Users. Name alice, Groups wheel"
    );
  }

  #[test]
  fn display_manager_page_explains_which_login_speaks_once() {
    crate::a11y::enable_for_tests();
    let mut installer = Installer::new();
    installer.basesystem = Some("Athena Arch".into());
    let mut page = ListPage::new(ListKind::DisplayManager, &installer);
    let first = say_page(&mut page, &mut installer, 30);
    assert!(first.ends_with("Orca starts after you log in."), "{first}");
    assert!(
      first.contains("Select Display Manager. Ly, 1 of"),
      "{first}"
    );
    page.handle_input(&mut installer, key(KeyCode::Down));
    let next = say_page(&mut page, &mut installer, 30);
    assert!(!next.contains("Note:"), "said once: {next}");
  }

  #[test]
  fn spoken_install_marks_the_config_for_speech() {
    crate::a11y::enable_for_tests();
    let mut installer = Installer::new();
    assert_eq!(
      installer.build_config_json()["config"]["accessibility"],
      true
    );
  }

  #[test]
  fn hostname_page_reads_the_explanation_before_the_field() {
    crate::a11y::enable_for_tests();
    let mut page = Hostname::new();
    let said = say_page(&mut page, &mut Installer::new(), 30);
    assert!(
      said.starts_with("The hostname is a unique identifier")
        && said.ends_with("Set Hostname. blank, e.g. 'my-computer'"),
      "{said}"
    );
  }

  #[test]
  fn main_menu_speaks_item_position_and_description_then_only_changes() {
    crate::a11y::enable_for_tests();
    let mut installer = Installer::new();
    let mut menu = Menu::new();
    let first = say(&mut menu, &mut installer);
    assert!(
      first.starts_with("Base System. Select the base system")
        && first.ends_with("long-term goals. Main Menu. Base System, 1 of 12"),
      "{first}"
    );
    menu.handle_input(&mut installer, key(KeyCode::Down));
    let second = say(&mut menu, &mut installer);
    assert!(
      second.starts_with("Timezone. Select the timezone") && second.ends_with("Timezone, 2 of 12"),
      "{second}"
    );
    assert!(
      !second.contains("Main Menu"),
      "region is not repeated: {second}"
    );

    // Jump to the buttons and press Done with nothing configured.
    menu.handle_input(&mut installer, key(KeyCode::End));
    let buttons = say(&mut menu, &mut installer);
    assert!(buttons.starts_with("Required Config"), "{buttons}");
    assert!(buttons.ends_with("Done, button"), "{buttons}");
    menu.handle_input(&mut installer, key(KeyCode::Enter));
    let refused = say(&mut menu, &mut installer);
    assert!(
      refused.starts_with("Cannot continue. Not configured yet: Base System, Timezone"),
      "{refused}"
    );
  }
}
