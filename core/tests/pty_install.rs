//! Runs `install()` against a fake `pacstrap` that behaves like pacman on a
//! terminal (progress bars via `\r`, parallel downloads via cursor-up), and
//! checks that only final lines reach the log while progress goes to the
//! live file.

use core::internal::install::{LIVE_FILE_ENV, PTY_COLS_ENV, install};
use shared::args::PackageManager;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Mutex, OnceLock};

static LINES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

struct Capture;
impl log::Log for Capture {
  fn enabled(&self, _: &log::Metadata) -> bool {
    true
  }
  fn log(&self, record: &log::Record) {
    LINES
      .get_or_init(Default::default)
      .lock()
      .unwrap()
      .push(format!("{}", record.args()));
  }
  fn flush(&self) {}
}

#[test]
fn progress_bars_are_live_and_log_stays_clean() {
  let _ = log::set_logger(&Capture).map(|()| log::set_max_level(log::LevelFilter::Info));
  let dir = tempfile::tempdir().unwrap();

  // Fake pacstrap: one package drawn with \r, then two parallel downloads
  // redrawn with cursor-up, like pacman's parallel download block.
  let script = r#"#!/bin/sh
test -t 1 || { echo "NOT A TTY"; exit 2; }
printf 'resolving dependencies...\n'
printf 'pkg-a   0%%\rpkg-a  50%%\rpkg-a 100%%\n'
printf 'pkg-b  10%%\npkg-c  10%%\n'
sleep 0.3
printf '\033[2Apkg-b 100%%\033[K\npkg-c 100%%\033[K\n'
printf 'done, cols=%s\n' "$(stty -F /dev/tty size | cut -d" " -f2)"
"#;
  let bin = dir.path().join("pacstrap");
  std::fs::write(&bin, script).unwrap();
  std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

  let live = dir.path().join("dory.live");
  let path = format!(
    "{}:{}",
    dir.path().display(),
    std::env::var("PATH").unwrap_or_default()
  );
  // SAFETY: single-threaded test setup before spawning.
  unsafe {
    std::env::set_var("PATH", path);
    std::env::set_var(LIVE_FILE_ENV, &live);
    std::env::set_var(PTY_COLS_ENV, "77");
  }

  // Watch the live file from another thread while the install runs.
  let live_path = live.clone();
  let watcher = std::thread::spawn(move || {
    let mut seen = Vec::new();
    for _ in 0..60 {
      if let Ok(s) = std::fs::read_to_string(&live_path) {
        seen.push(s);
      }
      std::thread::sleep(std::time::Duration::from_millis(20));
    }
    seen
  });

  let code = install(PackageManager::Pacstrap, &["dummy".into()]);
  assert_eq!(code, 0);

  let seen = watcher.join().unwrap();
  assert!(
    seen.iter().any(|s| s.contains("pkg-b  10%")),
    "live file never showed in-progress rows: {seen:?}"
  );
  assert!(!live.exists(), "live file must be removed at the end");

  let lines = LINES.get().unwrap().lock().unwrap().clone();
  let joined = lines.join("\n");
  assert!(joined.contains("pkg-a 100%"), "{joined}");
  assert!(
    !joined.contains("pkg-a  50%"),
    "intermediate progress leaked into the log: {joined}"
  );
  assert!(
    joined.contains("pkg-b 100%") && joined.contains("pkg-c 100%"),
    "{joined}"
  );
  assert!(
    !joined.contains("pkg-b  10%"),
    "overwritten rows leaked into the log: {joined}"
  );
  assert!(
    joined.contains("done, cols=77"),
    "pty width not applied: {joined}"
  );
}
