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
//!   speaks itself, with RHVoice through a private speech-dispatcher when
//!   available, else through speakup's `synth_direct`: the focused item, what changed and
//!   help popups. The installation log is never spoken, only the install
//!   steps and the result page. speakup is restored on exit, on panic and on
//!   fatal signals.
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
  io::{self, BufRead, BufReader, Write},
  os::unix::process::CommandExt,
  os::unix::{fs::FileExt, net::UnixStream},
  path::Path,
  process::{Command, Stdio},
  sync::{
    Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
  },
  time::{Duration, Instant},
};

use log::{info, warn};
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
  /// The natural voice, when it could be started; otherwise speech goes to
  /// speakup's own espeak-ng through `synth_direct`.
  voice: Mutex<Option<Voice>>,
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
      voice: Mutex::new(None),
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

  /// Replace whatever is being spoken with `text`.
  fn say(&self, text: &[u8]) {
    if text.is_empty() || !self.foreground() {
      return;
    }
    self.silent(MUTE_AND_FLUSH);
    if let Ok(mut voice) = self.voice.lock()
      && let Some(v) = voice.as_mut()
    {
      match v.speak(text) {
        Ok(()) => return,
        Err(e) => {
          warn!("Natural voice failed ({e}), back to espeak-ng");
          *voice = None;
        }
      }
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
/// The natural voice was used this session: espeakup is restarted on exit.
static VOICE_USED: AtomicBool = AtomicBool::new(false);

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
  // Mute and flush before the natural voice takes the sound device, so
  // espeakup is not in the middle of speaking (e.g. echoing "sudo dory").
  speaker.silent(MUTE_AND_FLUSH);
  info!("Speech on ({})", speaker.vt);
  match Voice::start(&voice_alsa_device()) {
    Ok(voice) => {
      info!("Speech voice: RHVoice {}", voice.name);
      VOICE_USED.store(true, Ordering::SeqCst);
      if let Ok(mut slot) = speaker.voice.lock() {
        *slot = Some(voice);
      }
    }
    Err(why) => info!("Speech voice: espeak-ng ({why})"),
  }
  Some(Session(()))
}

/// Give screen reading back to speakup. Idempotent; safe from the panic hook
/// and the signal thread.
pub fn shutdown() {
  if SPEAKING.swap(false, Ordering::SeqCst)
    && let Some(speaker) = SPEAKER.get()
  {
    // Stop the voice and wait until its speech-dispatcher has released the
    // sound device, then hand speech back. try_lock: a panic in the middle
    // of `say` holds the lock.
    let voice = speaker.voice.try_lock().ok().and_then(|mut v| v.take());
    drop(voice);
    if speaker.foreground() {
      speaker.silent(UNMUTE_AND_FLUSH);
    } else {
      // `silent` would hit the console the user is on, and ours would stay
      // muted. Leave a watcher that unmutes ours when the user returns.
      unmute_when_back(&speaker.vt);
    }
    if VOICE_USED.load(Ordering::SeqCst) {
      restart_espeakup();
    }
  }
}

/// espeakup 0.90 (2021, still Arch's) loops silently forever once a write
/// to the sound device failed, e.g. while dory's voice held it; recovery
/// only exists upstream since 2026. A restart is cheap and always clean.
/// --no-block: queue the job and return, also from the panic hook.
fn restart_espeakup() {
  let _ = Command::new("systemctl")
    .args(["--no-block", "try-restart", "espeakup.service"])
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status();
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
// Natural voice: RHVoice through a private speech-dispatcher
// ---------------------------------------------------------------------------

/// speakup itself can only drive espeak-ng (through espeakup). While dory
/// runs it does its own speaking anyway, so it can use a natural voice:
/// RHVoice's "slt" through a private speech-dispatcher instance it starts
/// itself. That instance uses ALSA (as root there is no PulseAudio or
/// PipeWire) and exits a few seconds after dory closes the connection.
const VOICE: &str = "slt";
const SPEECHD_DIR: &str = "/run/dory-speech";
const SPEECHD_START: Duration = Duration::from_secs(4);
const SPEECHD_REPLY: Duration = Duration::from_secs(2);
const SPEECHD_STOP: Duration = Duration::from_secs(1);

/// ALSA device for the voice; overridable for tests (`null`).
fn voice_alsa_device() -> String {
  std::env::var("DORY_SPEECH_ALSA_DEVICE").unwrap_or_else(|_| "default".into())
}

struct Voice {
  writer: UnixStream,
  reader: BufReader<UnixStream>,
  name: String,
  /// The private speech-dispatcher, leader of its own process group (its
  /// modules, which hold the sound device, are in the same group). Only
  /// held for its `Drop`.
  #[cfg_attr(not(test), allow(dead_code))]
  daemon: Daemon,
}

/// Stops the speech-dispatcher process group when dropped, and waits until
/// it is gone: espeakup 0.90 (Arch) never recovers when it tries to speak
/// while the sound device is still busy.
struct Daemon(std::process::Child);

impl Drop for Daemon {
  fn drop(&mut self) {
    let Ok(pgid) = i32::try_from(self.0.id()) else {
      return;
    };
    // SAFETY: plain kill(2) on our own child's process group.
    let group_alive = || unsafe { libc::kill(-pgid, 0) } == 0;
    unsafe { libc::kill(-pgid, libc::SIGTERM) };
    let deadline = Instant::now() + SPEECHD_STOP;
    loop {
      // Reap the leader, or it stays in the group as a zombie.
      let _ = self.0.try_wait();
      if !group_alive() || Instant::now() >= deadline {
        break;
      }
      std::thread::sleep(Duration::from_millis(10));
    }
    if group_alive() {
      unsafe { libc::kill(-pgid, libc::SIGKILL) };
    }
    let _ = self.0.wait();
  }
}

impl Voice {
  fn start(alsa_device: &str) -> Result<Self, String> {
    let dir = Path::new(SPEECHD_DIR);
    let conf = dir.join("conf");
    let sock = dir.join("speechd.sock");
    let logs = dir.join("log");
    for d in [&conf, &logs] {
      std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let _ = std::fs::write(conf.join("speechd.conf"), speechd_conf(alsa_device));
    let _ = std::fs::remove_file(&sock);

    // Single mode: stays our child, so it can be stopped (with its
    // modules) before speakup's espeakup needs the sound device again.
    let mut daemon = Daemon(
      Command::new("speech-dispatcher")
        .args(["-s", "-c", "unix_socket", "-t", "0", "-S"])
        .arg(&sock)
        .arg("-C")
        .arg(&conf)
        .arg("-L")
        .arg(&logs)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("speech-dispatcher not available ({e})"))?,
    );

    let deadline = Instant::now() + SPEECHD_START;
    let stream = loop {
      match UnixStream::connect(&sock) {
        Ok(s) => break s,
        // With RHVoice as its only module, speech-dispatcher exits when the
        // module cannot start (no sound device): say why, from its log.
        Err(_) if matches!(daemon.0.try_wait(), Ok(Some(_))) => {
          return Err(match last_error(&logs.join("rhvoice.log")) {
            Some(e) => format!("RHVoice could not start: {e}"),
            None => "speech-dispatcher exited".into(),
          });
        }
        Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
        Err(e) => return Err(format!("speech-dispatcher did not answer ({e})")),
      }
    };
    let io_err = |e: io::Error| format!("speech-dispatcher: {e}");
    stream
      .set_read_timeout(Some(SPEECHD_REPLY))
      .map_err(io_err)?;
    stream
      .set_write_timeout(Some(SPEECHD_REPLY))
      .map_err(io_err)?;
    let reader = BufReader::new(stream.try_clone().map_err(io_err)?);
    let mut voice = Self {
      writer: stream,
      reader,
      name: String::new(),
      daemon,
    };

    voice
      .cmd("SET self CLIENT_NAME root:dory:main")
      .map_err(io_err)?;
    // speech-dispatcher answers "OK" to any module name: check the list.
    let modules = voice.cmd("LIST OUTPUT_MODULES").map_err(io_err)?;
    if !modules.iter().any(|m| reply_text(m) == "rhvoice") {
      // The module also drops out when it cannot open the sound device:
      // say which, from its own log.
      return Err(match last_error(&logs.join("rhvoice.log")) {
        Some(e) => format!("RHVoice could not start: {e}"),
        None => "RHVoice is not installed".into(),
      });
    }
    voice
      .cmd("SET self OUTPUT_MODULE rhvoice")
      .map_err(io_err)?;
    let voices = voice.cmd("LIST SYNTHESIS_VOICES").map_err(io_err)?;
    voice.name = find_voice(&voices, VOICE).ok_or("the RHVoice slt voice is not installed")?;
    let set = format!("SET self SYNTHESIS_VOICE {}", voice.name);
    voice.cmd(&set).map_err(io_err)?;
    Ok(voice)
  }

  /// Send one command; return the reply lines. Errors on a non-2xx reply.
  fn cmd(&mut self, line: &str) -> io::Result<Vec<String>> {
    self.writer.write_all(line.as_bytes())?;
    self.writer.write_all(b"\r\n")?;
    self.reply()
  }

  fn reply(&mut self) -> io::Result<Vec<String>> {
    let mut lines = Vec::new();
    loop {
      let mut line = String::new();
      if self.reader.read_line(&mut line)? == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "closed"));
      }
      let line = line.trim_end_matches(['\r', '\n']).to_string();
      // "NNN-text" continues, "NNN text" ends the reply.
      let last = line.as_bytes().get(3) == Some(&b' ');
      let ok = line.starts_with('2');
      lines.push(line);
      if last {
        return if ok {
          Ok(lines)
        } else {
          Err(io::Error::other(lines.join(" | ")))
        };
      }
    }
  }

  /// Replace whatever is being spoken with `latin1` (the bytes built for
  /// `synth_direct`).
  fn speak(&mut self, latin1: &[u8]) -> io::Result<()> {
    self.cmd("CANCEL self")?;
    self.cmd("SPEAK")?;
    self.writer.write_all(ssip_message(latin1).as_bytes())?;
    self.reply().map(|_| ())
  }
}

/// Last error line of a speech-dispatcher module log, without its timestamp.
fn last_error(log: &Path) -> Option<String> {
  let text = std::fs::read_to_string(log).ok()?;
  let line = text
    .lines()
    .rev()
    .find(|l| l.to_lowercase().contains("error"))?;
  // "Wed Oct 21 15:21:53 2026  Audio output initialization error: ..."
  let msg = line.split_once("  ").map_or(line, |(_, m)| m);
  Some(msg.trim().to_string())
}

fn speechd_conf(alsa_device: &str) -> String {
  format!(
    "# Private speech-dispatcher for the dory installer.
AudioOutputMethod \"alsa\"
AudioALSADevice \"{alsa_device}\"
# Only RHVoice: every other module would be started and open the sound
# device too.
AddModule \"rhvoice\" \"sd_rhvoice\" \"rhvoice.conf\"
DefaultModule rhvoice
DefaultLanguage \"en-US\"
"
  )
}

/// Text of a reply line: `250-rhvoice` -> `rhvoice`.
fn reply_text(line: &str) -> &str {
  line.get(4..).unwrap_or("")
}

/// The voice called `wanted` (any case) in a `LIST SYNTHESIS_VOICES` reply
/// (`249-Slt\ten\tnone`), as speech-dispatcher spells it.
fn find_voice(reply: &[String], wanted: &str) -> Option<String> {
  reply.iter().find_map(|l| {
    let name = reply_text(l).split('\t').next()?;
    name.eq_ignore_ascii_case(wanted).then(|| name.to_string())
  })
}

/// SSIP message body: UTF-8, one line, dot-stuffed, ending with `\r\n.\r\n`.
fn ssip_message(latin1: &[u8]) -> String {
  let text: String = latin1
    .iter()
    .map(|&b| char::from(b))
    .map(|c| if c == '\r' || c == '\n' { ' ' } else { c })
    .collect();
  let text = text.trim();
  let dot = if text.starts_with('.') { "." } else { "" };
  format!("{dot}{text}\r\n.\r\n")
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
      st.compose()
    };
    (cursor, speech)
  });
  if let Some(pos) = cursor {
    terminal.set_cursor_position(pos)?;
  }
  if let Some(text) = speech
    && let Some(speaker) = SPEAKER.get()
  {
    speaker.say(&text);
  }
  Ok(())
}

// ---------------------------------------------------------------------------
// What to say
// ---------------------------------------------------------------------------

impl State {
  /// What to say for this frame; it replaces whatever is being spoken.
  fn compose(&mut self) -> Option<Vec<u8>> {
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
      let context_changed = first || repeat || frame.context != seen.context;
      let prev = seen.focus.as_ref();
      let focus = frame.focus.as_ref();
      let region_changed = focus.is_some_and(|f| prev.is_none_or(|p| p.region != f.region));
      let announce = full_item || region_changed;
      // Typing in a field: the echoed character comes first, a changing
      // count ("12 matches") after it.
      let echo = focus.filter(|f| !announce && f.edit.is_some() && prev != Some(*f));
      if let Some(f) = echo {
        describe_change(f, prev, &mut out);
      }
      // Otherwise the description first, then the focused control (label,
      // then control): on a new page the user hears what it is about before
      // which button or field has focus.
      if context_changed {
        out.text(&frame.context);
      }
      if let Some(f) = focus
        && echo.is_none()
      {
        if announce {
          if !f.region.is_empty() {
            out.text(&f.region);
          }
          out.text(&f.text);
        } else if prev != Some(f) {
          describe_change(f, prev, &mut out);
        }
      }
    }
    if let Some(alert) = self.alert.take() {
      out.text(&alert);
    }

    seen.focus = frame.focus.take();
    std::mem::swap(&mut seen.context, &mut frame.context);
    seen.modal = frame.modal.take();

    out.finish()
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
  #[cfg(test)]
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
    st.compose()
      .map(|b| String::from_utf8_lossy(&b).trim_end().to_string())
      .unwrap_or_default()
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
    st.compose()
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
    st.compose()
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
      "About base. Main Menu. Base System, 1 of 12"
    );
    // Same frame again (idle redraw): silence.
    assert_eq!(
      frame(&mut st, "Main Menu", "Base System, 1 of 12", "About base"),
      ""
    );
    // Moving within the same list: new description first, then the item;
    // the list name is not repeated.
    assert_eq!(
      frame(&mut st, "Main Menu", "Timezone, 2 of 12", "About tz"),
      "About tz. Timezone, 2 of 12"
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
      "About tz. Main Menu. Timezone, 2 of 12"
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
  fn fields_and_checkboxes_follow_their_description() {
    let mut st = state();
    st.frame.context = "The hostname identifies your computer.".into();
    assert_eq!(
      edit_frame(&mut st, "", 0, false),
      "The hostname identifies your computer. Set Hostname"
    );
    assert_eq!(
      frame(
        &mut st,
        "",
        "Encrypt ROOT (LUKS), checkbox, not checked",
        "Encrypt ROOT?\nOptionally enable LUKS on the root partition only."
      ),
      "Encrypt ROOT? Optionally enable LUKS on the root partition only. \
       Encrypt ROOT (LUKS), checkbox, not checked"
    );
  }

  #[test]
  fn a_typed_character_comes_before_a_changing_count() {
    let mut st = state();
    st.frame.context = "400 matches".into();
    edit_frame(&mut st, "", 0, false);
    st.frame.context = "12 matches".into();
    assert_eq!(edit_frame(&mut st, "r", 1, false), "r. 12 matches");
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
  fn screen_reading_is_long_but_bounded_and_says_when_cut() {
    let mut st = state();
    st.screen = Some("word ".repeat(2000));
    let main = st.compose().unwrap();
    assert!(main.len() > MAX_UTTERANCE && main.len() <= MAX_SCREEN + 30);
    assert!(main.ends_with(b"The rest was not read.\n"));
    let pieces: Vec<_> = chunks(&main, MAX_WRITE).collect();
    assert!(pieces.len() >= 2 && pieces.iter().all(|p| p.len() <= MAX_WRITE));
    assert_eq!(pieces.concat(), main);
  }

  #[test]
  fn punctuation_is_named_when_typed() {
    let mut st = state();
    edit_frame(&mut st, "a", 1, false);
    assert_eq!(edit_frame(&mut st, "a@", 2, false), "at");
    assert_eq!(edit_frame(&mut st, "a@,", 3, false), "comma");
  }

  #[test]
  fn ssip_messages_are_utf8_single_line_and_dot_stuffed() {
    assert_eq!(
      ssip_message(b"S\xe3o Paulo, 1 of 2\n"),
      "São Paulo, 1 of 2\r\n.\r\n"
    );
    assert_eq!(ssip_message(b".hidden file\n"), "..hidden file\r\n.\r\n");
    assert_eq!(ssip_message(b"a\r\nb"), "a  b\r\n.\r\n");
  }

  #[test]
  fn the_voice_is_found_whatever_its_case() {
    let reply: Vec<String> = [
      "249-Alan\ten\tnone",
      "249-Slt\ten\tnone",
      "249 OK VOICE LIST SENT",
    ]
    .map(String::from)
    .into();
    assert_eq!(find_voice(&reply, "slt").as_deref(), Some("Slt"));
    assert_eq!(find_voice(&reply, "clb"), None);
    assert_eq!(reply_text("250-rhvoice"), "rhvoice");
  }

  /// Needs speech-dispatcher with RHVoice installed. Run with `--ignored`.
  #[test]
  #[ignore]
  fn a_sound_device_failure_is_reported_as_such() {
    let err = Voice::start("no-such-device").err().expect("must fail");
    assert!(err.starts_with("RHVoice could not start: "), "{err}");
    assert!(!err.contains("2026"), "timestamp stripped: {err}");
  }

  /// Needs speech-dispatcher with RHVoice and the slt voice installed;
  /// plays to ALSA's `null` device. Run with `--ignored`.
  #[test]
  #[ignore]
  fn the_natural_voice_speaks() {
    let mut voice = Voice::start("null").expect("start");
    assert!(voice.name.eq_ignore_ascii_case("slt"));
    voice
      .speak(b"Main Menu. Timezone, 2 of 12.\n")
      .expect("speak");
    voice
      .speak(b"Replaced before it finished.\n")
      .expect("interrupt");
    let pgid = i32::try_from(voice.daemon.0.id()).unwrap();
    let t = Instant::now();
    drop(voice);
    assert!(t.elapsed() < SPEECHD_STOP + Duration::from_millis(200));
    let alive = unsafe { libc::kill(-pgid, 0) } == 0;
    assert!(
      !alive,
      "speech-dispatcher and its modules are gone after drop"
    );
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
}
