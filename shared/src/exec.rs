use crate::args::{ExecMode, OnFail};
use crate::strings::fmt_cmdline;
use std::ffi::OsStr;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Command, ExitStatus, Output, Stdio};

/// Run a command and return `Ok(())` on success.
///
/// `args` accepts anything iterable over string-like items, so call sites can
/// pass `["-s", dev, "--", "mklabel", "gpt"]` without allocating a `String`
/// per argument.
pub fn exec<I, S>(mode: ExecMode<'_>, command: &str, args: I, on_fail: OnFail) -> io::Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    exec_unit(mode, command, args, on_fail).map(|_| ())
}

/// Same as [`exec`] but returns the [`ExitStatus`].
///
/// stdout is streamed line-by-line into the log at `info` level, stderr is
/// captured and attached to the error message on failure.
pub fn exec_unit<I, S>(
    mode: ExecMode<'_>,
    command: &str,
    args: I,
    on_fail: OnFail,
) -> io::Result<ExitStatus>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    exec_with_stdin(mode, command, args, on_fail, None, false)
}

/// Run a command that must not have its arguments logged (password hashes,
/// key material). Arguments are replaced with `<redacted>` in error messages.
/// Optional `stdin` content is written to the child's stdin and never logged.
pub fn exec_secret<I, S>(
    mode: ExecMode<'_>,
    command: &str,
    args: I,
    stdin: Option<&[u8]>,
    on_fail: OnFail,
) -> io::Result<ExitStatus>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    exec_with_stdin(mode, command, args, on_fail, stdin, true)
}

fn exec_with_stdin<I, S>(
    mode: ExecMode<'_>,
    command: &str,
    args: I,
    on_fail: OnFail,
    stdin_data: Option<&[u8]>,
    redact: bool,
) -> io::Result<ExitStatus>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let (program, mut full_args): (&str, Vec<String>) = match mode {
        ExecMode::Direct => (command, Vec::new()),
        ExecMode::Chroot { root } => ("arch-chroot", vec![root.to_string(), command.to_string()]),
    };
    full_args.extend(args.into_iter().map(|a| a.as_ref().to_string_lossy().into_owned()));

    let mut child = Command::new(program)
        .args(&full_args)
        .stdin(if stdin_data.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    if let Some(data) = stdin_data {
        // Scope the handle so stdin is closed before we wait.
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin.write_all(data)?;
        stdin.write_all(b"\n")?;
    }

    // --- log stdout line-by-line via info!() ---
    let stdout = child.stdout.take().expect("piped stdout");
    let stdout_handle = std::thread::spawn(move || -> io::Result<()> {
        let mut reader = BufReader::new(stdout);
        let mut line = Vec::<u8>::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            let text = String::from_utf8_lossy(&line);
            log::info!("{}", text.trim_end_matches(['\r', '\n']));
        }
        Ok(())
    });

    // --- capture stderr fully ---
    let mut stderr = child.stderr.take().expect("piped stderr");
    let stderr_handle = std::thread::spawn(move || -> io::Result<Vec<u8>> {
        let mut v = Vec::new();
        stderr.read_to_end(&mut v)?;
        Ok(v)
    });

    let status = child.wait()?;
    stdout_handle.join().expect("stdout reader panicked")?;
    let stderr_buf = stderr_handle.join().expect("stderr reader panicked")?;

    if status.success() {
        return Ok(status);
    }

    let err_text = String::from_utf8_lossy(&stderr_buf).trim().to_string();
    let cmdline = if redact {
        // Only the program (and chroot wrapper) survive in the log.
        let shown = full_args.iter().take(if program == "arch-chroot" { 2 } else { 0 });
        let mut s = fmt_cmdline(program, &shown.cloned().collect::<Vec<_>>());
        s.push_str(" <redacted>");
        s
    } else {
        fmt_cmdline(program, &full_args)
    };

    let msg = if err_text.is_empty() {
        format!("'{cmdline}' command exited with {status}")
    } else {
        format!("'{cmdline}' command failed. {err_text}")
    };

    match on_fail {
        OnFail::Error => Err(io::Error::other(msg)),
        OnFail::Continue => {
            log::warn!("{msg}");
            Ok(status)
        }
    }
}

/// Run a command and return its captured [`Output`]. Non-zero exit is an error
/// carrying stderr.
pub fn exec_output<I, S>(command: &str, args: I) -> io::Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new(command).args(args).output()?;
    if !output.status.success() {
        return Err(io::Error::other(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    Ok(output)
}

/// Run a command in `workdir`.
pub fn exec_workdir<I, S>(command: &str, workdir: &str, args: I) -> io::Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<String> = args
        .into_iter()
        .map(|a| a.as_ref().to_string_lossy().into_owned())
        .collect();
    let out = Command::new(command)
        .args(&args)
        .current_dir(workdir)
        .stdin(Stdio::null())
        .output()?;

    if out.status.success() {
        return Ok(());
    }

    let err_text = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let cmdline = fmt_cmdline(command, &args);
    let msg = if err_text.is_empty() {
        format!("(In {workdir}) '{cmdline}' command exited with {}", out.status)
    } else {
        format!("(In {workdir}) '{cmdline}' command failed. {err_text}")
    };
    Err(io::Error::other(msg))
}

/// Returns true when the effective UID is 0. No process spawn needed.
pub fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// Exit with a message if not running as root.
pub fn check_if_root() {
    if !is_root() {
        eprintln!("You must be root to perform this operation.");
        std::process::exit(1);
    }
}
