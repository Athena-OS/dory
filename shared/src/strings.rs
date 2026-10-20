use log::error;
use std::process::exit;

fn fmt_arg(a: &str) -> String {
  let needs_quotes = a
    .chars()
    .any(|c| c.is_whitespace() || "\"'\\$`!(){}[]<>|&;*?".contains(c));
  if !needs_quotes {
    return a.to_string();
  }
  format!("'{}'", a.replace('\'', r"'\''"))
}

/// Render a command line the way a shell would need it quoted (for logs).
pub fn fmt_cmdline(cmd: &str, args: &[String]) -> String {
  let mut s = fmt_arg(cmd);
  for a in args {
    s.push(' ');
    s.push_str(&fmt_arg(a));
  }
  s
}

/// Log an error and terminate the process with `code`.
pub fn crash<S: AsRef<str>>(a: S, code: i32) -> ! {
  error!("{}", a.as_ref());
  exit(code);
}
