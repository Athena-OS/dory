//! Screen-reader support for the console installer.
//!
//! Two independent layers:
//!
//! * **Focus cursor** (always on). ratatui hides the terminal cursor and
//!   leaves it wherever its last diff write ended, so readers that follow
//!   the cursor (speakup, brltty) land on unrelated text. Focusable widgets
//!   report their position while rendering; after each draw the still-hidden
//!   cursor is parked there. Nothing changes visually.
//!
//! * **Speech** (only when speakup has a synthesizer on this console).
//!   speakup reads every byte a full-screen redraw writes, which turns a TUI
//!   into noise: diff fragments, spinners, log scrolling. While the installer
//!   runs it switches speakup's screen reading off for its own console and
//!   speaks itself through `synth_direct`: the focused item, what changed,
//!   help popups and installation milestones. speakup is restored on exit,
//!   on panic and on fatal signals.
//!
//! speakup interface (drivers/accessibility/speakup):
//! * `silent` acts on the foreground console. Bit 1 sets/clears, bit 2
//!   flushes queued speech, bit 4 is the "kill" bit. Key presses clear the
//!   plain shut-up bit but never the kill bit, and the kill bit only stops
//!   screen reading: text queued through `synth_direct` is still spoken.
//! * `synth_direct` queues raw bytes, one character per byte (so text must
//!   be Latin-1) after unescaping backslash sequences (so `\` is dropped).
//!
//! Cost when speech is off: one cursor move per frame. When it is on,
//! strings are only built for the focused widget and the visible
//! descriptions, and nothing is spoken unless something changed.

use std::{
  cell::RefCell,
  fs::{File, OpenOptions},
  os::unix::fs::FileExt,
  sync::{
    OnceLock,
    atomic::{AtomicBool, Ordering},
  },
  time::{Duration, Instant},
};

use log::info;
use ratatui::{
  Terminal,
  backend::Backend,
  buffer::Buffer,
  crossterm::event::{KeyCode, KeyEvent},
  layout::Position,
  text::Line,
};

const SPEAKUP: &str = "/sys/accessibility/speakup";
const ACTIVE_VT: &str = "/sys/class/tty/tty0/active";
/// `silent`: set kill + shut-up bits and flush queued speech.
const MUTE_AND_FLUSH: &[u8] = b"7";
/// `silent`: clear kill + shut-up bits and flush queued speech.
const UNMUTE_AND_FLUSH: &[u8] = b"6";
/// Normal utterances: a sentence or a few, never a wall of text.
const MAX_UTTERANCE: usize = 1500;
/// F3 (read the screen). speakup's buffer holds 8192 characters and stalls
/// console output when nearly full, so keep a margin.
const MAX_SCREEN: usize = 6000;
/// `synth_direct` stores at most a page per write.
const MAX_WRITE: usize = 3500;
/// Errors/warnings queued per frame; the rest are counted.
const MAX_URGENT: usize = 3;
/// Installation milestones are spoken at most this often (errors always).
const INFO_INTERVAL: Duration = Duration::from_secs(3);
/// After this long without speech during long work (package downloads write
/// no milestones for minutes), say what is going on.
const BUSY_INTERVAL: Duration = Duration::from_secs(45);
const GREETING: &str = "Athena OS installer. Speech is on. \
  F2 repeats, F3 reads the whole screen, question mark opens help.";

/// Keys handled by the speech layer itself (only while speech is on).
pub const KEY_REPEAT: KeyCode = KeyCode::F(2);
pub const KEY_READ_SCREEN: KeyCode = KeyCode::F(3);

// ---------------------------------------------------------------------------
// speakup device
// ---------------------------------------------------------------------------

struct Speaker {
  direct: File,
  silent: File,
  active_vt: File,
  /// Our console, e.g. `tty1`.
  vt: String,
}

impl Speaker {
  /// The reason is logged: it is the first thing to check when speech
  /// does not start.
  fn open() -> Result<Self, String> {
    let vt = console_name().ok_or("not running on a virtual console")?;
    let synth = std::fs::read_to_string(format!("{SPEAKUP}/synth"))
      .map_err(|e| format!("speakup not loaded ({e})"))?;
    if matches!(synth.trim(), "" | "none") {
      return Err("speakup has no synthesizer".into());
    }
    let wo = |name: &str| {
      OpenOptions::new()
        .write(true)
        .open(format!("{SPEAKUP}/{name}"))
        .map_err(|e| format!("cannot open speakup {name} ({e})"))
    };
    let speaker = Self {
      direct: wo("synth_direct")?,
      silent: wo("silent")?,
      active_vt: File::open(ACTIVE_VT).map_err(|e| format!("cannot read {ACTIVE_VT} ({e})"))?,
      vt,
    };
    if !speaker.foreground() {
      return Err(format!("{} is not the foreground console", speaker.vt));
    }
    Ok(speaker)
  }

  /// `silent` acts on the foreground console: only touch it (and only
  /// speak) while that console is ours.
  fn foreground(&self) -> bool {
    let mut buf = [0u8; 16];
    self
      .active_vt
      .read_at(&mut buf, 0)
      .is_ok_and(|n| buf[..n].trim_ascii() == self.vt.as_bytes())
  }

  fn silent(&self, value: &[u8]) {
    let _ = self.silent.write_at(value, 0);
  }

  fn say(&self, text: &[u8], interrupt: bool) {
    if text.is_empty() || !self.foreground() {
      return;
    }
    if interrupt {
      self.silent(MUTE_AND_FLUSH);
    }
    for chunk in chunks(text, MAX_WRITE) {
      let _ = self.direct.write_at(chunk, 0);
    }
  }
}

/// Split at spaces into pieces of at most `max` bytes.
fn chunks(mut text: &[u8], max: usize) -> impl Iterator<Item = &[u8]> {
  std::iter::from_fn(move || {
    if text.is_empty() {
      return None;
    }
    let mut end = text.len().min(max);
    if end < text.len()
      && let Some(space) = text[..end].iter().rposition(|&b| b == b' ')
      && space > 0
    {
      end = space + 1;
    }
    let (head, tail) = text.split_at(end);
    text = tail;
    Some(head)
  })
}

/// The virtual console we are shown on, as `ttyN`.
///
/// Not simply stdin: `sudo` (use_pty, the default since sudo 1.9.14) runs
/// the command on a fresh pseudo-terminal and relays it to the console. So
/// walk up the process tree to the first process whose controlling
/// terminal is a virtual console (`sudo dory` from tty1 finds tty1 two
/// levels up). Over ssh or in a terminal emulator there is none.
fn console_name() -> Option<String> {
  let mut pid = std::process::id();
  for _ in 0..32 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (ppid, tty_nr) = parse_stat(&stat)?;
    if let Some(n) = vt_number(tty_nr) {
      return Some(format!("tty{n}"));
    }
    if ppid <= 1 {
      return None;
    }
    pid = ppid;
  }
  None
}

/// `(ppid, tty_nr)` from `/proc/<pid>/stat`. The command name may contain
/// spaces and parentheses, so fields are counted after the last ')'.
fn parse_stat(stat: &str) -> Option<(u32, u32)> {
  let rest = &stat[stat.rfind(')')? + 1..];
  // state ppid pgrp session tty_nr
  let mut fields = rest.split_ascii_whitespace();
  let ppid = fields.nth(1)?.parse().ok()?;
  let tty_nr = fields.nth(2)?.parse::<i64>().ok()?;
  Some((ppid, u32::try_from(tty_nr).ok()?))
}

/// Virtual consoles are char device major 4, minors 1..=63.
fn vt_number(tty_nr: u32) -> Option<u32> {
  let major = (tty_nr >> 8) & 0xfff;
  let minor = (tty_nr & 0xff) | ((tty_nr >> 12) & 0xfff00);
  (major == 4 && (1..=63).contains(&minor)).then_some(minor)
}

static SPEAKER: OnceLock<Speaker> = OnceLock::new();
static SPEAKING: AtomicBool = AtomicBool::new(false);

/// Whether the installer currently drives speech. Widgets check this before
/// building any text.
#[inline]
pub fn speaking() -> bool {
  SPEAKING.load(Ordering::Relaxed)
}

/// Restores speakup when dropped. Keep it alive for the whole TUI session.
#[must_use]
pub struct Session(());

impl Drop for Session {
  fn drop(&mut self) {
    shutdown();
  }
}

/// Take over speech if speakup is active on our console. Returns `None`
/// (and leaves speakup untouched) otherwise.
pub fn init(disabled: bool) -> Option<Session> {
  if disabled {
    return None;
  }
  let speaker = match Speaker::open() {
    Ok(speaker) => speaker,
    Err(why) => {
      info!("Speech off: {why}");
      return None;
    }
  };
  let speaker = SPEAKER.get_or_init(|| speaker);
  // Handlers first: from the moment speakup is muted, every way out unmutes.
  restore_on_signal();
  SPEAKING.store(true, Ordering::SeqCst);
  speaker.silent(MUTE_AND_FLUSH);
  info!("Speech on ({})", speaker.vt);
  Some(Session(()))
}

/// Give screen reading back to speakup. Idempotent; safe from the panic hook
/// and the signal thread.
pub fn shutdown() {
  if SPEAKING.swap(false, Ordering::SeqCst)
    && let Some(speaker) = SPEAKER.get()
  {
    if speaker.foreground() {
      speaker.silent(UNMUTE_AND_FLUSH);
    } else {
      // `silent` would hit the console the user is on, and ours would stay
      // muted. Leave a watcher that unmutes ours when the user returns.
      unmute_when_back(&speaker.vt);
    }
  }
}

/// Detached shell loop: survives our exit (own process group, so no SIGHUP
/// from the console) and gives up after an hour.
fn unmute_when_back(vt: &str) {
  use std::os::unix::process::CommandExt;
  let script = format!(
    "i=0; while [ $i -lt 3600 ]; do \
       if [ \"$(cat {ACTIVE_VT})\" = {vt} ]; then echo 6 > {SPEAKUP}/silent; exit 0; fi; \
       i=$((i+1)); sleep 1; done"
  );
  let _ = std::process::Command::new("/bin/sh")
    .args(["-c", &script])
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .process_group(0)
    .spawn();
}

/// Fatal signals would otherwise leave speakup muted on this console, i.e. a
/// silent machine for a blind user. Restore it, then die as the signal would.
fn restore_on_signal() {
  use signal_hook::{
    consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM},
    iterator::Signals,
  };
  let Ok(mut signals) = Signals::new([SIGTERM, SIGHUP, SIGINT, SIGQUIT]) else {
    return;
  };
  let _ = std::thread::Builder::new()
    .name("a11y-signals".into())
    .spawn(move || {
      if let Some(sig) = signals.forever().next() {
        shutdown();
        let _ = signal_hook::low_level::emulate_default_handler(sig);
        std::process::exit(128 + sig);
      }
    });
}

// ---------------------------------------------------------------------------
// Per-frame report
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq)]
struct Focus {
  /// Container the item lives in ("Main Menu", "Set Hostname"). Spoken only
  /// when it changes.
  region: String,
  /// Full description of the item.
  text: String,
  edit: Option<Edit>,
}

#[derive(Clone, Debug, PartialEq)]
struct Edit {
  value: Vec<char>,
  cursor: usize,
  secret: bool,
}

#[derive(Default)]
struct Frame {
  cursor: Option<Position>,
  /// Where to park the cursor when nothing on the page has focus.
  default_cursor: Option<Position>,
  focus: Option<Focus>,
  /// Visible descriptive text, '\n'-separated, in render order.
  context: String,
  modal: Option<String>,
  /// Long-running work: what it is doing right now.
  busy: Option<String>,
}

#[derive(Default)]
struct Seen {
  focus: Option<Focus>,
  context: String,
  modal: Option<String>,
}

struct State {
  frame: Frame,
  seen: Seen,
  greeted: bool,
  repeat: bool,
  read_screen: bool,
  screen: Option<String>,
  alert: Option<String>,
  /// Latest installation milestone not spoken yet.
  info: Option<String>,
  info_at: Option<Instant>,
  /// Errors and warnings, always spoken, in order.
  urgent: Vec<String>,
  urgent_dropped: usize,
  last_spoken: Option<Instant>,
  /// Our console was in the background on the last frame.
  away: bool,
}

impl State {
  fn new() -> Self {
    Self {
      frame: Frame::default(),
      seen: Seen::default(),
      greeted: false,
      repeat: false,
      read_screen: false,
      screen: None,
      alert: None,
      info: None,
      info_at: None,
      urgent: Vec::new(),
      urgent_dropped: 0,
      last_spoken: None,
      away: false,
    }
  }
}

thread_local! {
  static STATE: RefCell<State> = RefCell::new(State::new());
}

/// Start collecting a new frame. Call at the top of the draw closure.
pub fn begin_frame() {
  STATE.with_borrow_mut(|st| {
    let f = &mut st.frame;
    f.cursor = None;
    f.default_cursor = None;
    f.focus = None;
    f.context.clear();
    f.modal = None;
    f.busy = None;
  });
}

/// Where the focused element is. Later calls win, so popups rendered on top
/// take the cursor. Always active (cursor-following readers, brltty).
pub fn cursor(x: u16, y: u16) {
  STATE.with_borrow_mut(|st| st.frame.cursor = Some(Position { x, y }));
}

/// Fallback cursor position (start of the page content), used when no
/// widget reports focus: braille displays then show the page, not a stray
/// line.
pub fn default_cursor(x: u16, y: u16) {
  STATE.with_borrow_mut(|st| st.frame.default_cursor = Some(Position { x, y }));
}

/// The focused item. `text` is only evaluated while speech is on.
pub fn focus(region: &str, text: impl FnOnce() -> String) {
  if !speaking() {
    return;
  }
  let text = text();
  STATE.with_borrow_mut(|st| {
    st.frame.focus = Some(Focus {
      region: region.to_owned(),
      text,
      edit: None,
    })
  });
}

/// A focused text field. Typing is echoed character by character; secret
/// fields never reveal their content.
pub fn edit(region: &str, value: &[char], cursor: usize, secret: bool, placeholder: Option<&str>) {
  if !speaking() {
    return;
  }
  let text = match (value.is_empty(), secret) {
    (true, _) => match placeholder {
      Some(p) if !p.is_empty() => format!("blank, {p}"),
      _ => "blank".to_owned(),
    },
    (false, true) if value.len() == 1 => "1 hidden character".to_owned(),
    (false, true) => format!("{} hidden characters", value.len()),
    (false, false) => value.iter().collect(),
  };
  let edit = Edit {
    value: value.to_vec(),
    cursor,
    secret,
  };
  STATE.with_borrow_mut(|st| {
    st.frame.focus = Some(Focus {
      region: region.to_owned(),
      text,
      edit: Some(edit),
    })
  });
}

/// Append descriptive text that is visible on screen (info boxes, status).
pub fn context(write: impl FnOnce(&mut String)) {
  if !speaking() {
    return;
  }
  // Built outside the borrow: `write` may itself call into this module.
  let mut part = String::new();
  write(&mut part);
  if part.is_empty() {
    return;
  }
  STATE.with_borrow_mut(|st| {
    let ctx = &mut st.frame.context;
    if !ctx.is_empty() {
      ctx.push('\n');
    }
    ctx.push_str(&part);
  });
}

/// `context` for a titled block of styled lines.
pub fn context_lines(title: &str, lines: &[Line]) {
  context(|ctx| push_lines(ctx, title, lines));
}

/// A popup on top of the page. While one is open only it is spoken.
pub fn modal(title: &str, lines: &[Line]) {
  if !speaking() {
    return;
  }
  let mut text = String::new();
  push_lines(&mut text, title, lines);
  STATE.with_borrow_mut(|st| st.frame.modal = Some(text));
}

/// Something happened that the screen does not spell out (a refused action).
pub fn alert(text: impl FnOnce() -> String) {
  if !speaking() {
    return;
  }
  let text = text();
  STATE.with_borrow_mut(|st| st.alert = Some(text));
}

/// Long-running work is visible; `status` (its latest output line) is only
/// evaluated while speech is on.
pub fn busy(status: impl FnOnce() -> String) {
  if !speaking() {
    return;
  }
  let status = strip_bars(&status());
  STATE.with_borrow_mut(|st| st.frame.busy = Some(status));
}

/// Drop `[#####-----]`-style bars: espeak would read every `#` and `-`.
fn strip_bars(line: &str) -> String {
  let mut out = String::with_capacity(line.len());
  let mut rest = line;
  while let Some(open) = rest.find('[') {
    let Some(len) = rest[open..].find(']') else {
      break;
    };
    let inner = &rest[open + 1..open + len];
    out.push_str(&rest[..open]);
    if !inner.chars().all(|c| "#-=>.~*|ocC ".contains(c)) {
      out.push_str(&rest[open..=open + len]);
    }
    rest = &rest[open + len + 1..];
  }
  out.push_str(rest);
  // Bare bars too ("checking keys ##########  100%").
  let bar = |w: &str| w.len() >= 3 && w.chars().all(|c| "#-=".contains(c));
  out
    .split_whitespace()
    .filter(|w| !bar(w))
    .collect::<Vec<_>>()
    .join(" ")
}

/// A structured backend log record (`[ LEVEL ] time message`).
pub fn progress(level: &str, msg: &str) {
  if !speaking() || msg.trim().is_empty() {
    return;
  }
  let urgent = match level {
    "ERROR" => "Error",
    "WARN" => "Warning",
    "INFO" => {
      STATE.with_borrow_mut(|st| st.info = Some(msg.to_owned()));
      return;
    }
    _ => return,
  };
  STATE.with_borrow_mut(|st| {
    if st.urgent.len() < MAX_URGENT {
      st.urgent.push(format!("{urgent}: {msg}"));
    } else {
      st.urgent_dropped += 1;
    }
  });
}

fn push_lines(out: &mut String, title: &str, lines: &[Line]) {
  let title = title.trim();
  if !title.is_empty() {
    out.push_str(title);
  }
  for line in lines {
    let start = out.len();
    if start > 0 {
      out.push('\n');
    }
    for span in &line.spans {
      out.push_str(&span.content);
    }
    // Drop empty separator lines so they do not become pauses.
    if out[start..].trim().is_empty() {
      out.truncate(start);
    }
  }
}

/// F2 / F3. Returns `true` when the key was consumed.
pub fn handle_key(key: &KeyEvent) -> bool {
  if !speaking() {
    return false;
  }
  match key.code {
    KEY_REPEAT => STATE.with_borrow_mut(|st| st.repeat = true),
    KEY_READ_SCREEN => STATE.with_borrow_mut(|st| st.read_screen = true),
    _ => return false,
  }
  true
}

/// Snapshot the rendered screen when F3 asked for it. Call at the end of the
/// draw closure; costs nothing otherwise.
pub fn capture(buf: &Buffer) {
  if !speaking() {
    return;
  }
  STATE.with_borrow_mut(|st| {
    if !std::mem::take(&mut st.read_screen) {
      return;
    }
    let area = buf.area;
    let mut text = String::with_capacity(usize::from(area.width) * usize::from(area.height));
    for y in area.top()..area.bottom() {
      let start = text.len();
      for x in area.left()..area.right() {
        if let Some(cell) = buf.cell((x, y)) {
          text.push_str(cell.symbol());
        }
      }
      let row = text[start..].trim_end().len();
      text.truncate(start + row);
      text.push('\n');
    }
    st.screen = Some(text);
  });
}

/// Park the cursor on the focused element and speak what changed. Call right
/// after `terminal.draw`.
pub fn end_frame<B: Backend>(terminal: &mut Terminal<B>) -> Result<(), B::Error> {
  let speaking = speaking();
  // Only speak while our console is in front; when it comes back, start
  // with a full announcement of where the user is.
  let foreground = speaking && SPEAKER.get().is_none_or(Speaker::foreground);
  let (cursor, speech) = STATE.with_borrow_mut(|st| {
    let cursor = st.frame.cursor.or(st.frame.default_cursor);
    let speech = if !speaking {
      None
    } else if !foreground {
      st.away = true;
      None
    } else {
      if std::mem::take(&mut st.away) {
        st.repeat = true;
      }
      Some(st.compose(Instant::now()))
    };
    (cursor, speech)
  });
  if let Some(pos) = cursor {
    terminal.set_cursor_position(pos)?;
  }
  if let Some(utterances) = speech
    && let Some(speaker) = SPEAKER.get()
  {
    if let Some(main) = &utterances.interrupt {
      speaker.say(main, true);
    }
    for queued in &utterances.queued {
      speaker.say(queued, false);
    }
  }
  Ok(())
}

// ---------------------------------------------------------------------------
// What to say
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Utterances {
  /// Replaces whatever is being spoken (navigation, page changes).
  interrupt: Option<Vec<u8>>,
  /// Appended after it (installation progress).
  queued: Vec<Vec<u8>>,
}

impl State {
  fn compose(&mut self, now: Instant) -> Utterances {
    let mut out = Speech::default();
    let first = !std::mem::replace(&mut self.greeted, true);
    if first {
      out.text(GREETING);
    }
    let repeat = std::mem::take(&mut self.repeat);
    let frame = &mut self.frame;
    let seen = &mut self.seen;

    if let Some(screen) = self.screen.take() {
      out.limit = MAX_SCREEN;
      out.text(&screen);
    } else if let Some(modal) = &frame.modal {
      if repeat || seen.modal.as_ref() != Some(modal) {
        out.text(modal);
      }
    } else {
      let modal_closed = seen.modal.is_some();
      let full_item = first || repeat || modal_closed;
      if let Some(focus) = &frame.focus {
        let prev = seen.focus.as_ref();
        let region_changed = prev.is_none_or(|p| p.region != focus.region);
        if (full_item || region_changed) && !focus.region.is_empty() {
          out.text(&focus.region);
        }
        if full_item || region_changed {
          out.text(&focus.text);
        } else if prev != Some(focus) {
          describe_change(focus, prev, &mut out);
        }
      }
      if first || repeat || frame.context != seen.context {
        out.text(&frame.context);
      }
    }
    if let Some(alert) = self.alert.take() {
      out.text(&alert);
    }

    seen.focus = frame.focus.take();
    std::mem::swap(&mut seen.context, &mut frame.context);
    seen.modal = frame.modal.take();

    let mut utterances = Utterances::default();
    if let Some(main) = out.finish() {
      // Something more relevant is being said; a pending milestone is stale.
      self.info = None;
      utterances.interrupt = Some(main);
    }
    for msg in self.urgent.drain(..) {
      utterances.queued.extend(Speech::once(&msg));
    }
    match std::mem::take(&mut self.urgent_dropped) {
      0 => {}
      1 => utterances
        .queued
        .extend(Speech::once("1 more message in the log")),
      n => utterances
        .queued
        .extend(Speech::once(&format!("{n} more messages in the log"))),
    }
    if self.info.is_some()
      && self
        .info_at
        .is_none_or(|t| now.duration_since(t) >= INFO_INTERVAL)
    {
      if let Some(info) = self.info.take() {
        utterances.queued.extend(Speech::once(&info));
      }
      self.info_at = Some(now);
    }
    let busy = self.frame.busy.take();
    if utterances.interrupt.is_some() || !utterances.queued.is_empty() {
      self.last_spoken = Some(now);
    } else if let Some(status) = busy {
      match self.last_spoken {
        // Silent long enough: reassure, with the latest progress line.
        Some(t) if now.duration_since(t) >= BUSY_INTERVAL => {
          let text = format!("Still working\n{status}");
          utterances.queued.extend(Speech::once(&text));
          self.last_spoken = Some(now);
        }
        Some(_) => {}
        None => self.last_spoken = Some(now),
      }
    }
    utterances
  }
}

/// Speak only what changed in a field the user is editing.
fn describe_change(now: &Focus, prev: Option<&Focus>, out: &mut Speech) {
  let (Some(e), Some(p)) = (&now.edit, prev.and_then(|p| p.edit.as_ref())) else {
    out.text(&now.text);
    return;
  };
  let name = |c: char| -> String {
    if e.secret {
      "star".into()
    } else {
      char_name(c)
    }
  };
  if e.value == p.value {
    if e.cursor != p.cursor {
      match e.value.get(e.cursor) {
        Some(&c) => out.text(&name(c)),
        None => out.text("end"),
      }
    }
  } else {
    let typed = e.value.len() == p.value.len() + 1 && e.cursor == p.cursor + 1;
    let erased = e.value.len() + 1 == p.value.len();
    let changed = if typed {
      e.value.get(p.cursor)
    } else if erased && e.cursor + 1 == p.cursor {
      p.value.get(e.cursor) // backspace
    } else if erased && e.cursor == p.cursor {
      p.value.get(p.cursor) // delete
    } else {
      None
    };
    match changed {
      Some(&c) => out.text(&name(c)),
      None => out.text(&now.text),
    }
  }
}

/// Name for an echoed character. espeak skips most lone punctuation, and
/// characters beyond Latin-1 cannot go through `synth_direct` at all.
fn char_name(c: char) -> String {
  let name = match c {
    ' ' => "space",
    '_' => "underscore",
    '\\' => "backslash",
    '.' => "dot",
    ',' => "comma",
    ';' => "semicolon",
    ':' => "colon",
    '-' => "dash",
    '/' => "slash",
    '@' => "at",
    '#' => "hash",
    '!' => "exclamation",
    '?' => "question",
    '"' => "quote",
    '\'' => "apostrophe",
    '`' => "backtick",
    '(' => "left paren",
    ')' => "right paren",
    '[' => "left bracket",
    ']' => "right bracket",
    '{' => "left brace",
    '}' => "right brace",
    '<' => "less than",
    '>' => "greater than",
    '*' => "star",
    '+' => "plus",
    '=' => "equals",
    '&' => "and",
    '%' => "percent",
    '$' => "dollar",
    '^' => "caret",
    '~' => "tilde",
    '|' => "bar",
    c if u32::from(c) > 0xff => "symbol",
    c => return c.to_string(),
  };
  name.to_owned()
}

/// Builder for one utterance in the bytes `synth_direct` expects.
struct Speech {
  out: Vec<u8>,
  /// A space is owed before the next visible byte.
  space: bool,
  /// A sentence break is owed before the next visible byte.
  stop: bool,
  limit: usize,
  truncated: bool,
}

impl Default for Speech {
  fn default() -> Self {
    Self {
      out: Vec::new(),
      space: false,
      stop: false,
      limit: MAX_UTTERANCE,
      truncated: false,
    }
  }
}

impl Speech {
  fn once(text: &str) -> Option<Vec<u8>> {
    let mut s = Self::default();
    s.text(text);
    s.finish()
  }

  /// Add a part; parts and lines are separated by sentence breaks.
  fn text(&mut self, text: &str) {
    self.stop = true;
    let mut prev = ' ';
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
      match c {
        '\n' => self.stop = true,
        // A dash between spaces is a separator ("Enter - Select"), or a
        // bullet when it starts a line (" - Timezone"): never "dash".
        '-' if prev == ' ' && chars.peek() == Some(&' ') => {
          if self.stop || self.out.is_empty() {
            self.space = true;
          } else {
            self.raw(b',');
          }
        }
        '\\' | '_' => self.space = true,
        c if c.is_whitespace() || c.is_control() => self.space = true,
        ' '..='~' => self.raw(c as u8),
        '\u{a1}'..='\u{ff}' => self.raw(c as u32 as u8),
        _ => self.word(match c {
          '→' | '⇒' | '➜' => "to",
          '←' => "left",
          '↑' => "up",
          '↓' => "down",
          '✓' | '✔' => "done",
          '✗' | '✘' => "failed",
          '…' => "...",
          '–' | '—' | '−' => "-",
          '‘' | '’' | '′' => "'",
          '“' | '”' => "\"",
          // Box drawing, spinners, emoji: not speakable through synth_direct.
          _ => "",
        }),
      }
      prev = c;
    }
  }

  fn word(&mut self, w: &str) {
    if w.is_empty() {
      self.space = true;
      return;
    }
    self.space = true;
    for b in w.bytes() {
      self.raw(b);
    }
    self.space = true;
  }

  fn raw(&mut self, b: u8) {
    // Room for a ". " separator plus the byte itself.
    if self.out.len() + 3 > self.limit {
      self.truncated = true;
      return;
    }
    if !self.out.is_empty() {
      let last = *self.out.last().unwrap_or(&b' ');
      if self.stop && b != b',' && b != b'.' {
        if !matches!(last, b'.' | b'!' | b'?' | b':' | b',' | b';') {
          self.out.push(b'.');
        }
        self.out.push(b' ');
      } else if self.space && b != b',' {
        self.out.push(b' ');
      }
    }
    self.stop = false;
    self.space = false;
    self.out.push(b);
  }

  fn finish(mut self) -> Option<Vec<u8>> {
    if self.out.is_empty() {
      return None;
    }
    if self.truncated {
      // Cut at the last whole word, and say so.
      if let Some(space) = self.out.iter().rposition(|&b| b == b' ') {
        self.out.truncate(space);
      }
      self.out.extend_from_slice(b". The rest was not read.");
    }
    self.out.push(b'\n');
    Some(self.out)
  }
}

/// Test hook: build speech text without a speakup device.
#[cfg(test)]
pub fn enable_for_tests() {
  SPEAKING.store(true, Ordering::Relaxed);
}

/// Test hook: what this thread's last frame would make speakup say.
#[cfg(test)]
pub fn utterance_for_tests() -> String {
  STATE.with_borrow_mut(|st| {
    st.greeted = true;
    st.compose(Instant::now())
      .interrupt
      .map(|b| String::from_utf8_lossy(&b).trim_end().to_string())
      .unwrap_or_default()
  })
}

/// Test hook: what this thread's last frame would queue (progress).
#[cfg(test)]
pub fn queued_for_tests() -> Vec<String> {
  STATE.with_borrow_mut(|st| {
    st.greeted = true;
    st.compose(Instant::now())
      .queued
      .iter()
      .map(|b| String::from_utf8_lossy(b).trim_end().to_string())
      .collect()
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  fn spoken(text: &str) -> String {
    Speech::once(text)
      .map(|b| String::from_utf8_lossy(&b).trim_end().to_string())
      .unwrap_or_default()
  }

  fn state() -> State {
    let mut st = State::new();
    st.greeted = true;
    st
  }

  fn frame(st: &mut State, region: &str, text: &str, context: &str) -> String {
    st.frame.focus = Some(Focus {
      region: region.into(),
      text: text.into(),
      edit: None,
    });
    st.frame.context = context.into();
    let u = st.compose(Instant::now());
    u.interrupt
      .map(|b| String::from_utf8_lossy(&b).trim_end().to_string())
      .unwrap_or_default()
  }

  fn edit_frame(st: &mut State, value: &str, cursor: usize, secret: bool) -> String {
    let value: Vec<char> = value.chars().collect();
    st.frame.focus = Some(Focus {
      region: "Set Hostname".into(),
      text: value.iter().collect(),
      edit: Some(Edit {
        value,
        cursor,
        secret,
      }),
    });
    let u = st.compose(Instant::now());
    u.interrupt
      .map(|b| String::from_utf8_lossy(&b).trim_end().to_string())
      .unwrap_or_default()
  }

  #[test]
  fn speech_is_latin1_without_backslashes_or_box_drawing() {
    let b = Speech::once("│ São Paulo ✓ a\\b ─┐").unwrap();
    assert!(b.iter().all(|&c| c != b'\\'));
    assert_eq!(&b[..], b"S\xe3o Paulo done a b\n");
  }

  #[test]
  fn lines_become_sentences_and_dashes_become_pauses() {
    assert_eq!(spoken("Main Menu\nTimezone"), "Main Menu. Timezone");
    assert_eq!(spoken("Set to:\nEurope/Rome"), "Set to: Europe/Rome");
    assert_eq!(
      spoken("Enter - Select and return"),
      "Enter, Select and return"
    );
    assert_eq!(spoken("Done.\n\n\nNext"), "Done. Next");
    assert_eq!(spoken("America/New_York"), "America/New York");
    assert_eq!(
      spoken("Not configured:\n - Base System\n - Timezone"),
      "Not configured: Base System. Timezone"
    );
  }

  #[test]
  fn speech_is_bounded() {
    let long = "word ".repeat(1000);
    let said = Speech::once(&long).unwrap();
    assert!(said.len() <= MAX_UTTERANCE + 30, "{}", said.len());
    assert!(said.ends_with(b"The rest was not read.\n"));
  }

  #[test]
  fn only_changes_are_spoken() {
    let mut st = state();
    assert_eq!(
      frame(&mut st, "Main Menu", "Base System, 1 of 12", "About base"),
      "Main Menu. Base System, 1 of 12. About base"
    );
    // Same frame again (idle redraw): silence.
    assert_eq!(
      frame(&mut st, "Main Menu", "Base System, 1 of 12", "About base"),
      ""
    );
    // Moving within the same list: no region, new item and new description.
    assert_eq!(
      frame(&mut st, "Main Menu", "Timezone, 2 of 12", "About tz"),
      "Timezone, 2 of 12. About tz"
    );
    // Description unchanged: item only.
    assert_eq!(
      frame(&mut st, "Main Menu", "Locale, 4 of 12", "About tz"),
      "Locale, 4 of 12"
    );
    // New page: region again.
    assert_eq!(
      frame(&mut st, "Select Timezone", "Europe/Rome, 1 of 400", ""),
      "Select Timezone. Europe/Rome, 1 of 400"
    );
  }

  #[test]
  fn repeat_says_everything() {
    let mut st = state();
    frame(&mut st, "Main Menu", "Timezone, 2 of 12", "About tz");
    st.repeat = true;
    assert_eq!(
      frame(&mut st, "Main Menu", "Timezone, 2 of 12", "About tz"),
      "Main Menu. Timezone, 2 of 12. About tz"
    );
  }

  #[test]
  fn modal_takes_over_and_closing_it_returns_to_the_item() {
    let mut st = state();
    frame(&mut st, "Main Menu", "Timezone, 2 of 12", "About tz");
    st.frame.modal = Some("Help: Main Menu\nq - Quit".into());
    assert_eq!(
      frame(&mut st, "Main Menu", "Timezone, 2 of 12", "About tz"),
      "Help: Main Menu. q, Quit"
    );
    st.frame.modal = Some("Help: Main Menu\nq - Quit".into());
    assert_eq!(
      frame(&mut st, "Main Menu", "Timezone, 2 of 12", "About tz"),
      ""
    );
    assert_eq!(
      frame(&mut st, "Main Menu", "Timezone, 2 of 12", "About tz"),
      "Main Menu. Timezone, 2 of 12"
    );
  }

  #[test]
  fn typing_is_echoed_per_character() {
    let mut st = state();
    assert_eq!(edit_frame(&mut st, "", 0, false), "Set Hostname");
    assert_eq!(edit_frame(&mut st, "a", 1, false), "a");
    assert_eq!(edit_frame(&mut st, "a-", 2, false), "dash");
    assert_eq!(edit_frame(&mut st, "a", 1, false), "dash"); // backspace
    assert_eq!(edit_frame(&mut st, "a", 0, false), "a"); // cursor left
    assert_eq!(edit_frame(&mut st, "a", 1, false), "end");
    assert_eq!(edit_frame(&mut st, "", 0, false), "a");
    assert_eq!(edit_frame(&mut st, "xyz", 3, false), "xyz"); // paste / clear
  }

  #[test]
  fn secrets_are_never_spoken() {
    let mut st = state();
    edit_frame(&mut st, "", 0, true);
    assert_eq!(edit_frame(&mut st, "p", 1, true), "star");
    assert_eq!(edit_frame(&mut st, "pw", 2, true), "star");
    assert_eq!(edit_frame(&mut st, "p", 1, true), "star");
  }

  #[test]
  fn progress_is_throttled_but_errors_are_not() {
    let mut st = state();
    let t0 = Instant::now();
    st.info = Some("first".into());
    let u = st.compose(t0);
    assert_eq!(u.queued.len(), 1);
    st.info = Some("second".into());
    st.info = Some("third".into());
    st.urgent.push("Error: disk".into());
    let u = st.compose(t0 + Duration::from_secs(1));
    assert_eq!(u.queued.len(), 1, "error now, milestone held back");
    let u = st.compose(t0 + INFO_INTERVAL);
    assert_eq!(
      u.queued,
      vec![b"third\n".to_vec()],
      "only the latest milestone"
    );
  }

  #[test]
  fn long_silent_work_gets_a_heartbeat() {
    let mut st = state();
    let t0 = Instant::now();
    let busy = |st: &mut State, t: Instant, status: &str| {
      st.frame.busy = Some(status.into());
      st.compose(t).queued
    };
    st.last_spoken = Some(t0);
    assert!(busy(&mut st, t0 + Duration::from_secs(10), "(1/9) linux").is_empty());
    assert_eq!(
      busy(&mut st, t0 + BUSY_INTERVAL, "(3/9) installing linux 40%"),
      vec![b"Still working. (3/9) installing linux 40%\n".to_vec()]
    );
    // The heartbeat counts as speech: the next one waits another interval.
    assert!(busy(&mut st, t0 + BUSY_INTERVAL + Duration::from_secs(5), "x").is_empty());
  }

  #[test]
  fn screen_reading_is_long_but_bounded_and_says_when_cut() {
    let mut st = state();
    st.screen = Some("word ".repeat(2000));
    let main = st.compose(Instant::now()).interrupt.unwrap();
    assert!(main.len() > MAX_UTTERANCE && main.len() <= MAX_SCREEN + 30);
    assert!(main.ends_with(b"The rest was not read.\n"));
    let pieces: Vec<_> = chunks(&main, MAX_WRITE).collect();
    assert!(pieces.len() >= 2 && pieces.iter().all(|p| p.len() <= MAX_WRITE));
    assert_eq!(pieces.concat(), main);
  }

  #[test]
  fn error_bursts_are_capped() {
    let mut st = state();
    for i in 0..10 {
      if st.urgent.len() < MAX_URGENT {
        st.urgent.push(format!("Warning: w{i}"));
      } else {
        st.urgent_dropped += 1;
      }
    }
    let q = st.compose(Instant::now()).queued;
    assert_eq!(q.len(), MAX_URGENT + 1);
    assert_eq!(q.last().unwrap(), b"7 more messages in the log\n");
  }

  #[test]
  fn progress_bars_are_not_read() {
    assert_eq!(
      strip_bars("(12/40) installing linux   [#####-----]  40%"),
      "(12/40) installing linux 40%"
    );
    assert_eq!(strip_bars("keep [this] text"), "keep [this] text");
    assert_eq!(
      strip_bars("(841/841) checking keys ############## 100%"),
      "(841/841) checking keys 100%"
    );
    assert_eq!(strip_bars("pacman  [--C o o]  12%"), "pacman 12%");
    assert_eq!(strip_bars("a - b"), "a - b");
    assert_eq!(strip_bars("open [### only"), "open [### only");
  }

  #[test]
  fn punctuation_is_named_when_typed() {
    let mut st = state();
    edit_frame(&mut st, "a", 1, false);
    assert_eq!(edit_frame(&mut st, "a@", 2, false), "at");
    assert_eq!(edit_frame(&mut st, "a@,", 3, false), "comma");
  }

  #[test]
  fn console_is_found_through_sudo() {
    // sudo (use_pty) on tty1: dory's own terminal is a pty (major 136),
    // the parent sudo still has tty1 (major 4, minor 1).
    let pty = "4242 (dory) S 4241 4242 4240 34817 4242 4194560";
    let sudo = "4240 (sudo) S 4239 4240 1000 1025 4240 4194560";
    assert_eq!(parse_stat(pty), Some((4241, 34817)));
    assert_eq!(vt_number(34817), None, "pts/1 is not a console");
    assert_eq!(parse_stat(sudo), Some((4239, 1025)));
    assert_eq!(vt_number(1025), Some(1));
    assert_eq!(vt_number(1024 + 12), Some(12));
    assert_eq!(vt_number(1024), None, "tty0 is the current-console alias");
    assert_eq!(vt_number(0), None, "no controlling terminal");
    // Command names with spaces and parentheses.
    assert_eq!(parse_stat("7 (a) b (c)) S 1 7 7 1025 7"), Some((1, 1025)));
  }

  #[test]
  fn real_proc_stat_parses() {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    let (ppid, _) = parse_stat(&stat).unwrap();
    assert_eq!(ppid, std::os::unix::process::parent_id());
  }

  #[test]
  fn navigation_drops_a_stale_milestone() {
    let mut st = state();
    st.info_at = Some(Instant::now());
    st.info = Some("old".into());
    frame(&mut st, "Install Steps", "x", "");
    assert!(st.info.is_none());
  }
}
