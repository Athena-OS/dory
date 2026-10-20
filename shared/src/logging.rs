use flexi_logger::{DeferredNow, Duplicate, FileSpec, LogSpecification, Logger, style};
use log::LevelFilter;
use std::{fs, io::Write, path::Path};

/// How the log file is opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogMode {
  /// Start a fresh log and also echo it to stderr (plain CLI runs).
  FreshWithStderr,
  /// Start a fresh log, file only (the TUI: stderr would draw over the frame).
  Fresh,
  /// Append to an existing log, file only (the installer child spawned by
  /// the TUI: the parent owns the file and tails it).
  Append,
}

/// Initialise the file logger. The file is always opened in append mode so
/// that other writers to the same file (the TUI's child redirect) interleave
/// correctly instead of overwriting each other's offsets.
pub fn init(verbosity: u8, log_file_path: &str, mode: LogMode) {
  let level = match verbosity {
    0 => LevelFilter::Info,
    1 => LevelFilter::Debug,
    _ => LevelFilter::Trace,
  };
  let log_specification = LogSpecification::builder().default(level).build();

  if mode != LogMode::Append {
    // Truncate rather than unlink: readers that already hold the file open
    // keep seeing the same inode.
    let _ = fs::write(log_file_path, b"");
  }

  let p = Path::new(log_file_path);
  let dir = p.parent().unwrap_or_else(|| Path::new("."));
  let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("dory");
  let ext = p
    .extension()
    .and_then(|s| s.to_str())
    .filter(|e| !e.is_empty());

  let mut spec = FileSpec::default()
    .directory(dir)
    .basename(stem)
    .suppress_timestamp();
  if let Some(ext) = ext {
    spec = spec.suffix(ext);
  }

  let duplicate = if mode == LogMode::FreshWithStderr {
    Duplicate::All
  } else {
    Duplicate::None
  };
  Logger::with(log_specification)
    .log_to_file(spec)
    .append()
    .duplicate_to_stderr(duplicate)
    .format(format_log_entry)
    .start()
    .expect("failed to start logger");
}

/// Formats a log entry with color
fn format_log_entry(
  w: &mut dyn Write,
  now: &mut DeferredNow,
  record: &log::Record,
) -> std::io::Result<()> {
  let level = record.level();
  write!(
    w,
    "[ {} ] {} {}",
    style(level).paint(level.to_string()),
    now.now().time().format("%H:%M:%S"),
    record.args()
  )
}
