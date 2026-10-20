use crate::strings::crash;
use log::info;
use std::io;

/// Exit code to use for an I/O error: the OS errno when there is one,
/// otherwise 1. Never panics (regex errors, UTF-8 errors etc. have no errno).
fn exit_code(e: &io::Error) -> i32 {
  e.raw_os_error().unwrap_or(1)
}

/// Log `logmsg` and return the value on success; crash with the error otherwise.
pub fn exec_eval<T>(result: io::Result<T>, logmsg: &str) -> T {
  match result {
    Ok(v) => {
      info!("{logmsg}");
      v
    }
    Err(e) => crash(format!("{logmsg}  ERROR: {e}"), exit_code(&e)),
  }
}

/// Same as [`exec_eval`]; kept as an alias so existing call sites read naturally.
pub fn exec_eval_result<T>(result: io::Result<T>, logmsg: &str) -> T {
  exec_eval(result, logmsg)
}

/// Same as [`exec_eval`] for file operations.
pub fn files_eval(result: io::Result<()>, logmsg: &str) {
  exec_eval(result, logmsg);
}
