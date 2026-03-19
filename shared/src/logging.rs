use flexi_logger::{style, DeferredNow, FileSpec, LogSpecification, Logger, Duplicate};
use log::LevelFilter;
use std::{fs, io::Write, path::Path};
use crate::files;

pub fn init(verbosity: u8, log_file_path: &str) {
    let log_specification = match verbosity {
        0 => LogSpecification::builder()
            .default(LevelFilter::Info)
            .build(),
        1 => LogSpecification::builder()
            .default(LevelFilter::Debug)
            .build(),
        _ => LogSpecification::builder()
            .default(LevelFilter::Trace)
            .build(),
    };

    // Check if the log file already exists
    if fs::metadata(log_file_path).is_ok() {
        // If an old log file exists, remove it
        files::remove_file(log_file_path);
    }

    let p = Path::new(log_file_path);
    let dir = p.parent().unwrap_or_else(|| Path::new("."));
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("dory");
    let ext  = p.extension().and_then(|s| s.to_str()); // e.g. "log"

    let mut spec = FileSpec::default()
        .directory(dir)
        .basename(stem)
        .suppress_timestamp();

    // Only add suffix if there is one; avoids trailing dot
    if let Some(ext) = ext
        && !ext.is_empty() {
            spec = spec.suffix(ext);
        }

    // Create a file-based logger and specify the log file path
    Logger::with(log_specification)
        .log_to_file(spec)
        .duplicate_to_stderr(Duplicate::All)
        .format(format_log_entry)
        .start()
        .unwrap();
}

/// Formats a log entry with color
fn format_log_entry(
    w: &mut dyn Write,
    now: &mut DeferredNow,
    record: &log::Record,
) -> std::io::Result<()> {
    let msg = record.args().to_string();
    let level = record.level();
    
    // Get the current time as a NaiveTime
    let time = now.now().time();
    
    // Format the time as a string
    let time_str = time.format("%H:%M:%S").to_string();
    
    write!(
        w,
        "[ {} ] {} {}",
        style(level).paint(level.to_string()),
        time_str,
        msg
    )
}
