use crate::files;
use flexi_logger::{DeferredNow, Duplicate, FileSpec, LogSpecification, Logger, style};
use log::LevelFilter;
use std::{fs, io::Write, path::Path};

/// Initialise the file logger.
///
/// `echo_to_stderr` should be `true` only for the non-interactive CLI path:
/// in TUI mode anything written to stderr lands on top of the ratatui frame.
pub fn init(verbosity: u8, log_file_path: &str, echo_to_stderr: bool) {
  let level = match verbosity {
    0 => LevelFilter::Info,
    1 => LevelFilter::Debug,
    _ => LevelFilter::Trace,
  };
  let log_specification = LogSpecification::builder().default(level).build();

  // Start every run with a fresh log file.
  if fs::metadata(log_file_path).is_ok() {
    files::remove_file(log_file_path);
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

  Logger::with(log_specification)
    .log_to_file(spec)
    .duplicate_to_stderr(if echo_to_stderr {
      Duplicate::All
    } else {
      Duplicate::None
    })
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
