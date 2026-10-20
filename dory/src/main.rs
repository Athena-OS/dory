use std::{env, io, path::Path};

use clap::Parser;
use core::{
  functions::locale::set_keyboard_live,
  internal::config::{install_config, read_config},
};
use log::{debug, info};
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui::{
  Terminal,
  crossterm::{
    execute,
    terminal::{Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
  },
  layout::Alignment,
  prelude::CrosstermBackend,
  style::{Color, Modifier, Style},
  text::Line,
  widgets::Paragraph,
};
use shared::{
  args::{Cli, ConfigInput},
  exec::check_if_root,
  logging,
};
use std::time::{Duration, Instant};

use crate::installer::{AgeVerification, Installer, ListKind, ListPage, Menu, Page, Signal};

mod drives;
mod installer;
#[macro_use]
mod macros;
mod widget;

const LOG_PATH: &str = "/tmp/dory.log";
/// Redraw interval for pages that animate (progress spinner, ticker).
const TICK_RATE: Duration = Duration::from_millis(100);
/// Poll interval for idle pages: only needed to notice a terminal resize.
const IDLE_POLL: Duration = Duration::from_secs(1);

type LineStyle = Option<(Color, Modifier)>;
pub fn styled_block<'a>(lines: Vec<Vec<(LineStyle, impl ToString)>>) -> Vec<Line<'a>> {
  lines
    .into_iter()
    .map(|line| {
      let spans = line
        .into_iter()
        .map(|(style_opt, text)| {
          let mut span = ratatui::text::Span::raw(text.to_string());
          if let Some((color, modifier)) = style_opt {
            span.style = Style::default().fg(color).add_modifier(modifier);
          }
          span
        })
        .collect::<Vec<_>>();
      Line::from(spans)
    })
    .collect()
}

/// RAII guard to ensure terminal state is properly cleaned up
/// when the TUI exits, either normally or via panic
struct RawModeGuard;

impl RawModeGuard {
  fn new(stdout: &mut io::Stdout) -> anyhow::Result<Self> {
    enable_raw_mode()?;
    // In the "linux" console entering the alternate screen doesn't clear,
    // so clear explicitly to avoid rendering artifacts.
    if let Ok("linux") = env::var("TERM").as_deref() {
      execute!(stdout, Clear(ClearType::All))?;
    }
    execute!(stdout, EnterAlternateScreen)?;
    Ok(Self)
  }
}

impl Drop for RawModeGuard {
  fn drop(&mut self) {
    // Ignore errors during cleanup - we're likely panicking or shutting down
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
  }
}

type Term = Terminal<CrosstermBackend<io::Stdout>>;

fn main() -> anyhow::Result<()> {
  // Restore the terminal if the installer panics, so the user is not left
  // with a raw-mode console.
  std::panic::set_hook(Box::new(|panic_info| {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
    eprintln!("====================================================");
    eprintln!("ATHENA OS INSTALLER PANIC - Terminal state restored!");
    eprintln!("====================================================");
    eprintln!("Panic occurred: {panic_info}");
    eprintln!("====================================================");
  }));

  let cli = Cli::parse();

  // --- Live-environment keymap mode -----------------------------------------
  // `--set-keymap`       -> launch the TUI picker, apply on exit
  // `--set-keymap=<id>`  -> apply directly without UI
  if let Some(value) = cli.set_keymap.as_deref() {
    check_if_root();
    if value.is_empty() {
      // Collect the choice inside the TUI scope, drop the raw-mode guard so
      // the terminal is fully restored, THEN apply + print on the real screen.
      let mut stdout = io::stdout();
      let selected: Option<String> = {
        let _raw_guard = RawModeGuard::new(&mut stdout)?;
        let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        run_keymap_picker(&mut terminal)?
      };
      if let Some(id) = selected {
        let km = set_keyboard_live(&id)?;
        // Mirror the `motd` alias: clear the screen, then render /etc/issue
        // with its escape sequences interpreted.
        print!("\x1b[2J\x1b[H");
        if let Ok(contents) = std::fs::read_to_string("/etc/issue") {
          print!("{}", contents.replace(r"\e", "\x1b"));
        }
        println!();
        println!("Keyboard layout set to: {} ({})", km.label, km.console);
      } else {
        println!("No keyboard layout selected.");
      }
      return Ok(());
    }
    let km = set_keyboard_live(value)?;
    println!("Keyboard layout set to: {} ({})", km.label, km.console);
    return Ok(());
  }

  let sources: Vec<ConfigInput> = cli
    .system_file
    .iter()
    .chain(cli.drives_file.iter())
    .cloned()
    .map(ConfigInput::File)
    .collect();
  let cli_mode = !sources.is_empty();

  // check_if_root must come before logging::init so the error is printed plainly.
  check_if_root();
  // Echo the log to stderr only in CLI mode: in the TUI it would draw over the frame.
  logging::init(cli.verbose, LOG_PATH, cli_mode);
  debug!("Logger initialized. Verbose: {}", cli.verbose);

  if cli_mode {
    if cli.dry {
      let _cfg = read_config(&sources);
      info!("Config validated (dry run).");
    } else {
      let exit_code = install_config(&sources, LOG_PATH);
      if exit_code != 0 {
        anyhow::bail!("Installation failed with exit code {exit_code}");
      }
      info!("Installation finished! You may reboot now!");
    }
    return Ok(());
  }

  // TUI path: no config files provided, launch the interactive installer
  let mut stdout = io::stdout();
  let res = {
    let _raw_guard = RawModeGuard::new(&mut stdout)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    debug!("Running TUI");
    run_app(&mut terminal, Path::new(LOG_PATH))
  };
  debug!("Exiting TUI");
  res
}

/// Draw the one-line header shared by both TUI entry points.
fn render_header(f: &mut ratatui::Frame, title: &str) -> ratatui::layout::Rect {
  let chunks = split_vert!(f.area(), 0, [Constraint::Length(1), Constraint::Min(0)]);
  let header = split_hor!(
    chunks[0],
    0,
    [Constraint::Percentage(33), Constraint::Percentage(34), Constraint::Percentage(33)]
  );
  let help_text = Paragraph::new("Press '?' for help")
    .style(Style::default().fg(Color::Gray))
    .alignment(Alignment::Center);
  f.render_widget(help_text, header[0]);
  let title = Paragraph::new(title)
    .style(Style::default().add_modifier(Modifier::BOLD))
    .alignment(Alignment::Center);
  f.render_widget(title, header[1]);
  chunks[1]
}

/// Processes signals from UI pages to control navigation and installer actions
/// Returns Ok(true) if the application should quit, Ok(false) to continue
fn handle_signal(
  signal: Signal,
  page_stack: &mut Vec<Box<dyn Page>>,
  installer: &mut Installer,
  log_path: &Path,
) -> anyhow::Result<bool> {
  match signal {
    Signal::Wait => Ok(false),
    Signal::Push(new_page) => {
      page_stack.push(new_page);
      Ok(false)
    }
    Signal::Pop => {
      page_stack.pop();
      Ok(false)
    }
    Signal::PopCount(n) => {
      for _ in 0..n {
        if page_stack.len() > 1 {
          page_stack.pop();
        }
      }
      Ok(false)
    }
    Signal::Unwind => {
      page_stack.truncate(1);
      Ok(false)
    }
    Signal::Quit => {
      debug!("Quit signal received");
      Ok(true)
    }
    Signal::WriteCfg => {
      debug!("WriteCfg signal received - starting installation process");
      let (system_cfg, disko_cfg) = installer.write_config_files()?;
      page_stack.push(Box::new(AgeVerification::new(system_cfg, disko_cfg, log_path.to_path_buf())));
      Ok(false)
    }
    Signal::Error(err) => Err(err),
  }
}

/// Wait for input. Animated pages are redrawn every `TICK_RATE`; idle pages
/// only when a key arrives (or once a second to pick up resizes).
fn next_key(animated: bool, last_tick: &mut Instant) -> anyhow::Result<Option<event::KeyEvent>> {
  let timeout = if animated {
    TICK_RATE.checked_sub(last_tick.elapsed()).unwrap_or(Duration::ZERO)
  } else {
    IDLE_POLL
  };
  let key = if event::poll(timeout)? {
    match event::read()? {
      // Key-release/repeat events (kitty protocol, Windows) must not double-trigger.
      Event::Key(key) if key.kind == KeyEventKind::Press => Some(key),
      _ => None,
    }
  } else {
    None
  };
  if last_tick.elapsed() >= TICK_RATE {
    *last_tick = Instant::now();
  }
  Ok(key)
}

/// Main TUI event loop: a page stack; each page renders, handles input and
/// can emit signals that drive navigation.
pub fn run_app(terminal: &mut Term, log_path: &Path) -> anyhow::Result<()> {
  let mut installer = Installer::new();
  let mut page_stack: Vec<Box<dyn Page>> = vec![Box::new(Menu::new())];
  let mut last_tick = Instant::now();

  loop {
    terminal.draw(|f| {
      let body = render_header(f, "Install Athena OS");
      if let Some(page) = page_stack.last_mut() {
        page.render(&mut installer, f, body);
      }
    })?;

    // Pages can emit a signal without input (e.g. installation finished).
    if let Some(page) = page_stack.last()
      && let Some(signal) = page.signal()
      && handle_signal(signal, &mut page_stack, &mut installer, log_path)?
    {
      break;
    }

    let animated = page_stack.last().is_some_and(|p| p.needs_tick());
    if let Some(key) = next_key(animated, &mut last_tick)? {
      let Some(page) = page_stack.last_mut() else {
        page_stack.push(Box::new(Menu::new()));
        continue;
      };
      let signal = page.handle_input(&mut installer, key);
      if handle_signal(signal, &mut page_stack, &mut installer, log_path)? {
        break;
      }
    }
  }
  Ok(())
}

/// Standalone TUI loop for the `--set-keymap` flag: `KeyboardLayout` is the
/// only page. Returns the chosen keymap id once the user confirms.
pub fn run_keymap_picker(terminal: &mut Term) -> anyhow::Result<Option<String>> {
  let mut installer = Installer::new();
  let mut page: Box<dyn Page> = Box::new(ListPage::new(ListKind::KeyboardLayout, &installer));
  let mut last_tick = Instant::now();
  let mut user_confirmed = false;

  loop {
    terminal.draw(|f| {
      let body = render_header(f, "Athena OS - Set Keyboard Layout");
      page.render(&mut installer, f, body);
    })?;

    if let Some(key) = next_key(page.needs_tick(), &mut last_tick)? {
      match page.handle_input(&mut installer, key) {
        Signal::Pop => {
          user_confirmed = true;
          break;
        }
        Signal::Quit | Signal::Unwind => break,
        Signal::Error(err) => return Err(err),
        _ => {}
      }
    }
  }

  Ok(if user_confirmed { installer.keyboard_layout } else { None })
}
