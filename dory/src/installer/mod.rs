use std::{
  fs::{File, OpenOptions},
  collections::VecDeque,
  fmt::{Debug, Display},
  io::{Read,Write},
  net::TcpStream,
  path::PathBuf,
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
use base64::Engine as _;
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use serde_json::{json, from_value, to_string_pretty, Value};
use tempfile::NamedTempFile;
use shared::{
  keyboard::{resolve, KEYMAPS},
};

use crate::{
  command,
  drives::{Disk, DiskItem, part_table},
  installer::{users::User},
  split_hor, split_vert, styled_block, ui_back, ui_close, ui_down, ui_enter, ui_left, ui_right,
  ui_up,
  widget::{
    Button, ConfigWidget, FancyTicker, HelpModal, InfoBox, InstallSteps, LineEditor, LogBox,
    StrList, WidgetBox, WidgetBoxBuilder,
  },
};

const HIGHLIGHT: Option<(Color, Modifier)> = Some((Color::Yellow, Modifier::BOLD));

pub mod drivepages;
pub mod users;
use drivepages::Drives;
use users::UserAccounts;

/// Add syntax highlighting to JSON code using the bat tool
///
/// Useful for displaying formatted JSON configurations in the UI
fn highlight_json(content: &str) -> anyhow::Result<String> {
  // Spawn bat with JSON syntax highlighting
  let mut bat_child = Command::new("bat")
    .arg("-p") // Plain output (no line numbers)
    .arg("-f") // Force colored output
    .arg("-l")
    .arg("json") // Use JSON syntax highlighting
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .spawn()?;
  if let Some(stdin) = bat_child.stdin.as_mut() {
    use std::io::Write;
    stdin.write_all(content.as_bytes())?;
  }

  let output = bat_child.wait_with_output()?;
  if output.status.success() {
    let highlighted = String::from_utf8(output.stdout)?;
    Ok(highlighted)
  } else {
    let err = String::from_utf8_lossy(&output.stderr);
    Err(anyhow::anyhow!("bat failed: {err}"))
  }
}

/// Decode base64+gzip into a String.
/// Also converts literal "\x1b" sequences into real escapes.
pub fn decode_logo(encoded: &str) -> String {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .expect("Invalid base64");

    let mut gz = GzDecoder::new(&bytes[..]);
    let mut out = String::new();
    gz.read_to_string(&mut out).expect("Decompression failed");

    out.replace("\\x1b", "\x1b")
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Installer {
  pub basesystem: Option<String>,
  pub language: Option<String>,
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

  /// Used as an escape hatch for inter-page communication
  /// If you can't find a good way to pass a value from one page to another
  /// Store it here, and use mem::take() on it in the receiving page
  pub shared_register: Option<Value>,
  pub dry_run: bool,
}

impl Installer {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn has_all_requirements(&self) -> bool {
    self.root_passwd_hash.is_some()
      && !self.users.is_empty()
      && self.drive_config.is_some()
      && self.basesystem.is_some()
  }
  pub fn make_drive_config_display(&mut self) {
    let Some(drive) = &self.drive_config else {
      self.drive_config_display = None;
      return;
    };
    self.drive_config_display = Some(drive.layout().to_vec())
  }

  pub fn to_json(&mut self) -> anyhow::Result<Value> {
    // Create the installer configuration JSON
    // This is used as an intermediate representation before being serialized into
    // Dory backend
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
      "extra_packages": Some(&self.extra_packages)
    });

    // drive configuration if present
    let drv_cfg = self.drive_config.as_mut().map(|d| d.as_disko_cfg());

    let config = json!({
      "config": sys_config,
      "drives": drv_cfg,
    });

    Ok(config)
  }

  pub fn from_json(json: Value) -> anyhow::Result<Self> {
    from_value(json)
      .map_err(|e| anyhow::anyhow!("Failed to deserialize installer config: {e}"))
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
  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    (
      "Help".to_string(),
      vec![Line::from("No help available for this page.")],
    )
  }

  /// This is used as an escape hatch for pages that need to send a signal
  /// without user input This method is called on every redraw
  fn signal(&self) -> Option<Signal> {
    None
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuPages {
  BaseSys,
  Timezone,
  //,
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
  pub fn all_pages() -> &'static [MenuPages] {
    &[
      MenuPages::BaseSys,
      MenuPages::Timezone,
      //MenuPages::Language,
      MenuPages::KeyboardLayout,
      MenuPages::Locale,
      MenuPages::Drives,
      MenuPages::Hostname,
      MenuPages::RootPassword,
      MenuPages::UserAccounts,
      MenuPages::DesktopEnvironment,
      MenuPages::DisplayManager,
      MenuPages::Design,
    ]
  }
  pub fn supported_pages() -> &'static [MenuPages] {
    &[
      MenuPages::BaseSys,
      MenuPages::Timezone,
      //MenuPages::Language,
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
      //MenuPages::Language => "Language",
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
  /// Get the display widget for this page, if any
  pub fn display_widget(self, installer: &mut Installer) -> Option<Box<dyn ConfigWidget>> {
    match self {
      MenuPages::BaseSys => BaseSys::display_widget(installer),
      MenuPages::Timezone => Timezone::display_widget(installer),
      //MenuPages::Language => Language::display_widget(installer),
      MenuPages::KeyboardLayout => KeyboardLayout::display_widget(installer),
      MenuPages::Locale => Locale::display_widget(installer),
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
      MenuPages::DesktopEnvironment => DesktopEnvironment::display_widget(installer),
      MenuPages::DisplayManager => DisplayManager::display_widget(installer),
      MenuPages::Design => Design::display_widget(installer),
      MenuPages::ExtraPackages => ExtraPackages::display_widget(installer),
    }
  }

  /// Get the page info (title and description) for this page
  pub fn page_info<'a>(self) -> (String, Vec<Line<'a>>) {
    match self {
      MenuPages::BaseSys => BaseSys::page_info(),
      MenuPages::Timezone => Timezone::page_info(),
      //MenuPages::Language => Language::page_info(),
      MenuPages::KeyboardLayout => KeyboardLayout::page_info(),
      MenuPages::Locale => Locale::page_info(),
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
      MenuPages::DesktopEnvironment => DesktopEnvironment::page_info(),
      MenuPages::DisplayManager => DisplayManager::page_info(),
      MenuPages::Design => Design::page_info(),
      MenuPages::ExtraPackages => ExtraPackages::page_info(),
    }
  }

  /// Navigate to the page - returns a Signal to push the appropriate page
  pub fn navigate(self, installer: &mut Installer) -> Signal {
    match self {
      MenuPages::BaseSys => Signal::Push(Box::new(BaseSys::new())),
      MenuPages::Timezone => Signal::Push(Box::new(Timezone::new())),
      //MenuPages::Language => Signal::Push(Box::new(Language::new())),
      MenuPages::KeyboardLayout => Signal::Push(Box::new(KeyboardLayout::new())),
      MenuPages::Locale => Signal::Push(Box::new(Locale::new())),
      MenuPages::Drives => Signal::Push(Box::new(Drives::new())),
      MenuPages::Hostname => Signal::Push(Box::new(Hostname::new())),
      MenuPages::RootPassword => Signal::Push(Box::new(RootPassword::new())),
      MenuPages::UserAccounts => Signal::Push(Box::new(UserAccounts::new(installer.users.clone()))),
      MenuPages::DesktopEnvironment => Signal::Push(Box::new(
        DesktopEnvironment::new_for(installer.basesystem.as_deref())
      )),
      MenuPages::DisplayManager => Signal::Push(Box::new(DisplayManager::new())),
      MenuPages::Design => Signal::Push(Box::new(Design::new())),
      MenuPages::ExtraPackages => Signal::Push(Box::new(ExtraPackages::new())),
    }
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
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate menu options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select and configure option"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Tab, End, G"),
        (None, " - Move to action buttons"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Home, g"),
        (None, " - Return to menu options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "q"),
        (None, " - Quit installer"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
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
  ) -> InfoBox<'_> {
    let mut lines = vec![];
    if installer.basesystem.is_none() {
      lines.push(vec![(
        Some((Color::Red, Modifier::BOLD)),
        " - Base System",
      )]);
    }
    if installer.timezone.is_none() {
      lines.push(vec![(
        Some((Color::Red, Modifier::BOLD)),
        " - Timezone",
      )]);
    }
    if installer.keyboard_layout.is_none() {
      lines.push(vec![(
        Some((Color::Red, Modifier::BOLD)),
        " - Keyboard Layout",
      )]);
    }
    if installer.locale.is_none() {
      lines.push(vec![(
        Some((Color::Red, Modifier::BOLD)),
        " - Locale",
      )]);
    }
    if installer.drives.is_empty() || installer.drive_config.is_none() {
      lines.push(vec![(
        Some((Color::Red, Modifier::BOLD)),
        " - Drive Configuration",
      )]);
    }
    if installer.hostname.is_none() {
      lines.push(vec![(
        Some((Color::Red, Modifier::BOLD)),
        " - Hostname",
      )]);
    }
    if installer.root_passwd_hash.is_none() {
      lines.push(vec![(
        Some((Color::Red, Modifier::BOLD)),
        " - Root Password",
      )]);
    }
    if installer.users.is_empty() {
      lines.push(vec![(
        Some((Color::Red, Modifier::BOLD)),
        " - At least one User Account",
      )]);
    }
    if installer.desktop_environment.is_none() {
      lines.push(vec![(
        Some((Color::Red, Modifier::BOLD)),
        " - Desktop Environment",
      )]);
    }
    if installer.display_manager.is_none() {
      lines.push(vec![(
        Some((Color::Red, Modifier::BOLD)),
        " - Display Manager",
      )]);
    }
    if installer.design.is_none() {
      lines.push(vec![(
        Some((Color::Red, Modifier::BOLD)),
        " - Design",
      )]);
    }
    if lines.is_empty() {
      lines.push(vec![(
        Some((Color::Green, Modifier::BOLD)),
        "All required options have been configured!",
      )]);
    } else {
      lines.insert(
        0,
        vec![(
          None,
          "The following required options are not yet configured:",
        )],
      );
      lines.push(vec![(None, "Please configure them before proceeding.")]);
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
      // genuinely insane that this scoping trickery is actually necessary here
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

  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate menu options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select and configure option"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Tab, End, G"),
        (None, " - Move to action buttons"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Home, g"),
        (None, " - Return to menu options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "q"),
        (None, " - Quit installer"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Required options are shown in red when not configured.",
      )],
      vec![(None, "Configure all required options before proceeding.")],
    ]);
    ("Main Menu".to_string(), help_content)
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
                match ConfigPreview::new(installer) {
                  Ok(preview) => Signal::Push(Box::new(preview)),
                  Err(e) => Signal::Error(anyhow::anyhow!(
                    "Failed to generate configuration preview: {e}"
                  )),
                }
              } else {
                self.border_flash_timer = 6;
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
/*
      MenuPages::Language,
      MenuPages::KeyboardLayout,
      MenuPages::Locale,
      MenuPages::Drives,
      MenuPages::Hostname,
      MenuPages::RootPassword,
      MenuPages::UserAccounts,
      MenuPages::DesktopEnvironment,
      MenuPages::Timezone,
*/

pub struct Language {
  langs: StrList,
  help_modal: HelpModal<'static>,
}

impl Language {
  pub fn new() -> Self {
    let languages = ["English"]
      .iter()
      .map(|s| s.to_string())
      .collect::<Vec<_>>();
    let mut langs = StrList::new("Select Language", languages);
    langs.focus();
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate language options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select language and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(None, "Select the language to be used for your system.")],
    ]);
    let help_modal = HelpModal::new("Language", help_content);
    Self { langs, help_modal }
  }
  pub fn display_widget(installer: &mut Installer) -> Option<Box<dyn ConfigWidget>> {
    installer.language.clone().map(|s| {
      let ib = InfoBox::new(
        "",
        styled_block(vec![
          vec![(None, "Current language set to:")],
          vec![(HIGHLIGHT, &s)],
        ]),
      );
      Box::new(ib) as Box<dyn ConfigWidget>
    })
  }
  pub fn page_info<'a>() -> (String, Vec<Line<'a>>) {
    (
      "Language".to_string(),
      styled_block(vec![vec![(
        None,
        "Select the language to be used for your system.",
      )]]),
    )
  }
}

impl Default for Language {
  fn default() -> Self {
    Self::new()
  }
}

impl Page for Language {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let chunks = split_vert!(area, 1, [Constraint::Percentage(100)]);
    self.langs.render(f, chunks[0]);
    self.help_modal.render(f, area);
  }

  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate language options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select language and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(None, "Select the language to be used for your system.")],
    ]);
    ("Language".to_string(), help_content)
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
      ui_back!() => Signal::Pop,
      KeyCode::Enter => {
        installer.language = Some(self.langs.items[self.langs.selected_idx].clone());
        Signal::Pop
      }
      _ => self.langs.handle_input(event),
    }
  }
}

pub struct KeyboardLayout {
  layouts: StrList,
  search_bar: LineEditor,
  search_focused: bool,
  help_modal: HelpModal<'static>,
}

impl KeyboardLayout {
  pub fn new() -> Self {
    let items = KEYMAPS.iter().map(|k| k.label.to_string()).collect::<Vec<_>>();
    let layouts = StrList::new("Select Keyboard Layout", items);

    let mut search_bar = LineEditor::new("Search (press '/' to focus)", Some("Type to filter..."));
    search_bar.focus();

    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate keyboard layout options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select keyboard layout and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "/"),
        (None, " - Focus search bar")
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Choose the keyboard layout that matches your physical keyboard.",
      )],
    ]);
    let help_modal = HelpModal::new("Keyboard Layout", help_content);
    Self {
      layouts,
      search_bar,
      search_focused: true,
      help_modal,
    }
  }
  pub fn selected_keymap_id(&self) -> Option<&'static str> {
      let selected_label = self.layouts.selected_item()?;
      KEYMAPS.iter()
          .find(|k| k.label == selected_label)
          .map(|k| k.id)
  }
  pub fn display_widget(installer: &mut Installer) -> Option<Box<dyn ConfigWidget>> {
      installer.keyboard_layout.clone().map(|id| {
          // try to resolve; fall back to the raw id if not found
          let pretty = resolve(&id).map(|k| k.label).unwrap_or_else(|| id.as_str());
          let ib = InfoBox::new(
              "",
              styled_block(vec![
                  vec![(None, "Current keyboard layout set to:")],
                  vec![(HIGHLIGHT, pretty)],
              ]),
          );
          Box::new(ib) as Box<dyn ConfigWidget>
      })
  }
  pub fn page_info<'a>() -> (String, Vec<Line<'a>>) {
    (
      "Keyboard Layout".to_string(),
      styled_block(vec![vec![(
        None,
        "Choose the keyboard layout that matches your physical keyboard.",
      )]]),
    )
  }
}

impl Default for KeyboardLayout {
  fn default() -> Self {
    Self::new()
  }
}

impl Page for KeyboardLayout {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let chunks = split_vert!(area, 0, [Constraint::Length(3), Constraint::Min(0)]);
    self.search_bar.render(f, chunks[0]);
    self.layouts.render(f, chunks[1]);
    self.help_modal.render(f, area);
  }

  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate keyboard layout options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select keyboard layout and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Choose the keyboard layout that matches your physical keyboard.",
      )],
    ]);
    ("Keyboard Layout".to_string(), help_content)
  }

  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Char('?') => { self.help_modal.toggle(); return Signal::Wait; }
      ui_close!() if self.help_modal.visible => { self.help_modal.hide(); return Signal::Wait; }
      _ if self.help_modal.visible => return Signal::Wait,
      ui_back!() => return Signal::Pop,

      // Focus search bar
      KeyCode::Char('/') if !self.search_focused => {
        self.search_focused = true;
        self.search_bar.focus();
        self.search_bar.clear();
        return Signal::Wait;
      }
      _ => {}
    }

    if self.search_focused {
      match event.code {
        KeyCode::Esc => {
          self.search_bar.clear();
          self.layouts.set_filter(None::<String>);
          self.search_bar.unfocus();
          self.search_focused = false;
          self.layouts.focus();
          return Signal::Wait;
        }
        KeyCode::Enter | KeyCode::Tab | KeyCode::Down => {
          self.search_bar.unfocus();
          self.search_focused = false;
          self.layouts.focus();
          return Signal::Wait;
        }
        _ => {
          let _ = self.search_bar.handle_input(event);
          let text = self.search_bar
            .get_value()
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default();
          if text.is_empty() {
            self.layouts.set_filter(None::<String>);
          } else {
            self.layouts.set_filter(Some(text));
          }
          return Signal::Wait;
        }
      }
    }

    // List-focused behavior
    match event.code {
      KeyCode::Enter => {
        installer.keyboard_layout = self.selected_keymap_id().map(str::to_string);
        Signal::Pop
      }
      ui_up!() => { if !self.layouts.previous_item() { self.layouts.last_item(); } Signal::Wait }
      ui_down!() => { if !self.layouts.next_item() { self.layouts.first_item(); } Signal::Wait }
      _ => self.layouts.handle_input(event),
    }
  }
}

pub struct Locale {
  locales: StrList,
  search_bar: LineEditor,
  search_focused: bool,
  help_modal: HelpModal<'static>,
}

impl Locale {
  pub fn new() -> Self {
    let locales = vec![
      "aa_DJ.UTF-8 UTF-8",
      "af_ZA.UTF-8 UTF-8",
      "an_ES.UTF-8 UTF-8",
      "ar_AE.UTF-8 UTF-8",
      "ar_BH.UTF-8 UTF-8",
      "ar_DZ.UTF-8 UTF-8",
      "ar_EG.UTF-8 UTF-8",
      "ar_IQ.UTF-8 UTF-8",
      "ar_JO.UTF-8 UTF-8",
      "ar_KW.UTF-8 UTF-8",
      "ar_LB.UTF-8 UTF-8",
      "ar_LY.UTF-8 UTF-8",
      "ar_MA.UTF-8 UTF-8",
      "ar_OM.UTF-8 UTF-8",
      "ar_QA.UTF-8 UTF-8",
      "ar_SA.UTF-8 UTF-8",
      "ar_SD.UTF-8 UTF-8",
      "ar_SY.UTF-8 UTF-8",
      "ar_TN.UTF-8 UTF-8",
      "ar_YE.UTF-8 UTF-8",
      "ast_ES.UTF-8 UTF-8",
      "be_BY.UTF-8 UTF-8",
      "bhb_IN.UTF-8 UTF-8",
      "bg_BG.UTF-8 UTF-8",
      "br_FR.UTF-8 UTF-8",
      "bs_BA.UTF-8 UTF-8",
      "ca_AD.UTF-8 UTF-8",
      "ca_ES.UTF-8 UTF-8",
      "ca_FR.UTF-8 UTF-8",
      "ca_IT.UTF-8 UTF-8",
      "cs_CZ.UTF-8 UTF-8",
      "cy_GB.UTF-8 UTF-8",
      "da_DK.UTF-8 UTF-8",
      "de_AT.UTF-8 UTF-8",
      "de_BE.UTF-8 UTF-8",
      "de_CH.UTF-8 UTF-8",
      "de_DE.UTF-8 UTF-8",
      "de_IT.UTF-8 UTF-8",
      "de_LI.UTF-8 UTF-8",
      "de_LU.UTF-8 UTF-8",
      "el_GR.UTF-8 UTF-8",
      "el_CY.UTF-8 UTF-8",
      "en_AU.UTF-8 UTF-8",
      "en_BW.UTF-8 UTF-8",
      "en_CA.UTF-8 UTF-8",
      "en_DK.UTF-8 UTF-8",
      "en_GB.UTF-8 UTF-8",
      "en_HK.UTF-8 UTF-8",
      "en_IE.UTF-8 UTF-8",
      "en_NZ.UTF-8 UTF-8",
      "en_PH.UTF-8 UTF-8",
      "en_SC.UTF-8 UTF-8",
      "en_SG.UTF-8 UTF-8",
      "en_US.UTF-8 UTF-8",
      "en_ZA.UTF-8 UTF-8",
      "en_ZW.UTF-8 UTF-8",
      "es_AR.UTF-8 UTF-8",
      "es_BO.UTF-8 UTF-8",
      "es_CL.UTF-8 UTF-8",
      "es_CO.UTF-8 UTF-8",
      "es_CR.UTF-8 UTF-8",
      "es_DO.UTF-8 UTF-8",
      "es_EC.UTF-8 UTF-8",
      "es_ES.UTF-8 UTF-8",
      "es_GT.UTF-8 UTF-8",
      "es_HN.UTF-8 UTF-8",
      "es_MX.UTF-8 UTF-8",
      "es_NI.UTF-8 UTF-8",
      "es_PA.UTF-8 UTF-8",
      "es_PE.UTF-8 UTF-8",
      "es_PR.UTF-8 UTF-8",
      "es_PY.UTF-8 UTF-8",
      "es_SV.UTF-8 UTF-8",
      "es_US.UTF-8 UTF-8",
      "es_UY.UTF-8 UTF-8",
      "es_VE.UTF-8 UTF-8",
      "et_EE.UTF-8 UTF-8",
      "eu_ES.UTF-8 UTF-8",
      "fo_FO.UTF-8 UTF-8",
      "fr_BE.UTF-8 UTF-8",
      "fr_CA.UTF-8 UTF-8",
      "fr_CH.UTF-8 UTF-8",
      "fr_FR.UTF-8 UTF-8",
      "fr_LU.UTF-8 UTF-8",
      "ga_IE.UTF-8 UTF-8",
      "gd_GB.UTF-8 UTF-8",
      "gl_ES.UTF-8 UTF-8",
      "gv_GB.UTF-8 UTF-8",
      "he_IL.UTF-8 UTF-8",
      "hr_HR.UTF-8 UTF-8",
      "hsb_DE.UTF-8 UTF-8",
      "hu_HU.UTF-8 UTF-8",
      "id_ID.UTF-8 UTF-8",
      "is_IS.UTF-8 UTF-8",
      "it_CH.UTF-8 UTF-8",
      "it_IT.UTF-8 UTF-8",
      "ja_JP.UTF-8 UTF-8",
      "ka_GE.UTF-8 UTF-8",
      "kk_KZ.UTF-8 UTF-8",
      "kl_GL.UTF-8 UTF-8",
      "ko_KR.UTF-8 UTF-8",
      "kw_GB.UTF-8 UTF-8",
      "lg_UG.UTF-8 UTF-8",
      "lt_LT.UTF-8 UTF-8",
      "ltg_LV.UTF-8 UTF-8",
      "lv_LV.UTF-8 UTF-8",
      "mg_MG.UTF-8 UTF-8",
      "mi_NZ.UTF-8 UTF-8",
      "mk_MK.UTF-8 UTF-8",
      "ms_MY.UTF-8 UTF-8",
      "mt_MT.UTF-8 UTF-8",
      "nb_NO.UTF-8 UTF-8",
      "nl_BE.UTF-8 UTF-8",
      "nl_NL.UTF-8 UTF-8",
      "nn_NO.UTF-8 UTF-8",
      "oc_FR.UTF-8 UTF-8",
      "om_KE.UTF-8 UTF-8",
      "pl_PL.UTF-8 UTF-8",
      "pt_BR.UTF-8 UTF-8",
      "pt_PT.UTF-8 UTF-8",
      "ro_RO.UTF-8 UTF-8",
      "ru_RU.UTF-8 UTF-8",
      "ru_UA.UTF-8 UTF-8",
      "sk_SK.UTF-8 UTF-8",
      "sl_SI.UTF-8 UTF-8",
      "so_DJ.UTF-8 UTF-8",
      "so_KE.UTF-8 UTF-8",
      "so_SO.UTF-8 UTF-8",
      "sq_AL.UTF-8 UTF-8",
      "st_ZA.UTF-8 UTF-8",
      "sv_FI.UTF-8 UTF-8",
      "sv_SE.UTF-8 UTF-8",
      "tcy_IN.UTF-8 UTF-8",
      "tg_TJ.UTF-8 UTF-8",
      "th_TH.UTF-8 UTF-8",
      "tl_PH.UTF-8 UTF-8",
      "tr_CY.UTF-8 UTF-8",
      "tr_TR.UTF-8 UTF-8",
      "uk_UA.UTF-8 UTF-8",
      "uz_UZ.UTF-8 UTF-8",
      "wa_BE.UTF-8 UTF-8",
      "xh_ZA.UTF-8 UTF-8",
      "yi_US.UTF-8 UTF-8",
      "zh_CN.UTF-8 UTF-8",
      "zh_HK.UTF-8 UTF-8",
      "zh_SG.UTF-8 UTF-8",
      "zh_TW.UTF-8 UTF-8",
      "zu_ZA.UTF-8 UTF-8",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect::<Vec<_>>();
    let locales = StrList::new("Select Locale", locales);

    let mut search_bar = LineEditor::new("Search (press '/' to focus)", Some("Type to filter..."));
    search_bar.focus();

    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate locale options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select locale and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "/"),
        (None, " - Focus search bar")
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(None, "Set the locale for your system, which determines")],
      vec![(None, "language and regional settings.")],
    ]);
    let help_modal = HelpModal::new("Locale", help_content);
    Self {
      locales,
      search_bar,
      search_focused: true,
      help_modal,
    }
  }
  pub fn display_widget(installer: &mut Installer) -> Option<Box<dyn ConfigWidget>> {
    installer.locale.clone().map(|s| {
      let ib = InfoBox::new(
        "",
        styled_block(vec![
          vec![(None, "Current locale set to:")],
          vec![(HIGHLIGHT, &s)],
        ]),
      );
      Box::new(ib) as Box<dyn ConfigWidget>
    })
  }
  pub fn page_info<'a>() -> (String, Vec<Line<'a>>) {
    (
      "Locale".to_string(),
      styled_block(vec![vec![(
        None,
        "Set the locale for your system, which determines language and regional settings.",
      )]]),
    )
  }
}

impl Default for Locale {
  fn default() -> Self {
    Self::new()
  }
}

impl Page for Locale {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let chunks = split_vert!(area, 0, [Constraint::Length(3), Constraint::Min(0)]);
    self.search_bar.render(f, chunks[0]);
    self.locales.render(f, chunks[1]);
    self.help_modal.render(f, area);
  }

  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate locale options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select locale and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(None, "Set the locale for your system, which determines")],
      vec![(None, "language and regional settings.")],
    ]);
    ("Locale".to_string(), help_content)
  }

  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Char('?') => { self.help_modal.toggle(); return Signal::Wait; }
      ui_close!() if self.help_modal.visible => { self.help_modal.hide(); return Signal::Wait; }
      _ if self.help_modal.visible => return Signal::Wait,
      ui_back!() => return Signal::Pop,

      // Focus search bar
      KeyCode::Char('/') if !self.search_focused => {
        self.search_focused = true;
        self.search_bar.focus();
        self.search_bar.clear();
        return Signal::Wait;
      }
      _ => {}
    }

    if self.search_focused {
      match event.code {
        KeyCode::Esc => {
          self.search_bar.clear();
          self.locales.set_filter(None::<String>);
          self.search_bar.unfocus();
          self.search_focused = false;
          self.locales.focus();
          return Signal::Wait;
        }
        KeyCode::Enter | KeyCode::Tab | KeyCode::Down => {
          self.search_bar.unfocus();
          self.search_focused = false;
          self.locales.focus();
          return Signal::Wait;
        }
        _ => {
          let _ = self.search_bar.handle_input(event);
          let text = self.search_bar
            .get_value()
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default();
          if text.is_empty() {
            self.locales.set_filter(None::<String>);
          } else {
            self.locales.set_filter(Some(text));
          }
          return Signal::Wait;
        }
      }
    }

    // List-focused behavior
    match event.code {
      ui_up!() => { if !self.locales.previous_item() { self.locales.last_item(); } Signal::Wait }
      ui_down!() => { if !self.locales.next_item() { self.locales.first_item(); } Signal::Wait }
      KeyCode::Enter => {
        // Use selected_item() so it’s correct with filtering
        if let Some(sel) = self.locales.selected_item() {
          installer.locale = Some(sel.clone());
          return Signal::Pop;
        }
        Signal::Wait
      }
      _ => self.locales.handle_input(event),
    }
  }
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
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Save hostname and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "←/→"),
        (None, " - Move cursor"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Home/End"),
        (None, " - Jump to beginning/end"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Backspace/Del"),
        (None, " - Delete characters"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
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
    installer.hostname.clone().map(|s| {
      let ib = InfoBox::new(
        "",
        styled_block(vec![
          vec![(None, "Current hostname set to:")],
          vec![(HIGHLIGHT, &s)],
        ]),
      );
      Box::new(ib) as Box<dyn ConfigWidget>
    })
  }
  pub fn page_info<'a>() -> (String, Vec<Line<'a>>) {
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

  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Save hostname and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "←/→"),
        (None, " - Move cursor"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Home/End"),
        (None, " - Jump to beginning/end"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Backspace/Del"),
        (None, " - Delete characters"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Set a unique hostname for your computer on the network.",
      )],
    ]);
    ("Hostname".to_string(), help_content)
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
        let hostname = self
          .input
          .get_value()
          .unwrap()
          .as_str()
          .unwrap()
          .trim()
          .to_string();
        if !hostname.is_empty() {
          installer.hostname = Some(hostname);
        }
        Signal::Pop
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
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Move to next field or save when complete"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Tab"),
        (None, " - Switch between password fields"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "←/→"),
        (None, " - Move cursor"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Home/End"),
        (None, " - Jump to beginning/end"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Backspace/Del"),
        (None, " - Delete characters"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
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
  pub fn page_info<'a>() -> (String, Vec<Line<'a>>) {
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
  pub fn mkpasswd(passwd: String) -> anyhow::Result<String> {
    let mut child = Command::new("mkpasswd")
      .arg("--method=sha512crypt")
      .arg("--stdin")
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
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

  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Move to next field or save when complete"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Tab"),
        (None, " - Switch between password fields"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "←/→"),
        (None, " - Move cursor"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Home/End"),
        (None, " - Jump to beginning/end"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Backspace/Del"),
        (None, " - Delete characters"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(None, "Set a strong root password for system security.")],
    ]);
    ("Root Password".to_string(), help_content)
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
          let passwd = self
            .input
            .get_value()
            .unwrap()
            .as_str()
            .unwrap()
            .trim()
            .to_string();
          let confirm = self
            .confirm
            .get_value()
            .unwrap()
            .as_str()
            .unwrap()
            .trim()
            .to_string();
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
            match Self::mkpasswd(passwd) {
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

pub struct BaseSys {
  systems: StrList,
  help_modal: HelpModal<'static>,
}

impl BaseSys {
  pub fn new() -> Self {
    let systems = [
      "Athena Arch",
      "Athena Nix",
      //"Athena Fedora",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect::<Vec<_>>();
    let mut systems = StrList::new("Select Base System", systems);
    systems.focus();
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate base system options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select base system and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Select the base system for your environment.",
      )],
    ]);
    let help_modal = HelpModal::new("Base System", help_content);
    Self {
      systems,
      help_modal,
    }
  }
  pub fn get_basesystem_info<'a>(name: &str) -> InfoBox<'a> {
    match name {
      "Athena Arch" => {
        
          let logo = decode_logo(
              "H4sIAAAAAAAAA52TUQ6EIAxE/z0FRyWuGg+waTRovFtPIilxV2Va0H7SmddCi3MwmLpGy2xMI9OM80d6NERXjaFEQl3O1DJ9mTzTUuk5FejFviJvbtQbqyj7xIwRzwmQ8xaDYCVWkLloowFEExgOjUY7o+zW/qqKtoovptIG2bCO6RP3rKnm/a5qXfbWpgJNz+/vm5SOddyVFqWTwyGfx+cEAYSsMvCn7Yi2HWcEzBuVBAAA"
          );
          let mut blocks = vec![
              vec![
                  (HIGHLIGHT, "Athena Arch"),
                  (None, " is based on "),
                  (HIGHLIGHT, "Arch Linux"),
                  (None, ", offering a "),
                  (HIGHLIGHT, "rolling-release, lightweight environment"),
                  (None, " with access to the "),
                  (HIGHLIGHT, "AUR and bleeding-edge packages"),
                  (None, "."),
              ],
              vec![
                  (None, "It ships with the "),
                  (HIGHLIGHT, "Pacman package manager"),
                  (None, " and integrates a fork of the "),
                  (HIGHLIGHT, "BlackArch repository"),
                  (None, ", providing "),
                  (HIGHLIGHT, "2800+ pentesting tools"),
                  (None, " that are "),
                  (HIGHLIGHT, "GPG-signed for trust"),
                  (None, "."),
              ],
              vec![
                  (None, "Athena Arch is "),
                  (HIGHLIGHT, "highly customizable and fast to update"),
                  (None, " but requires "),
                  (HIGHLIGHT, "manual setup and maintenance"),
                  (None, "; ideal for those who prefer "),
                  (HIGHLIGHT, "DIY configuration and bleeding-edge software"),
                  (None, "."),
              ],
          ];

          // append the logo *line by line*
          for line in logo.lines() {
              blocks.push(vec![(None, line)]);
          }

          InfoBox::new("Athena Arch", styled_block(blocks))
        },
      "Athena Nix" => {
        
          let logo = decode_logo(
              "H4sIAAAAAAAAA6WUzQ3DIAyF70zBqDnkkAncWjTqbkzSSlCw8U+tJPIFw/sQz45znl+FR4W9hZLqWbKGlJn8VeH4BVrZCm+SKAwRY7iEEEKKOTMeKMllAjHdo+t34ILE1cOd1sizxnNy2bM8Lh7EY3SKpo/ZteJiPmMym+E+nL1/fXzrCBdqtCPzCAN64bv+U7SjZ4VtxACQAmepDzj416BeftXaizeIaaJWzBgrXveH5pLb+RcIszBtvZF4jlMz9VV/AOenEZ3PBQAA"
          );

          let mut blocks = vec![
              vec![
                (HIGHLIGHT, "Athena Nix"),
                (None, " leverages "),
                (HIGHLIGHT, "NixOS"),
                (None, " to provide a "),
                (HIGHLIGHT, "declarative, reproducible Linux system"),
                (None, " built on the "),
                (HIGHLIGHT, "Nix package manager"),
                (None, "."),
              ],
              vec![
                (None, "It enables "),
                (HIGHLIGHT, "atomic upgrades and rollbacks"),
                (None, ", "),
                (HIGHLIGHT, "isolated dependencies"),
                (None, ", "),
                (HIGHLIGHT, "per-user profiles"),
                (None, ", and "),
                (HIGHLIGHT, "secure package management"),
                (None, " with automatic CVE checks."),
              ],
              vec![
                (None, "Athena Nix offers "),
                (HIGHLIGHT, "consistency across systems and CI-friendly workflows"),
                (None, " but comes with a "),
                (HIGHLIGHT, "steeper learning curve and unique paradigms"),
                (None, "; perfect for users who value "),
                (HIGHLIGHT, "determinism, reproducibility, and rollback safety"),
                (None, "."),
              ],
          ];

          // append the logo *line by line*
          for line in logo.lines() {
              blocks.push(vec![(None, line)]);
          }

          InfoBox::new("Athena Nix", styled_block(blocks))
        },
      "Athena Fedora" => {

          let logo = decode_logo(
              "H4sIAAAAAAAAA+NSwAMeTevARDBJLhL1YTUGpyHYNeAwhihTcInRRDOOMFMgwtG4tMIApj5U5cQEO6ZBBHQRGZ0Y5hLSRIah+IOWmHDDrmzo6MehlJSEQzA+cRuDlgBJ1Y0lAaPp4eICAA0CX3KVBAAA"
          );
          let mut blocks = vec![
              vec![
                (HIGHLIGHT, "Athena Fedora"),
                (None, " builds on "),
                (HIGHLIGHT, "Fedora Linux"),
                (None, " to deliver a "),
                (HIGHLIGHT, "stable yet modern environment"),
                (None, " with a focus on "),
                (HIGHLIGHT, "security and upstream collaboration"),
                (None, "."),
              ],
              vec![
                (None, "It uses "),
                (HIGHLIGHT, "DNF and RPM packaging"),
                (None, ", includes "),
                (HIGHLIGHT, "SELinux enabled by default"),
                (None, ", and benefits from "),
                (HIGHLIGHT, "Fedora's 6-month release cadence"),
                (None, " ensuring both stability and freshness."),
              ],
              vec![
                (None, "Athena Fedora is "),
                (HIGHLIGHT, "secure and polished out of the box"),
                (None, " with "),
                (HIGHLIGHT, "containerization and Secure Boot support"),
                (None, ", though it is "),
                (HIGHLIGHT, "less customizable than Arch"),
                (None, "; excellent for those wanting "),
                (HIGHLIGHT, "a dependable and security-hardened workflow"),
                (None, "."),
              ],
          ];

          // append the logo *line by line*
          for line in logo.lines() {
              blocks.push(vec![(None, line)]);
          }

          InfoBox::new("Athena Fedora", styled_block(blocks))
        },
      _ => InfoBox::new(
        "Unknown Base System",
        styled_block(vec![vec![(
          None,
          "No information available for this base system.",
        )]]),
      ),
    }
  }
  pub fn display_widget(installer: &mut Installer) -> Option<Box<dyn ConfigWidget>> {
    installer.basesystem.clone().map(|s| {
      let ib = InfoBox::new(
        "",
        styled_block(vec![
          vec![(None, "Current base system set to:")],
          vec![(HIGHLIGHT, &s)],
        ]),
      );
      Box::new(ib) as Box<dyn ConfigWidget>
    })
  }
  pub fn page_info<'a>() -> (String, Vec<Line<'a>>) {
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
}

impl Default for BaseSys {
  fn default() -> Self {
    Self::new()
  }
}

impl Page for BaseSys {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let vert_chunks = Layout::default()
      .direction(Direction::Vertical)
      .constraints([Constraint::Percentage(30), Constraint::Percentage(70)].as_ref()) // top box and bottom box height of Base System choice
      .split(area);
    let hor_chunks = split_hor!(
      vert_chunks[0],
      1,
      [
        Constraint::Percentage(38),
        Constraint::Length(28),
        Constraint::Percentage(38),
      ]
    );

    self.systems.render(f, hor_chunks[1]);
    
    let selected = self
      .systems
      .items
      .get(self.systems.selected_idx)
      .map(|s| s.as_str())
      .unwrap_or("");
    
    let info_box = Self::get_basesystem_info(selected);
    info_box.render(f, vert_chunks[1]);
    
    self.help_modal.render(f, area);
  }

  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate base system options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select base system and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Select the base system for your environment.",
      )],
    ]);
    ("Base System".to_string(), help_content)
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
      ui_back!() => Signal::Pop,
      KeyCode::Enter => {
        installer.basesystem =
          Some(self.systems.items[self.systems.selected_idx].clone());
        installer.extra_packages.clear(); // Clear the extra pkg list if you select base system
        Signal::Pop
      }
      ui_up!() => {
        if !self.systems.previous_item() {
          self.systems.last_item();
        }
        Signal::Wait
      }
      ui_down!() => {
        if !self.systems.next_item() {
          self.systems.first_item();
        }
        Signal::Wait
      }
      _ => self.systems.handle_input(event),
    }
  }
}

pub struct DesktopEnvironment {
  desktops: StrList,
  help_modal: HelpModal<'static>,
}

impl DesktopEnvironment {
  pub fn new() -> Self {
    let desktops = [
      "GNOME",
      "KDE Plasma",
      "Hyprland",
      "XFCE",
      "Cinnamon",
      "MATE",
      "Bspwm",
      "None",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect::<Vec<_>>();
    let mut desktops = StrList::new("Select Desktop Environment", desktops);
    desktops.focus();
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate desktop environment options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select desktop environment and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Select the desktop environment for your graphical interface.",
      )],
    ]);
    let help_modal = HelpModal::new("Desktop Environment", help_content);
    Self {
      desktops,
      help_modal,
    }
  }
  pub fn new_for(base: Option<&str>) -> Self {
    match base {
      // Exact labels match the BaseSys page
      Some("Athena Arch") => Self::new(), // all options
      Some("Athena Fedora") => Self::with_list(vec![
        "GNOME",
        "KDE Plasma",
        "XFCE",
        "Cinnamon",
        "MATE",
        "None",
      ]),
      Some("Athena Nix") => Self::with_list(vec![
        "GNOME",
        "MATE",
        "Cinnamon",
        "None",
      ]),
      _ => Self::new(), // fallback to all
    }
  }
  fn with_list(items: Vec<&'static str>) -> Self {
    let desktops = items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let mut desktops = StrList::new("Select Desktop Environment", desktops);
    desktops.focus();
    let help_content = styled_block(vec![
      vec![(Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"), (None, " - Navigate desktop environment options")],
      vec![(Some((Color::Yellow, Modifier::BOLD)), "Enter"), (None, " - Select desktop environment and return")],
      vec![(Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"), (None, " - Cancel and return to menu")],
      vec![(Some((Color::Yellow, Modifier::BOLD)), "?"), (None, " - Show this help")],
      vec![(None, "")],
      vec![(None, "Select the desktop environment for your graphical interface.")],
    ]);
    let help_modal = HelpModal::new("Desktop Environment", help_content);
    Self { desktops, help_modal }
  }
  pub fn get_desktop_info<'a>(name: &str) -> InfoBox<'a> {
    match name {
      "GNOME" => InfoBox::new(
        "GNOME",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "GNOME"),
            (None, " is a "),
            (HIGHLIGHT, "modern and popular desktop environment"),
            (None, " that provides a "),
            (HIGHLIGHT, "user-friendly experience"),
            (None, " with a focus on "),
            (HIGHLIGHT, "simplicity and elegance"),
            (None, "."),
          ],
          vec![
            (None, "It features a "),
            (HIGHLIGHT, "clean interface"),
            (None, " with "),
            (HIGHLIGHT, "activities overview"),
            (None, ", "),
            (HIGHLIGHT, "workspaces"),
            (None, ", and extensive "),
            (HIGHLIGHT, "customization options"),
            (None, " through extensions."),
          ],
          vec![
            (None, "GNOME is "),
            (HIGHLIGHT, "resource-intensive"),
            (None, " but offers "),
            (HIGHLIGHT, "excellent accessibility"),
            (None, " and "),
            (HIGHLIGHT, "touch support"),
            (None, "."),
          ],
        ]),
      ),
      "KDE Plasma" => InfoBox::new(
        "KDE Plasma",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "KDE Plasma"),
            (None, " is a "),
            (HIGHLIGHT, "highly customizable desktop environment"),
            (None, " that offers "),
            (HIGHLIGHT, "extensive configuration options"),
            (None, " and a "),
            (HIGHLIGHT, "traditional desktop experience"),
            (None, "."),
          ],
          vec![
            (None, "It provides "),
            (HIGHLIGHT, "powerful widgets"),
            (None, ", "),
            (HIGHLIGHT, "multiple panel layouts"),
            (None, ", and "),
            (HIGHLIGHT, "advanced system settings"),
            (None, " with a familiar Windows-like interface."),
          ],
          vec![
            (None, "KDE Plasma is "),
            (HIGHLIGHT, "feature-rich"),
            (None, " and "),
            (HIGHLIGHT, "resource-efficient"),
            (
              None,
              ", making it suitable for both power users and beginners.",
            ),
          ],
        ]),
      ),
      "Hyprland" => InfoBox::new(
        "Hyprland",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "Hyprland"),
            (None, " is a "),
            (HIGHLIGHT, "dynamic tiling Wayland compositor"),
            (None, " that focuses on "),
            (HIGHLIGHT, "eye candy and customization"),
            (None, "."),
          ],
          vec![
            (None, "It features "),
            (HIGHLIGHT, "beautiful animations"),
            (None, ", "),
            (HIGHLIGHT, "automatic window tiling"),
            (None, ", and "),
            (HIGHLIGHT, "extensive configuration"),
            (None, " through text files."),
          ],
          vec![
            (None, "Hyprland is "),
            (HIGHLIGHT, "highly efficient"),
            (None, " and perfect for users who prefer "),
            (HIGHLIGHT, "keyboard-driven workflows"),
            (None, " and "),
            (HIGHLIGHT, "minimal resource usage"),
            (None, "."),
          ],
        ]),
      ),
      "XFCE" => InfoBox::new(
        "XFCE",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "XFCE"),
            (None, " is a "),
            (HIGHLIGHT, "lightweight and fast desktop environment"),
            (None, " that aims to be "),
            (HIGHLIGHT, "visually appealing and user-friendly"),
            (None, " while being "),
            (HIGHLIGHT, "resource-efficient"),
            (None, "."),
          ],
          vec![
            (None, "It provides a "),
            (HIGHLIGHT, "traditional desktop experience"),
            (None, " with "),
            (HIGHLIGHT, "customizable panels"),
            (None, ", "),
            (HIGHLIGHT, "file manager"),
            (None, ", and "),
            (HIGHLIGHT, "application menu"),
            (None, "."),
          ],
          vec![
            (None, "XFCE is "),
            (HIGHLIGHT, "perfect for older hardware"),
            (None, " or users who want a "),
            (HIGHLIGHT, "simple, stable"),
            (None, " desktop without sacrificing functionality."),
          ],
        ]),
      ),
      "Cinnamon" => InfoBox::new(
        "Cinnamon",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "Cinnamon"),
            (None, " is a "),
            (HIGHLIGHT, "modern desktop environment"),
            (None, " that provides a "),
            (HIGHLIGHT, "familiar and intuitive experience"),
            (None, " similar to traditional desktops."),
          ],
          vec![
            (None, "It features a "),
            (HIGHLIGHT, "taskbar-style panel"),
            (None, ", "),
            (HIGHLIGHT, "system tray"),
            (None, ", and "),
            (HIGHLIGHT, "start menu"),
            (None, " with "),
            (HIGHLIGHT, "smooth animations"),
            (None, " and effects."),
          ],
          vec![
            (None, "Cinnamon balances "),
            (HIGHLIGHT, "modern features"),
            (None, " with "),
            (HIGHLIGHT, "traditional usability"),
            (
              None,
              ", making it great for users transitioning from other operating systems.",
            ),
          ],
        ]),
      ),
      "MATE" => InfoBox::new(
        "MATE",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "MATE"),
            (None, " is a "),
            (HIGHLIGHT, "traditional desktop environment"),
            (None, " that continues the legacy of "),
            (HIGHLIGHT, "GNOME 2"),
            (None, " with a "),
            (HIGHLIGHT, "classic interface"),
            (None, "."),
          ],
          vec![
            (None, "It provides "),
            (HIGHLIGHT, "stability"),
            (None, ", "),
            (HIGHLIGHT, "reliability"),
            (None, ", and "),
            (HIGHLIGHT, "low resource usage"),
            (None, " while maintaining "),
            (HIGHLIGHT, "familiar desktop metaphors"),
            (None, "."),
          ],
          vec![
            (None, "MATE is "),
            (HIGHLIGHT, "ideal for users"),
            (None, " who prefer "),
            (HIGHLIGHT, "conventional desktop layouts"),
            (None, " and "),
            (HIGHLIGHT, "proven workflows"),
            (None, "."),
          ],
        ]),
      ),
      "Bspwm" => InfoBox::new(
        "Bspwm",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "Bspwm"),
            (None, " is a "),
            (HIGHLIGHT, "tiling window manager"),
            (None, " that represents "),
            (HIGHLIGHT, "windows as the leaves of a full binary tree"),
            (None, "."),
          ],
          vec![
            (None, "It offers "),
            (HIGHLIGHT, "manual tiling"),
            (None, " controlled entirely via "),
            (HIGHLIGHT, "external programs"),
            (None, "."),
          ],
          vec![
            (None, "Bspwm emphasizes "),
            (HIGHLIGHT, "simplicity"),
            (None, " and "),
            (HIGHLIGHT, "minimalism"),
            (None, "."),
          ],
        ]),
      ),
      "None" => InfoBox::new(
        "None",
        styled_block(vec![vec![(None, "No desktop environment will be installed.")]]),
      ),
      _ => InfoBox::new(
        "Unknown Desktop Environment",
        styled_block(vec![vec![(
          None,
          "No information available for this desktop environment.",
        )]]),
      ),
    }
  }
  pub fn display_widget(installer: &mut Installer) -> Option<Box<dyn ConfigWidget>> {
    installer.desktop_environment.clone().map(|s| {
      let ib = InfoBox::new(
        "",
        styled_block(vec![
          vec![(None, "Current desktop environment set to:")],
          vec![(HIGHLIGHT, &s)],
        ]),
      );
      Box::new(ib) as Box<dyn ConfigWidget>
    })
  }
  pub fn page_info<'a>() -> (String, Vec<Line<'a>>) {
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
}

impl Default for DesktopEnvironment {
  fn default() -> Self {
    Self::new()
  }
}

impl Page for DesktopEnvironment {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let vert_chunks = Layout::default()
      .direction(Direction::Vertical)
      .constraints([Constraint::Percentage(50), Constraint::Percentage(50)].as_ref())
      .split(area);
    let hor_chunks = split_hor!(
      vert_chunks[0],
      1,
      [
        Constraint::Percentage(38),
        Constraint::Length(28),
        Constraint::Percentage(38),
      ]
    );

    let selected = self
      .desktops
      .items
      .get(self.desktops.selected_idx)
      .map(|s| s.as_str())
      .unwrap_or("");
    let info_box = Self::get_desktop_info(selected);
    self.desktops.render(f, hor_chunks[1]);
    info_box.render(f, vert_chunks[1]);

    self.help_modal.render(f, area);
  }

  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate desktop environment options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select desktop environment and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Select the desktop environment for your graphical interface.",
      )],
    ]);
    ("Desktop Environment".to_string(), help_content)
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
      ui_back!() => Signal::Pop,
      KeyCode::Enter => {
        installer.desktop_environment =
          Some(self.desktops.items[self.desktops.selected_idx].clone());
        Signal::Pop
      }
      ui_up!() => {
        if !self.desktops.previous_item() {
          self.desktops.last_item();
        }
        Signal::Wait
      }
      ui_down!() => {
        if !self.desktops.next_item() {
          self.desktops.first_item();
        }
        Signal::Wait
      }
      _ => self.desktops.handle_input(event),
    }
  }
}

pub struct Design {
  designs: StrList,
  help_modal: HelpModal<'static>,
}

impl Design {
  pub fn new() -> Self {
    let designs = [
      "Cyborg",
      "Frost",
      "Graphite",
      "HackTheBox",
      "RedMoon",
      "Samurai",
      "Sweet",
      "Temple",
      "None",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect::<Vec<_>>();
    let mut designs = StrList::new("Select Design", designs);
    designs.focus();
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate design options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select design and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Select the design for your graphical interface.",
      )],
    ]);
    let help_modal = HelpModal::new("Design", help_content);
    Self {
      designs,
      help_modal,
    }
  }
  pub fn get_design_info<'a>(name: &str) -> InfoBox<'a> {
    match name {
      "Cyborg" => InfoBox::new(
        "Cyborg",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "Cyborg design"),
            (None, " features a "),
            (HIGHLIGHT, "dark cyberpunk aesthetic"),
            (None, " with a striking "),
            (HIGHLIGHT, "monochrome illustration"),
            (None, " of a woman’s face."),
          ],
          vec![
            (None, "Her hair is rendered in a "),
            (HIGHLIGHT, "bright glowing tone"),
            (None, " that contrasts with the "),
            (HIGHLIGHT, "shadowed facial features"),
            (None, " and mechanical details."),
          ],
          vec![
            (None, "A "),
            (HIGHLIGHT, "geometric wireframe cube"),
            (None, " surrounds the figure, giving the environment a "),
            (HIGHLIGHT, "futuristic and surreal atmosphere"),
            (None, "."),
          ],
          vec![
            (None, "Above the figure, there is "),
            (HIGHLIGHT, "stylized Japanese text"),
            (None, " that enhances the "),
            (HIGHLIGHT, "sci-fi and anime-inspired vibe"),
            (None, "."),
          ],
        ]),
      ),
      "Frost" => InfoBox::new(
        "Frost",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "Frost design"),
            (None, " evokes a sense of "),
            (HIGHLIGHT, "quiet elegance"),
            (None, " through its "),
            (HIGHLIGHT, "monochrome palette"),
            (None, " of "),
            (HIGHLIGHT, "icy grays and silver tones"),
            (None, "."),
          ],
          vec![
            (None, "The centerpiece features a "),
            (HIGHLIGHT, "melancholic anime-style portrait"),
            (None, " rendered with delicate highlights that resemble "),
            (HIGHLIGHT, "frozen light reflections"),
            (None, " against a vast, starry backdrop."),
          ],
          vec![
            (None, "Its atmosphere combines "),
            (HIGHLIGHT, "cosmic stillness"),
            (None, " and "),
            (HIGHLIGHT, "fragile beauty"),
            (None, ", giving the impression of "),
            (HIGHLIGHT, "ice meeting starlight"),
            (None, " in a timeless void."),
          ],
          vec![
            (None, "The theme captures both "),
            (HIGHLIGHT, "emotional depth and minimalist design"),
            (None, ", creating an environment that feels "),
            (HIGHLIGHT, "cold yet introspective"),
            (None, ", perfect for those drawn to serene darkness."),
          ],
        ]),
      ),
      "Graphite" => InfoBox::new(
        "Graphite",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "Graphite design"),
            (None, " embraces a "),
            (HIGHLIGHT, "terminal-inspired aesthetic"),
            (None, " with "),
            (HIGHLIGHT, "ASCII art and code fragments"),
            (None, " integrated into the environment."),
          ],
          vec![
            (None, "It combines a "),
            (HIGHLIGHT, "dark background"),
            (None, " with text in "),
            (HIGHLIGHT, "contrasting bright colors"),
            (None, " evoking a "),
            (HIGHLIGHT, "programming and hacker culture vibe"),
            (None, "."),
          ],
          vec![
            (None, "The design conveys a sense of "),
            (HIGHLIGHT, "minimalism"),
            (None, " while celebrating "),
            (HIGHLIGHT, "open-source and coding culture"),
            (None, "."),
          ],
        ]),
      ),
      "HackTheBox" => InfoBox::new(
        "HackTheBox",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "HackTheBox design"),
            (None, " presents a "),
            (HIGHLIGHT, "minimalist approach"),
            (None, " with a "),
            (HIGHLIGHT, "dark solid background"),
            (None, " and a "),
            (HIGHLIGHT, " Hack The Box logo"),
            (None, " at the center."),
          ],
          vec![
            (None, "The cube is rendered in a "),
            (HIGHLIGHT, "bright green tone"),
            (None, " which stands out sharply against the "),
            (HIGHLIGHT, "dark backdrop"),
            (None, ", creating a strong sense of "),
            (HIGHLIGHT, "contrast and focus"),
            (None, "."),
          ],
          vec![
            (None, "This design emphasizes "),
            (HIGHLIGHT, "simplicity, clarity, and modern aesthetics"),
            (None, " while avoiding visual clutter."),
          ],
        ]),
      ),
      "RedMoon" => InfoBox::new(
        "RedMoon",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "RedMoon design"),
            (None, " embody a "),
            (HIGHLIGHT, "dark gothic style"),
            (None, " with "),
            (HIGHLIGHT, "anime-inspired characters"),
            (None, " as the central focus."),
          ],
          vec![
            (None, "They feature a palette dominated by "),
            (HIGHLIGHT, "deep blacks and vivid reds"),
            (None, " which convey a sense of "),
            (HIGHLIGHT, "intensity, danger, and allure"),
            (None, "."),
          ],
          vec![
            (None, "These designs create a "),
            (HIGHLIGHT, "dark yet captivating aesthetic"),
            (None, " that balances "),
            (HIGHLIGHT, "elegance and menace"),
            (None, "."),
          ],
        ]),
      ),
      "Samurai" => InfoBox::new(
        "Samurai",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "Samurai design"),
            (None, " combines a "),
            (HIGHLIGHT, "stylized anime character"),
            (None, " holding a "),
            (HIGHLIGHT, "katana"),
            (None, " with a "),
            (HIGHLIGHT, "dreamlike mountain background"),
            (None, "."),
          ],
          vec![
            (None, "The character is framed inside a "),
            (HIGHLIGHT, "soft circular outline"),
            (None, " that enhances focus and creates a sense of "),
            (HIGHLIGHT, "contrast against the blurred scenery"),
            (None, "."),
          ],
          vec![
            (None, "The overall palette features "),
            (HIGHLIGHT, "pastel gradients of purple, blue, and pink"),
            (None, " which evoke a feeling of "),
            (HIGHLIGHT, "calmness mixed with subtle intensity"),
            (None, "."),
          ],
          vec![
            (None, "This design merges "),
            (HIGHLIGHT, "urban streetwear details"),
            (None, " with a "),
            (HIGHLIGHT, "samurai-inspired aesthetic"),
            (None, " for a unique modern-meets-traditional style."),
          ],
        ]),
      ),
      "Sweet" => InfoBox::new(
        "Sweet",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "Sweet design"),
            (None, " showcases a "),
            (HIGHLIGHT, "futuristic neon environment"),
            (None, " with a glowing "),
            (HIGHLIGHT, "circular portal"),
            (None, " at the center, radiating vivid light."),
          ],
          vec![
            (None, "The surrounding scene is filled with "),
            (HIGHLIGHT, "angular crystalline structures"),
            (None, " and a reflective "),
            (HIGHLIGHT, "gridded floor"),
            (None, " that enhances the sense of depth."),
          ],
          vec![
            (None, "The palette blends "),
            (HIGHLIGHT, "purple, pink, and electric blue hues"),
            (None, " to create a striking "),
            (HIGHLIGHT, "sci-fi and synthwave atmosphere"),
            (None, "."),
          ],
          vec![
            (None, "This design conveys a feeling of "),
            (HIGHLIGHT, "mystery and technological wonder"),
            (None, " while remaining visually bold and immersive."),
          ],
        ]),
      ),
      "Temple" => InfoBox::new(
        "Temple",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "Temple design"),
            (None, " depicts a "),
            (HIGHLIGHT, "mythological theme"),
            (None, " featuring "),
            (HIGHLIGHT, "Athena"),
            (None, " as a central figure in a setting that blends "),
            (HIGHLIGHT, "ancient architecture"),
            (None, " with "),
            (HIGHLIGHT, "futuristic interfaces"),
            (None, "."),
          ],
          vec![
            (None, "The atmosphere combines "),
            (HIGHLIGHT, "Greek-inspired columns and pottery"),
            (None, " with glowing "),
            (HIGHLIGHT, "digital displays and floating code"),
            (None, ", merging tradition with modern technology."),
          ],
          vec![
            (None, "Athena is portrayed in "),
            (HIGHLIGHT, "golden armor and blue attire"),
            (None, " embodying both "),
            (HIGHLIGHT, "wisdom and strength"),
            (None, " while surrounded by an aura of "),
            (HIGHLIGHT, "strategic intelligence"),
            (None, "."),
          ],
          vec![
            (None, "This design creates a unique "),
            (HIGHLIGHT, "fusion of mythology and cyber aesthetics"),
            (None, " symbolizing a balance between the "),
            (HIGHLIGHT, "ancient and the futuristic"),
            (None, "."),
          ],
        ]),
      ),
      "None" => InfoBox::new(
        "None",
        styled_block(vec![vec![(None, "No design will be installed.")]]),
      ),
      _ => InfoBox::new(
        "Unknown Design",
        styled_block(vec![vec![(
          None,
          "No information available for this design.",
        )]]),
      ),
    }
  }
  pub fn display_widget(installer: &mut Installer) -> Option<Box<dyn ConfigWidget>> {
    installer.design.clone().map(|s| {
      let ib = InfoBox::new(
        "",
        styled_block(vec![
          vec![(None, "Current design set to:")],
          vec![(HIGHLIGHT, &s)],
        ]),
      );
      Box::new(ib) as Box<dyn ConfigWidget>
    })
  }
  pub fn page_info<'a>() -> (String, Vec<Line<'a>>) {
    (
      "Design".to_string(),
      styled_block(vec![
        vec![(
          None,
          "Select the design to be installed on your system.",
        )],
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
}

impl Default for Design {
  fn default() -> Self {
    Self::new()
  }
}

impl Page for Design {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let vert_chunks = Layout::default()
      .direction(Direction::Vertical)
      .constraints([Constraint::Percentage(50), Constraint::Percentage(50)].as_ref())
      .split(area);
    let hor_chunks = split_hor!(
      vert_chunks[0],
      1,
      [
        Constraint::Percentage(38),
        Constraint::Length(28),
        Constraint::Percentage(38),
      ]
    );

    self.designs.render(f, hor_chunks[1]);
    
    let selected = self
      .designs
      .items
      .get(self.designs.selected_idx)
      .map(|s| s.as_str())
      .unwrap_or("");
    
    let info_box = Self::get_design_info(selected);
    info_box.render(f, vert_chunks[1]);
    
    self.help_modal.render(f, area);
  }

  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate design options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select design and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Select the design for your graphical interface.",
      )],
    ]);
    ("Design".to_string(), help_content)
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
      ui_back!() => Signal::Pop,
      KeyCode::Enter => {
        installer.design =
          Some(self.designs.items[self.designs.selected_idx].clone());
        Signal::Pop
      }
      ui_up!() => {
        if !self.designs.previous_item() {
          self.designs.last_item();
        }
        Signal::Wait
      }
      ui_down!() => {
        if !self.designs.next_item() {
          self.designs.first_item();
        }
        Signal::Wait
      }
      _ => self.designs.handle_input(event),
    }
  }
}

pub struct DisplayManager {
  dms: StrList,
  help_modal: HelpModal<'static>,
}

impl DisplayManager {
  pub fn new() -> Self {
    let dms = [
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
    ]
    .iter()
    .map(|s| s.to_string())
    .collect::<Vec<_>>();
    let mut dms = StrList::new("Select Display Manager", dms);
    dms.focus();
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate display manager options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select display manager and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Select the display manager for your graphical interface.",
      )],
    ]);
    let help_modal = HelpModal::new("Display Manager", help_content);
    Self {
      dms,
      help_modal,
    }
  }
  pub fn get_displaymanager_info<'a>(name: &str) -> InfoBox<'a> {
    match name {
      "Ly" => InfoBox::new(
        "Ly",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "Ly"),
            (None, " is a "),
            (HIGHLIGHT, "lightweight, terminal-based display manager"),
            (None, " designed for "),
            (HIGHLIGHT, "speed, simplicity, and minimal resource usage"),
            (None, "."),
          ],
          vec![
            (None, "It runs "),
            (HIGHLIGHT, "directly on a TTY"),
            (None, " and provides a "),
            (HIGHLIGHT, "keyboard-driven login interface"),
            (None, " without any graphical overhead."),
          ],
          vec![
            (None, "Ly is "),
            (HIGHLIGHT, "ideal for tiling window managers"),
            (None, ", "),
            (HIGHLIGHT, "Wayland compositors"),
            (None, ", and users who prefer a "),
            (HIGHLIGHT, "clean, distraction-free workflow"),
            (None, "."),
          ],
          vec![
            (None, "Its minimal design makes it "),
            (HIGHLIGHT, "fast to start"),
            (None, ", "),
            (HIGHLIGHT, "easy to configure"),
            (None, ", and "),
            (HIGHLIGHT, "reliable on low-end or headless systems"),
            (None, "."),
          ],
        ]),
      ),
      "Astronaut" => InfoBox::new(
        "Astronaut",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "SDDM Astronaut"),
            (None, " presents a "),
            (HIGHLIGHT, "space-themed illustration"),
            (None, " featuring a "),
            (HIGHLIGHT, "vast planet"),
            (None, " set against a "),
            (HIGHLIGHT, "star-filled sky"),
            (None, "."),
          ],
          vec![
            (None, "A nearby "),
            (HIGHLIGHT, "spacecraft"),
            (None, " and a "),
            (HIGHLIGHT, "lone astronaut"),
            (None, " introduce a sense of "),
            (HIGHLIGHT, "exploration and scale"),
            (None, "."),
          ],
          vec![
            (None, "The palette leans on "),
            (HIGHLIGHT, "cool blues and muted tones"),
            (None, " with "),
            (HIGHLIGHT, "clean, graphic shapes"),
            (None, " that create a "),
            (HIGHLIGHT, "calm, futuristic atmosphere"),
            (None, "."),
          ],
        ]),
      ),
      "Black Hole" => InfoBox::new(
        "Black Hole",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "SDDM Black Hole"),
            (None, " depicts a "),
            (HIGHLIGHT, "deep-space scene"),
            (None, " dominated by a "),
            (HIGHLIGHT, "massive black hole"),
            (None, " encircled by a bright "),
            (HIGHLIGHT, "accretion ring"),
            (None, " of swirling light and dust."),
          ],
          vec![
            (None, "Floating nearby are "),
            (HIGHLIGHT, "asteroids and debris"),
            (None, " with a "),
            (HIGHLIGHT, "solitary astronaut"),
            (None, " adding a sense of "),
            (HIGHLIGHT, "scale and motion"),
            (None, "."),
          ],
          vec![
            (None, "The color palette blends "),
            (HIGHLIGHT, "deep purples and blues"),
            (None, " with "),
            (HIGHLIGHT, "fiery orange highlights"),
            (None, ", creating a "),
            (HIGHLIGHT, "cinematic cosmic atmosphere"),
            (None, "."),
          ],
          vec![
            (None, "Overall, it conveys "),
            (HIGHLIGHT, "mystery, gravity, and vastness"),
            (None, " while remaining visually "),
            (HIGHLIGHT, "striking and immersive"),
            (None, "."),
          ],
        ]),
      ),
      "Cyberpunk" => InfoBox::new(
        "Cyberpunk",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "SDDM Cyberpunk"),
            (None, " features a "),
            (HIGHLIGHT, "cyberpunk portrait"),
            (None, " of a "),
            (HIGHLIGHT, "hooded figure with a masked/visored face"),
            (None, ", rendered against a "),
            (HIGHLIGHT, "dark background"),
            (None, "."),
          ],
          vec![
            (None, "Surrounding the figure are "),
            (HIGHLIGHT, "glitch effects and HUD-like geometric lines"),
            (None, " that suggest "),
            (HIGHLIGHT, "digital interference and motion"),
            (None, "."),
          ],
          vec![
            (None, "The palette blends "),
            (HIGHLIGHT, "neon magenta and electric cyan"),
            (None, " accents with "),
            (HIGHLIGHT, "high-contrast blacks"),
            (None, " for a bold, tech-noir look."),
          ],
          vec![
            (None, "Overall, it conveys "),
            (HIGHLIGHT, "anonymity, hacking culture, and futuristic grit"),
            (None, " with a distinctly "),
            (HIGHLIGHT, "synthwave aesthetic"),
            (None, "."),
          ],
        ]),
      ),
      "Cyborg" => InfoBox::new(
        "Cyborg",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "SDDM Cyborg"),
            (None, " showcases a "),
            (HIGHLIGHT, "minimalist retro-futuristic illustration"),
            (None, " of a "),
            (HIGHLIGHT, "woman in profile"),
            (None, " with long dark hair and subtle "),
            (HIGHLIGHT, "cybernetic accents"),
            (None, " across the face."),
          ],
          vec![
            (None, "She is framed by a "),
            (HIGHLIGHT, "geometric wireframe cube"),
            (None, " in perspective, adding a sense of "),
            (HIGHLIGHT, "depth and containment"),
            (None, "."),
          ],
          vec![
            (None, "The palette uses "),
            (HIGHLIGHT, "warm monochrome tones"),
            (None, ", cream, beige, and charcoal, creating a "),
            (HIGHLIGHT, "calm yet otherworldly atmosphere"),
            (None, " with generous negative space."),
          ],
        ]),
      ),
      "Kath" => InfoBox::new(
        "Kath",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "SDDM Kath"),
            (None, " is an "),
            (HIGHLIGHT, "animated pixel-art scene"),
            (None, " set in a "),
            (HIGHLIGHT, "futuristic industrial room"),
            (None, " where a "),
            (HIGHLIGHT, "blue-haired girl"),
            (None, " sits under "),
            (HIGHLIGHT, "soft neon lighting"),
            (None, "."),
          ],
          vec![
            (None, "The palette blends "),
            (HIGHLIGHT, "cool blues and violets"),
            (None, " with "),
            (HIGHLIGHT, "electric cyan highlights"),
            (None, ", evoking a "),
            (HIGHLIGHT, "retro sci-fi arcade vibe"),
            (None, "."),
          ],
          vec![
            (None, "Overall, it conveys "),
            (HIGHLIGHT, "calm, atmospheric energy"),
            (None, " with a mix of "),
            (HIGHLIGHT, "nostalgia and futurism"),
            (None, "."),
          ],
        ]),
      ),
      "Jake The Dog" => InfoBox::new(
        "Jake The Dog",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "SDDM Jake The Dog"),
            (None, " is an "),
            (HIGHLIGHT, "animated, cartoon-style scene"),
            (None, " featuring a "),
            (HIGHLIGHT, "relaxed dog wearing headphones"),
            (None, " sitting on a rooftop and enjoying music."),
          ],
          vec![
            (None, "The background shows a "),
            (HIGHLIGHT, "dusk forest landscape"),
            (None, " with "),
            (HIGHLIGHT, "soft purples and muted greens"),
            (None, " rendered in "),
            (HIGHLIGHT, "simple shapes and thick outlines"),
            (None, "."),
          ],
          vec![
            (None, "Overall, it conveys a "),
            (HIGHLIGHT, "cozy, playful, and nostalgic vibe"),
            (None, " ideal for a laid-back login mood."),
          ],
        ]),
      ),
      "Pixel Sakura" => InfoBox::new(
        "Pixel Sakura",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "SDDM Pixel Sakura"),
            (None, " is a "),
            (HIGHLIGHT, "pixel-art cityscape"),
            (None, " with a "),
            (HIGHLIGHT, "train crossing a bridge"),
            (None, " over calm water, a distant "),
            (HIGHLIGHT, "skyline"),
            (None, ", and a foreground "),
            (HIGHLIGHT, "cherry-blossom branch"),
            (None, "."),
          ],
          vec![
            (None, "A "),
            (HIGHLIGHT, "looping animation"),
            (None, " sends "),
            (HIGHLIGHT, "petals/leaves drifting"),
            (None, " through the air, creating a sense of "),
            (HIGHLIGHT, "breeze and quiet motion"),
            (None, "."),
          ],
          vec![
            (None, "The palette favors "),
            (HIGHLIGHT, "soft grays and off-white"),
            (None, " with "),
            (HIGHLIGHT, "delicate pink accents"),
            (None, " and a "),
            (HIGHLIGHT, "muted sun"),
            (None, ", with gentle "),
            (HIGHLIGHT, "reflections in the water"),
            (None, "."),
          ],
          vec![
            (None, "Overall, it conveys "),
            (HIGHLIGHT, "serene, nostalgic vibes"),
            (None, " blending "),
            (HIGHLIGHT, "urban slice-of-life ambience"),
            (None, " with "),
            (HIGHLIGHT, "seasonal calm"),
            (None, "."),
          ],
        ]),
      ),
      "Post-Apocalypse" => InfoBox::new(
        "Post-Apocalypse",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "SDDM Post-Apocalypse"),
            (None, " presents a "),
            (HIGHLIGHT, "gritty, graphic-novel illustration"),
            (None, " of a "),
            (HIGHLIGHT, "hooded figure with a skull-like face"),
            (None, " and "),
            (HIGHLIGHT, "glowing pink goggles"),
            (None, "."),
          ],
          vec![
            (None, "Cables and tubes trail from the mask as the character "),
            (HIGHLIGHT, "hunches over a keyboard"),
            (None, " beside a "),
            (HIGHLIGHT, "well-worn laptop"),
            (None, ", adding a sense of "),
            (HIGHLIGHT, "tension and focus"),
            (None, "."),
          ],
          vec![
            (None, "The palette leans on "),
            (HIGHLIGHT, "muted olive, maroon, and charcoal"),
            (None, " accented by "),
            (HIGHLIGHT, "neon magenta highlights"),
            (None, ", with "),
            (HIGHLIGHT, "rough inks and grunge textures"),
            (None, " that emphasize a "),
            (HIGHLIGHT, "post-apocalyptic hacker vibe"),
            (None, "."),
          ],
        ]),
      ),
      "Purple Leaves" => InfoBox::new(
        "Purple Leaves",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "SDDM Purple Leaves"),
            (None, " features a "),
            (HIGHLIGHT, "stylized botanical pattern"),
            (None, " of "),
            (HIGHLIGHT, "overlapping leaves"),
            (None, " rendered with bold outlines and layered shading."),
          ],
          vec![
            (None, "The palette leans on "),
            (HIGHLIGHT, "deep indigos, violets, and navy tones"),
            (None, " creating a "),
            (HIGHLIGHT, "moody, nocturnal atmosphere"),
            (None, "."),
          ],
          vec![
            (None, "Overall, it provides a "),
            (HIGHLIGHT, "calm, elegant backdrop"),
            (None, " with "),
            (HIGHLIGHT, "soft contrast"),
            (None, " that keeps the scene cohesive and unobtrusive."),
          ],
        ]),
      ),
      "None" => InfoBox::new(
        "None",
        styled_block(vec![vec![(None, "No display manager will be installed.")]]),
      ),
      _ => InfoBox::new(
        "Unknown Display Manager",
        styled_block(vec![vec![(
          None,
          "No information available for this display manager.",
        )]]),
      ),
    }
  }
  pub fn display_widget(installer: &mut Installer) -> Option<Box<dyn ConfigWidget>> {
    installer.display_manager.clone().map(|s| {
      let ib = InfoBox::new(
        "",
        styled_block(vec![
          vec![(None, "Current display manager set to:")],
          vec![(HIGHLIGHT, &s)],
        ]),
      );
      Box::new(ib) as Box<dyn ConfigWidget>
    })
  }
  pub fn page_info<'a>() -> (String, Vec<Line<'a>>) {
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
}

impl Default for DisplayManager {
  fn default() -> Self {
    Self::new()
  }
}

impl Page for DisplayManager {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let vert_chunks = Layout::default()
      .direction(Direction::Vertical)
      .constraints([Constraint::Percentage(50), Constraint::Percentage(50)].as_ref())
      .split(area);
    let hor_chunks = split_hor!(
      vert_chunks[0],
      1,
      [
        Constraint::Percentage(38),
        Constraint::Length(28),
        Constraint::Percentage(38),
      ]
    );

    self.dms.render(f, hor_chunks[1]);

    let selected = self
      .dms
      .items
      .get(self.dms.selected_idx)
      .map(|s| s.as_str())
      .unwrap_or("");

    let info_box = Self::get_displaymanager_info(selected);
    info_box.render(f, vert_chunks[1]);

    self.help_modal.render(f, area);
  }

  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate display manager options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select display manager and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Select the display manager for your graphical interface.",
      )],
    ]);
    ("Display Manager".to_string(), help_content)
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
      ui_back!() => Signal::Pop,
      KeyCode::Enter => {
        installer.display_manager =
          Some(self.dms.items[self.dms.selected_idx].clone());
        Signal::Pop
      }
      ui_up!() => {
        if !self.dms.previous_item() {
          self.dms.last_item();
        }
        Signal::Wait
      }
      ui_down!() => {
        if !self.dms.next_item() {
          self.dms.first_item();
        }
        Signal::Wait
      }
      _ => self.dms.handle_input(event),
    }
  }
}

pub struct Timezone {
  timezones: StrList,
  search_bar: LineEditor,
  search_focused: bool,
  help_modal: HelpModal<'static>,
}

impl Timezone {
  pub fn new() -> Self {
    let timezone_list = Self::list_timezones();
    let timezones = StrList::new("Select Timezone", timezone_list);

    let mut search_bar = LineEditor::new("Search (press '/' to focus)", Some("Type to filter..."));
    search_bar.focus();

    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate timezone options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select timezone and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "/"),
        (None, " - Focus search bar")
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Select the timezone that matches your physical location.",
      )],
    ]);
    let help_modal = HelpModal::new("Timezone", help_content);
    Self {
      timezones,
      search_bar,
      search_focused: true,
      help_modal,
    }
  }

  fn list_timezones() -> Vec<String> {
      match Command::new("timedatectl").arg("list-timezones").output() {
          Ok(out) if out.status.success() => {
              String::from_utf8_lossy(&out.stdout)
                  .lines()
                  .map(str::trim)
                  .filter(|s| !s.is_empty())
                  .map(|s| s.to_string())
                  .collect()
          }
          _ => Vec::new(),
      }
  }
  pub fn display_widget(installer: &mut Installer) -> Option<Box<dyn ConfigWidget>> {
    installer.timezone.clone().map(|s| {
      let ib = InfoBox::new(
        "",
        styled_block(vec![
          vec![(None, "Current timezone set to:")],
          vec![(HIGHLIGHT, &s)],
        ]),
      );
      Box::new(ib) as Box<dyn ConfigWidget>
    })
  }
  pub fn page_info<'a>() -> (String, Vec<Line<'a>>) {
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
}

impl Default for Timezone {
  fn default() -> Self {
    Self::new()
  }
}

impl Page for Timezone {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let chunks = split_vert!(area, 0, [Constraint::Length(3), Constraint::Min(0)]);
    self.search_bar.render(f, chunks[0]);
    self.timezones.render(f, chunks[1]);
    self.help_modal.render(f, area);
  }

  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate timezone options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select timezone and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Select the timezone that matches your physical location.",
      )],
    ]);
    ("Timezone".to_string(), help_content)
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
      ui_back!() => return Signal::Pop,

      // Focus search with '/'
      KeyCode::Char('/') if !self.search_focused => {
        self.search_focused = true;
        self.search_bar.focus();
        self.search_bar.clear();
        return Signal::Wait;
      }

      _ => {}
    }

    if self.search_focused {
      // While the search bar has focus, keystrokes go there
      match event.code {
        KeyCode::Esc => {
          // Clear filter and return focus to the list
          self.search_bar.clear();
          self.timezones.set_filter(None::<String>);
          self.search_bar.unfocus();
          self.search_focused = false;
          self.timezones.focus();
          return Signal::Wait;
        }
        KeyCode::Enter | KeyCode::Tab | KeyCode::Down => {
          // Move back to the list
          self.search_bar.unfocus();
          self.search_focused = false;
          self.timezones.focus();
          return Signal::Wait;
        }
        _ => {
          // Let the editor mutate its contents
          let _ = self.search_bar.handle_input(event);

          // Pull current value and update fuzzy filter
          let text = self
            .search_bar
            .get_value()
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default();

          if text.is_empty() {
            self.timezones.set_filter(None::<String>);
          } else {
            self.timezones.set_filter(Some(text));
          }
          return Signal::Wait;
        }
      }
    }

    // List-focused behavior
    match event.code {
      KeyCode::Enter => {
        if let Some(sel) = self.timezones.selected_item() {
          installer.timezone = Some(sel.clone());
          return Signal::Pop;
        }
        Signal::Wait
      }
      ui_up!() => {
        if !self.timezones.previous_item() {
          self.timezones.last_item();
        }
        Signal::Wait
      }
      ui_down!() => {
        if !self.timezones.next_item() {
          self.timezones.first_item();
        }
        Signal::Wait
      }
      _ => self.timezones.handle_input(event),
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
  Idle,                 // first entry into the page
  Syncing,              // pacman -Sy running
  Ready,                // repo list loaded, picker usable
  Failed(String),       // sync failed
}

impl ExtraPackages {
  pub fn new() -> Self {
    // Build an empty picker; we’ll fill it on first render (lazy init)
    let picker = crate::widget::PackagePicker::new(
      "Selected Packages",
      "Available Packages",
      Vec::new(),
      Vec::new(),
    );

    let help = HelpModal::new(
      "Extra Packages",
      styled_block(vec![
        vec![(None, "Select extra packages to install.")],
        vec![(None, "Use '/' to search, Enter to add/remove, Tab to switch panes.")],
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

  pub fn page_info<'a>() -> (String, Vec<Line<'a>>) {
    (
      "Extra Packages".to_string(),
      styled_block(vec![vec![(
        None,
        "Pick additional packages to install alongside your base selection.",
      )]]),
    )
  }

  pub fn display_widget(installer: &mut Installer) -> Option<Box<dyn ConfigWidget>> {
    let is_arch = installer
      .basesystem
      .as_deref()
      .map(|s| s.to_lowercase().contains("arch"))
      .unwrap_or(false);

    let text = if is_arch {
      let n = installer.extra_packages.len();
      format!("{} selected package{}", n, if n == 1 { "" } else { "s" })
    } else {
      "Not applicable to this base.".to_string()
    };
    Some(Box::new(InfoBox::new("", styled_block(vec![vec![(None, text)]]))))
  }

  fn kick_off_sync(&mut self, installer: &mut Installer) {
    if matches!(self.state, LoadState::Idle) {
      // If we've already synced earlier in this boot, skip calling pacman -Sy
      if installer.repo_synced {
        // jump straight to “Ready” after ensuring we have a cached list
        self.state = LoadState::Syncing; // small trick to reuse post-sync path
        // fake a success message through the channel so the normal flow runs
        let (tx, rx) = mpsc::channel();
        let _ = tx.send(Ok(()));
        self.rx = Some(rx);
        return;
      }
  
      let (tx, rx) = mpsc::channel::<anyhow::Result<()>>();
      self.rx = Some(rx);
      self.state = LoadState::Syncing;
  
      thread::spawn(move || {
        let status = Command::new("pacman")
          .args(["-Sy"])
          .stdin(Stdio::null())
          .stdout(Stdio::null())
          .stderr(Stdio::null())
          .status();
  
        match status {
          Ok(st) if st.success() => { let _ = tx.send(Ok(())); }
          Ok(st) => { let _ = tx.send(Err(anyhow::anyhow!("pacman -Sy failed (exit {})", st.code().unwrap_or(-1)))); }
          Err(e) => { let _ = tx.send(Err(anyhow::anyhow!("failed to exec pacman -Sy: {e}"))); }
        }
      });
    }
  }
  
  fn load_packages_after_sync(&mut self, installer: &mut Installer) -> anyhow::Result<()> {
    // Use cache if present
    let available = if let Some(list) = installer.cached_repo_pkgs.clone() {
      list
    } else {
      // Otherwise fetch once with -Slq and cache it
      let out = Command::new("pacman").arg("-Slq").output()?;
      if !out.status.success() {
        anyhow::bail!(
          "pacman -Slq failed (exit {})",
          out.status.code().unwrap_or(-1)
        );
      }
      let mut names: Vec<String> = String::from_utf8(out.stdout)?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(ToOwned::to_owned)
        .collect();
      names.sort();
      names.dedup();
  
      installer.cached_repo_pkgs = Some(names.clone());
      names
    };
  
    installer.repo_synced = true;
  
    // Rebuild picker with fresh data
    self.picker.package_manager =
      crate::widget::PackageManager::new(available, installer.extra_packages.clone());
  
    // Update BOTH panes + respect current filter
    self.picker.selected.set_items(self.picker.get_selected_packages());
  
    if let Some(ref ftxt) = self.picker.current_filter {
      // if user was filtering, keep it and show filtered availables
      let filtered = self.picker.package_manager.get_available_filtered(ftxt);
      self.picker.available.set_items(filtered);
      self.picker.available.selected_idx = 0;
    } else {
      let avail = self.picker.package_manager.get_available_packages();
      self.picker.available.set_items(avail);
      self.picker.available.selected_idx = 0;
    }
  
    // Start focused in the search bar
    self.picker.focus();
  
    Ok(())
  }

  fn request_refresh(&mut self, installer: &mut Installer) {
    // Clear cache and re-run the tiny state machine
    installer.cached_repo_pkgs = None;
    installer.repo_synced = false;
    self.state = LoadState::Idle;
    self.rx = None;
  }
}

impl Default for ExtraPackages {
  fn default() -> Self { Self::new() }
}

impl Page for ExtraPackages {
  fn render(&mut self, installer: &mut Installer, f: &mut Frame, area: Rect) {
    let is_arch = installer
      .basesystem
      .as_deref()
      .map(|s| s.to_lowercase().contains("arch"))
      .unwrap_or(false);

    if !is_arch {
      let p = Paragraph::new("This page is available if the selected Base System is Arch.")
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::ALL).title("Extra Packages"));
      f.render_widget(p, area);
      return;
    }

    // 1) If first time here, start syncing
    if matches!(self.state, LoadState::Idle) {
      self.kick_off_sync(installer);
    }

    // 2) If syncing, poll for completion and transition
    if matches!(self.state, LoadState::Syncing)
      && let Some(rx) = &self.rx
        && let Ok(result) = rx.try_recv() {
          match result {
            Ok(()) => {
              match self.load_packages_after_sync(installer) {
                Ok(()) => {
                  self.state = LoadState::Ready;
                }
                Err(e) => {
                  self.state = LoadState::Failed(format!("Failed to load packages: {e}"));
                }
              }
            }
            Err(e) => {
              self.state = LoadState::Failed(format!("{e}"));
            }
          }
        }

    // 3) Render by state
    match &self.state {
      LoadState::Idle | LoadState::Syncing => {
        // Simple centered “syncing” message
        let msg = Paragraph::new("Syncing package list with pacman -Sy...")
          .alignment(Alignment::Center)
          .block(Block::default().borders(Borders::ALL).title("Extra Packages"));
        f.render_widget(msg, area);
      }
      LoadState::Failed(err) => {
        let lines = styled_block(vec![
          vec![(Some((Color::Red, Modifier::BOLD)), "Failed to prepare package list.")],
          vec![(None, "")],
          vec![(None, "You can run the following manually in the live system:")],
          vec![(Some((Color::Yellow, Modifier::BOLD)), "  pacman -Sy")],
          vec![(None, "")],
          vec![(Some((Color::Indexed(245), Modifier::ITALIC)), err.as_str())],
          vec![(None, "")],
          vec![(None, "Press Esc to go back.")],
        ]);
        let p = Paragraph::new(lines)
          .block(Block::default().borders(Borders::ALL).title("Extra Packages"))
          .wrap(Wrap { trim: false });
        f.render_widget(p, area);
      }
      LoadState::Ready => {
        // Keep picker selection in sync if it was pre-populated externally
        if self.picker.selected.items.is_empty() && !installer.extra_packages.is_empty() {
          self.picker.package_manager = crate::widget::PackageManager::new(
            self.picker.package_manager.get_available_packages(),
            installer.extra_packages.clone(),
          );
          self.picker.selected.set_items(self.picker.get_selected_packages());
          if let Some(ref ftxt) = self.picker.current_filter {
            self.picker.set_filter(Some(ftxt.clone()));
          } else {
            let items = self.picker.package_manager.get_current_available();
            self.picker.available.set_items(items);
          }
        }

        self.picker.render(f, area);
        self.help.render(f, area);
      }
    }
  }

  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    use ratatui::crossterm::event::KeyCode;

    if let KeyCode::Char('?') = event.code {
      self.help.toggle();
      return Signal::Wait;
    }
    if self.help.visible {
      if let KeyCode::Esc | KeyCode::Char('?') = event.code {
        self.help.toggle();
      }
      return Signal::Wait;
    }

    // --- refresh : F5
    if let KeyCode::F(5) = event.code {
      self.request_refresh(installer);
      return Signal::Wait;
    }

    // If not ready, only allow leaving the page
    if !matches!(self.state, LoadState::Ready) {
      if matches!(event.code, KeyCode::Esc) {
        return Signal::Pop;
      }
      return Signal::Wait;
    }

    // Ready: forward most inputs to picker; on Esc store selection and go back
    if event.code == KeyCode::Esc {
      installer.extra_packages = self.picker.get_selected_packages();
      return Signal::Pop;
    }

    self.picker.handle_input(event)
  }
}

pub struct ConfigPreview {
  system_config: String,
  partition_config: String,
  scroll_position: usize,
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

#[derive(Default)]
struct Configs {
  system: String,
  partition: String,
}

impl ConfigPreview {
  /// Maximum scroll distance for config preview window
  fn get_max_scroll(&self, visible_lines: usize) -> usize {
    let config_content = match self.current_view {
      ConfigView::System => &self.system_config,
      ConfigView::Partitioning => &self.partition_config,
    };
    let lines = config_content.lines().count();
    lines.saturating_sub(visible_lines)
  }

  pub fn new(installer: &mut Installer) -> anyhow::Result<Self> {
    // Generate the configuration like the main app does
    let config_json = installer.to_json()?;
    let mut configs = Configs::default();

    if let Value::Object(mut map) = config_json {
      let system_v = map.remove("config").unwrap_or(Value::Null);
      let drives_v = map.remove("drives").unwrap_or(Value::Null);

      configs.system    = to_string_pretty(&system_v)?;
      configs.partition = to_string_pretty(&drives_v)?;
    } else {
      // fallback
      configs.system = "null".to_string();
      configs.partition = "null".to_string();
    }

    let buttons: Vec<Box<dyn ConfigWidget>> = vec![
      Box::new(Button::new("Begin Installation")),
      Box::new(Button::new("Back")),
    ];
    let button_row = WidgetBox::button_menu(buttons);
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "1/2"),
        (None, " - Switch between System/Partition config"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Scroll config content"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Page Up/Down"),
        (None, " - Scroll page by page"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Tab"),
        (None, " - Switch to buttons"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Activate selected button"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc"),
        (None, " - Go back to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Review the generated Athena OS configuration before saving.",
      )],
    ]);
    let help_modal = HelpModal::new("Config Preview", help_content);

    Ok(Self {
      system_config: configs.system,
      partition_config: configs.partition,
      scroll_position: 0,
      button_row,
      current_view: ConfigView::System,
      help_modal,
      visible_lines: 10, // Default value, will be updated during rendering
    })
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
    let config_content = match self.current_view {
      ConfigView::System => highlight_json(&self.system_config).unwrap_or_default(),
      ConfigView::Partitioning => highlight_json(&self.partition_config).unwrap_or_default(),
    };
    debug!("Rendering config preview with text {config_content:?}");

    let lines: Vec<Line<'_>> = config_content.into_text().unwrap().lines;
    let visible_lines = chunks[1].height as usize - 2; // Account for borders
    self.visible_lines = visible_lines;

    let start_line = self.scroll_position;
    let end_line = std::cmp::min(start_line + visible_lines, lines.len());
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

    // Buttons
    self.button_row.render(f, chunks[2]);

    // Help modal
    self.help_modal.render(f, area);
  }

  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "1/2"),
        (None, " - Switch between System/Partition config"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Scroll config content"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Page Up/Down"),
        (None, " - Scroll page by page"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Tab"),
        (None, " - Switch to buttons"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Activate selected button"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc"),
        (None, " - Go back to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Review the generated Athena OS configuration before saving.",
      )],
    ]);
    ("Config Preview".to_string(), help_content)
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
        Signal::Wait
      }
      KeyCode::Char('2') => {
        self.button_row.unfocus();
        self.current_view = ConfigView::Partitioning;
        self.scroll_position = 0;
        Signal::Wait
      }
      ui_up!() => {
        if self.button_row.is_focused() {
          if !self.button_row.prev_child() {
            self.button_row.unfocus();
          }
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
          if self.scroll_position < max_scroll {
            self.scroll_position += 1;
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
        } else if self.current_view == ConfigView::Partitioning {
            self.current_view = ConfigView::System;
            self.scroll_position = 0;
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
        } else if self.current_view == ConfigView::System {
            self.current_view = ConfigView::Partitioning;
            self.scroll_position = 0;
        }

        Signal::Wait
      }
      KeyCode::PageUp => {
        self.scroll_position = self.scroll_position.saturating_sub(10);
        Signal::Wait
      }
      KeyCode::PageDown => {
        let max_scroll = self.get_max_scroll(self.visible_lines);
        self.scroll_position = std::cmp::min(self.scroll_position + 10, max_scroll);
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

pub struct InstallProgress<'a> {
  _installer: Installer,
  steps: InstallSteps<'a>,
  log_box: LogBox<'a>,
  //progress_bar: ProgressBar,
  progress_bar: FancyTicker,
  help_modal: HelpModal<'static>,
  signal: Option<Signal>,

  // we only hold onto these to keep them alive during installation
  _system_cfg: NamedTempFile,
  _partition_cfg: NamedTempFile,
  _log_file: PathBuf,
}

pub struct AgeVerification<'a> {
  installer: Installer,
  system_cfg: Option<NamedTempFile>,
  partition_cfg: Option<NamedTempFile>,
  log_path: PathBuf,
  started_at: Instant,
  skipped_animation: bool,
  selected_button: usize,
  button_row: WidgetBox,
  help_modal: HelpModal<'static>,
  _phantom: std::marker::PhantomData<&'a ()>,
}

impl<'a> AgeVerification<'a> {
  pub fn new(
    installer: Installer,
    system_cfg: NamedTempFile,
    partition_cfg: NamedTempFile,
    log_path: PathBuf,
  ) -> Self {
    let mut button_row = WidgetBox::button_menu(vec![
      Box::new(Button::new("I am overqualified")) as Box<dyn ConfigWidget>,
      Box::new(Button::new("I am not")) as Box<dyn ConfigWidget>,
    ]);

    let help_modal = HelpModal::new(
      "Age verification",
      styled_block(vec![
        vec![(Some((Color::Yellow, Modifier::BOLD)), "Enter"), (None, " - continue to installation")],
        vec![(Some((Color::Yellow, Modifier::BOLD)), "Esc"), (None, " - go back to the previous page")],
        vec![(Some((Color::Yellow, Modifier::BOLD)), "?"), (None, " - show this help")],
        vec![(None, "")],
        vec![(None, "This is a serious verification process, in the sense that it has a window and a button.")],
      ]),
    );

    button_row.focus();

    Self {
      installer,
      system_cfg: Some(system_cfg),
      partition_cfg: Some(partition_cfg),
      log_path,
      started_at: Instant::now(),
      skipped_animation: false,
      selected_button: 0,
      button_row,
      help_modal,
      _phantom: std::marker::PhantomData,
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
    self.skipped_animation || self.visible_joke_text().chars().count() >= Self::full_joke_text().chars().count()
  }

  fn popup_area(area: Rect) -> Rect {
    let popup_width = (area.width as f32 * 0.72) as u16;
    let popup_height = 14.min(area.height.saturating_sub(2)).max(10);
    let x = area.x + area.width.saturating_sub(popup_width) / 2;
    let y = area.y + area.height.saturating_sub(popup_height) / 2;

    Rect { x, y, width: popup_width, height: popup_height }
  }

  fn start_install(&mut self) -> Signal {
    let Some(system_cfg) = self.system_cfg.take() else {
      return Signal::Error(anyhow::anyhow!("System config temp file already consumed"));
    };

    let Some(partition_cfg) = self.partition_cfg.take() else {
      return Signal::Error(anyhow::anyhow!("Partition config temp file already consumed"));
    };

    match InstallProgress::new(
      self.installer.clone(),
      system_cfg,
      partition_cfg,
      self.log_path.clone(),
    ) {
      Ok(progress) => Signal::Push(Box::new(progress)),
      Err(err) => Signal::Error(err),
    }
  }
}

impl<'a> Page for AgeVerification<'a> {
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
              Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
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
impl<'a> InstallProgress<'a> {
  // Invoked when no args are passed to dory
  pub fn new(
      installer: Installer,
      system_cfg: NamedTempFile,
      partition_cfg: NamedTempFile,
      log_path: PathBuf,
  ) -> anyhow::Result<Self> {
      
      let log_path_str = log_path
          .to_str()
          .ok_or_else(|| anyhow::anyhow!("Invalid log file path"))?
          .to_string();

      let install_steps = Self::install_commands(
          system_cfg
              .path()
              .to_str()
              .ok_or_else(|| anyhow::anyhow!("Invalid system config path"))?
              .to_string(),
          partition_cfg
              .path()
              .to_str()
              .ok_or_else(|| anyhow::anyhow!("Invalid partition config path"))?
              .to_string(),
          log_path_str.clone(),
      )?;

      let mut steps = InstallSteps::new("Install Steps", install_steps);
      steps.log_path = Some(log_path.clone());

      //let progress_bar = ProgressBar::new("Progress", 0);
      let progress_bar = FancyTicker::new(vec![
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
      ]);

      let help_content = styled_block(vec![
          vec![(Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"), (None, " - Navigate through installation steps")],
          vec![(Some((Color::Yellow, Modifier::BOLD)), "Esc"), (None, " - Exit installation (if completed)")],
          vec![(Some((Color::Yellow, Modifier::BOLD)), "?"), (None, " - Show this help")],
          vec![(None, "")],
          vec![(None, "This page shows the progress of the Athena OS installation process.")],
          vec![(None, "Installation steps are executed sequentially and their status is shown above.")],
      ]);
      let help_modal = HelpModal::new("Installation Progress", help_content);

      let mut log_box = LogBox::new("Logs".into());
      OpenOptions::new()
          .create(true)
          .write(true)  // Each time I start a new install, it makes the dory.log file empty
          .truncate(true)      // one-time clean slate
          .open(&log_path)?;
      log_box.open_log(log_path.clone())?;

      Ok(Self {
          _installer: installer,
          steps,
          log_box,
          progress_bar,
          help_modal,
          signal: None,
          _system_cfg: system_cfg,
          _partition_cfg: partition_cfg,
          _log_file: log_path,
      })
  }

  pub fn is_complete(&self) -> bool {
    self.steps.is_complete()
  }

  pub fn has_error(&self) -> bool {
    self.steps.has_error()
  }

  /// The actual installation steps
  fn install_commands(
    system_cfg_path: String,
    disk_cfg_path: String,
    log_file_path: String,
  ) -> anyhow::Result<Vec<(Line<'static>, VecDeque<Command>)>> {
    Ok(vec![
			(Line::from("Athena OS Installation..."),
			vec![
			command!("sh", "-c", format!("echo Beginning Athena OS Installation... >> {log_file_path} 2>&1")),
			command!("sh", "-c", format!("echo Writing logs in {log_file_path} >> {log_file_path} 2>&1")),
      command!(
        "sh",
        "-c",
        format!(
          "stdbuf -oL -eL -- dory --system-file '{}' --drives-file '{}' >> '{}' 2>&1",
          system_cfg_path,
          disk_cfg_path,
          log_file_path
        )
      ),
      command!("sh", "-c", format!("echo Writing logs in {log_file_path} >> {log_file_path} 2>&1")),
			/*
      command!("sh", "-c", format!("cat {system_cfg_path} 2>&1 >> {log_file_path}")),
			command!("sh", "-c", format!("echo BOH 2>&1 >> {log_file_path}")),
			command!("sh", "-c", format!("cat {disk_cfg_path} 2>&1 >> {log_file_path}")),
      */
			].into()),
			(Line::from("Finalizing installation..."),
			vec![
			//command!("sh", "-c", format!("ls {log_file_path} 2>&1 >> {log_file_path}")),
			//command!("sh", "-c", format!("cat {log_file_path} | nc termbin.com 9999 | tr -d '\0'")),
      command!("sh", "-c", format!("sleep 5")),
			].into()),
			])
  }
}

impl<'a> Page for InstallProgress<'a> {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    // Tick the steps to update animation and process commands
    let _ = self.steps.tick();
    let _ = self.log_box.poll_log();

    let chunks = split_vert!(area, 1, [Constraint::Min(0), Constraint::Length(3)]);
    let hor_chunks = split_hor!(
      chunks[0],
      1,
      [Constraint::Percentage(30), Constraint::Percentage(70)]
    );

    // Render InstallSteps widget in the main area
    self.steps.render(f, hor_chunks[0]);
    self.log_box.render(f, hor_chunks[1]);

    // Update progress bar with completion percentage
    //let progress = (self.steps.progress() * 100.0) as u32;
    self.progress_bar.tick();
    if self.steps.has_error() {
        self.signal = Some(Signal::Push(Box::new(InstallFailed::new(self._log_file.clone()))));
    } else if self.steps.is_complete() {
        self.signal = Some(Signal::Push(Box::new(InstallComplete::new())));
    }
    //self.progress_bar.set_progress(progress);
    self.progress_bar.render(f, chunks[1]);

    // Help modal
    self.help_modal.render(f, area);
  }

  fn signal(&self) -> Option<Signal> {
    // This lets us return a signal without any input
    if let Some(ref signal) = self.signal {
      match signal {
        Signal::Wait => Some(Signal::Wait),
        Signal::Push(_) => {
            if self.steps.has_error() {
                Some(Signal::Push(Box::new(InstallFailed::new(self._log_file.clone()))))
            } else {
                Some(Signal::Push(Box::new(InstallComplete::new())))
            }
        }
        Signal::Pop => Some(Signal::Pop),
        Signal::PopCount(n) => Some(Signal::PopCount(*n)),
        Signal::Quit => Some(Signal::Quit),
        Signal::WriteCfg => Some(Signal::WriteCfg),
        Signal::Unwind => Some(Signal::Unwind),
        Signal::Error(_) => Some(Signal::Wait),
      }
    } else {
      None
    }
  }

  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Scroll through command output"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Page Up/Down"),
        (None, " - Scroll output page by page"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Home/End"),
        (None, " - Jump to beginning/end of output"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc"),
        (None, " - Exit installation (if completed)"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(None, "Watch the progress as Athena OS installs. Commands run")],
      vec![(None, "sequentially and their output is logged above.")],
    ]);
    ("Installation Progress".to_string(), help_content)
  }

  fn handle_input(&mut self, _installer: &mut Installer, event: KeyEvent) -> Signal {
    if event.code == KeyCode::Char('c') && event.modifiers.contains(KeyModifiers::CONTROL) {
      return Signal::Quit;
    }
    if self.has_error() {
      match event.code {
        KeyCode::Esc => Signal::Pop,
        KeyCode::Char('q') => Signal::Pop,
        _ => Signal::Wait,
      }
    } else {
      Signal::Wait
    }
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
        "The installation remains mounted on /mnt if you wish to perform any manual configuration on the new system.",
      )],
      vec![(
        None,
        "Such manual configuration can be performed using the 'athena-chroot' command.",
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
    state: LogGenState,
    rx: Option<Receiver<Result<String, String>>>,
}

enum LogGenState {
    Idle,
    Running,
    Done(String),
    Error(String),
}

impl InstallFailed {
    pub fn new(log_path: PathBuf) -> Self {
        Self {
            log_path,
            state: LogGenState::Idle,
            rx: None,
        }
    }

    fn start_generation(&mut self) {
        if !matches!(self.state, LogGenState::Idle | LogGenState::Error(_)) {
            return;
        }

        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.state = LogGenState::Running;

        let log_path = self.log_path.clone();
        thread::spawn(move || {
            // 1. Read the log file into memory.
            let mut contents = Vec::new();
            if let Ok(mut f) = File::open(&log_path) {
                if let Err(e) = f.read_to_end(&mut contents) {
                    let _ = tx.send(Err(format!("Failed to read log file: {e}")));
                    return;
                }
            } else {
                let _ = tx.send(Err("Could not open log file".into()));
                return;
            }

            // 2. Connect directly to termbin.com:9999
            match TcpStream::connect(("termbin.com", 9999)) {
                Ok(mut stream) => {
                    if let Err(e) = stream.write_all(&contents) {
                        let _ = tx.send(Err(format!("Write error: {e}")));
                        return;
                    }
                    // termbin closes connection after sending the link
                    let mut response = String::new();
                    if let Err(e) = stream.read_to_string(&mut response) {
                        let _ = tx.send(Err(format!("Read error: {e}")));
                        return;
                    }
                    let link = response.trim().to_string();
                    if link.is_empty() {
                        let _ = tx.send(Err("No response from termbin".into()));
                    } else {
                        let _ = tx.send(Ok(link));
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(format!("Connection error: {e}")));
                }
            }
        });
    }

    fn poll_generation(&mut self) {
        if let Some(rx) = &self.rx
            && let Ok(msg) = rx.try_recv() {
                match msg {
                    Ok(url) => self.state = LogGenState::Done(url),
                    Err(err) => self.state = LogGenState::Error(err),
                }
                self.rx = None;
            }
    }

    fn content_lines(&self) -> Vec<Line<'static>> {
        match &self.state {
            LogGenState::Idle => styled_block(vec![
                vec![(None, "Athena OS installation FAILED.")],
                vec![(None, "")],
                vec![(Some((Color::Yellow, Modifier::BOLD)), "Press 'G'"), (None, " to generate a shareable log link.")],
                vec![(Some((Color::Yellow, Modifier::BOLD)), "Esc/Q"), (None, " - Go back")],
                vec![(Some((Color::Yellow, Modifier::BOLD)), "Enter"), (None, " - Exit installer")],
            ]),
            LogGenState::Running => styled_block(vec![
                vec![(None, "Generating log link... contacting termbin.com")],
                vec![(None, "")],
                vec![(Some((Color::Yellow, Modifier::BOLD)), "Esc/Q"), (None, " - Go back")],
                vec![(Some((Color::Yellow, Modifier::BOLD)), "Enter"), (None, " - Exit installer")],
            ]),
            LogGenState::Done(_) => styled_block(vec![
                vec![(None, "Log link generated successfully:")],
                vec![(None, "")],
                vec![(Some((Color::Yellow, Modifier::BOLD)), "Esc/Q"), (None, " - Go back")],
                vec![(Some((Color::Yellow, Modifier::BOLD)), "Enter"), (None, " - Exit installer")],
            ]),
            LogGenState::Error(_) => styled_block(vec![
                vec![(None, "Failed to generate log link.")],
                vec![(None, "")],
                vec![(Some((Color::Yellow, Modifier::BOLD)), "G"), (None, " - Retry")],
                vec![(Some((Color::Yellow, Modifier::BOLD)), "Esc/Q"), (None, " - Go back")],
                vec![(Some((Color::Yellow, Modifier::BOLD)), "Enter"), (None, " - Exit installer")],
            ]),
        }
    }
}

impl Default for InstallFailed {
    fn default() -> Self {
        Self::new(PathBuf::from("/tmp/dory.log"))
    }
}

impl Page for InstallFailed {
    fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
        self.poll_generation();
        let chunks = split_vert!(area, 1, [Constraint::Percentage(100)]);
    
        let mut lines = self.content_lines();
    
        match &self.state {
            LogGenState::Done(url) => {
                let leaked: &'static str = Box::leak(url.clone().into_boxed_str());
                let url_line = Line::from(vec![
                    Span::styled(leaked, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                ]);
                
                // Insert *below* the header "Log link generated successfully:"
                // Your Done() content_lines() currently builds:
                // 0: "Log link generated successfully:"
                // 1: ""  (spacer)
                // 2..: keybinds
                // Replace the spacer
                if lines.len() >= 2 {
                    lines[1] = url_line;
                } else {
                    // fallback: just insert after header
                    lines.insert(1, url_line);
                }
            }
            LogGenState::Error(err) => {
                let leaked: &'static str = Box::leak(err.clone().into_boxed_str());
                let err_line = Line::from(vec![
                    Span::styled(leaked, Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                ]);
                
                // Put the error directly under "Failed to generate log link."
                // Your Error() content_lines() currently builds:
                // 0: "Failed to generate log link."
                // 1: ""  (spacer)
                if lines.len() >= 2 {
                    lines[1] = err_line;
                } else {
                    lines.insert(1, err_line);
                }
            }
            _ => {}
        }
      
        let box_widget = InfoBox::new("Installation Failed", lines);
        box_widget.render(f, chunks[0]);
    }

    fn handle_input(&mut self, _installer: &mut Installer, event: KeyEvent) -> Signal {
        match event.code {
            KeyCode::Esc | KeyCode::Char('q') => Signal::PopCount(2),
            KeyCode::Enter => Signal::Quit,
            KeyCode::Char('g') | KeyCode::Char('G') => {
                self.start_generation();
                Signal::Wait
            }
            _ => Signal::Wait,
        }
    }
}