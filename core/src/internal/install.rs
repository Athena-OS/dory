use crate::internal::live::LiveScreen;
use log::{debug, error, info};
use regex::Regex;
use shared::args::PackageManager;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

const MAX_RETRIES: u32 = 15;

/// Environment variable set by the TUI: path of the file that receives the
/// live (still-changing) progress rows, rewritten atomically.
pub const LIVE_FILE_ENV: &str = "DORY_LIVE_FILE";
/// Environment variable set by the TUI: width of the log pane, used as the
/// pty width so pacman sizes its progress bars to fit.
pub const PTY_COLS_ENV: &str = "DORY_PTY_COLS";
/// Minimum interval between rewrites of the live file.
const LIVE_REFRESH: Duration = Duration::from_millis(100);

/// Run the package manager, streaming its output to the log.
///
/// For pacstrap/pacman, mirror and keyring failures are detected in the
/// output, the offending mirror is demoted in the relevant mirrorlist and the
/// command is retried (up to `MAX_RETRIES` times). Returns the last exit code.
pub fn install(pkgmanager: PackageManager, pkgs: &[String]) -> i32 {
  let mut last_exit = 0;

  for attempt in 1..=MAX_RETRIES {
    let (name, mut child, master) = match spawn(pkgmanager, pkgs) {
      Ok(v) => v,
      Err(e) => {
        error!("Failed to start package manager: {e}");
        return 1;
      }
    };

    // Everything the package manager prints comes through the pty master.
    // Finished lines go to the log; lines that are still being redrawn
    // (progress bars) go to the terminal or to the live file.
    let mut retry = false;
    let mut output = PtyOutput::new();
    let mut buf = [0u8; 8192];
    let mut master: File = master.into();
    loop {
      let n = match master.read(&mut buf) {
        Ok(0) => break,
        Ok(n) => n,
        // EIO is how a pty reports that the child closed its side.
        Err(e) if e.raw_os_error() == Some(libc::EIO) => break,
        Err(e) => {
          error!("read error from package manager pty: {e}");
          break;
        }
      };
      for line in output.feed(&buf[..n]) {
        info!("{line}");
        if let Some(pm) = name
          && handle_mirror_problem(&line, pm)
        {
          // Do NOT break: keep draining so the child can exit.
          retry = true;
        }
      }
    }
    for line in output.finish() {
      info!("{line}");
      if let Some(pm) = name
        && handle_mirror_problem(&line, pm)
      {
        retry = true;
      }
    }

    let status = child.wait().expect("Failed to wait on package manager");
    last_exit = status.code().unwrap_or(-1);
    if !status.success() {
      error!("The package manager failed with exit code: {last_exit}");
    }

    if !retry {
      break;
    }
    info!("Retrying package installation (attempt {attempt}/{MAX_RETRIES})");
  }

  last_exit
}

/// Spawn the package manager under a pseudo-terminal so it behaves as on a
/// console: line-flushed output and real progress bars. Returns the name used
/// for mirrorlist handling (`Some` only for the Arch tools), the child and the
/// pty master to read from.
fn spawn(
  pkgmanager: PackageManager,
  pkgs: &[String],
) -> io::Result<(Option<&'static str>, Child, OwnedFd)> {
  let mut cmd = match pkgmanager {
    PackageManager::Pacstrap => {
      let mut c = Command::new("pacstrap");
      c.arg("/mnt").args(pkgs);
      c
    }
    PackageManager::Pacman => {
      let mut c = Command::new("arch-chroot");
      c.args(["/mnt", "pacman", "-Syyu", "--needed", "--noconfirm"])
        .args(pkgs);
      c
    }
    PackageManager::Nix => {
      let mut c = Command::new("nix-shell");
      c.args([
        "-p",
        "nixos-install-tools",
        "--run",
        "nixos-install --no-root-password --keep-going",
      ]);
      c
    }
  };
  let name = match pkgmanager {
    PackageManager::Pacstrap => Some("pacstrap"),
    PackageManager::Pacman => Some("pacman"),
    PackageManager::Nix => None,
  };
  debug!("Spawning {cmd:?}");

  let cols: u16 = std::env::var(PTY_COLS_ENV)
    .ok()
    .and_then(|c| c.parse().ok())
    .unwrap_or(100);
  let (master, slave) = openpty(cols.max(40), 50)?;
  // stdin stays /dev/null: an unexpected prompt must fail, not wait forever.
  cmd
    .stdin(Stdio::null())
    .stdout(Stdio::from(slave.try_clone()?))
    .stderr(Stdio::from(slave));
  // SAFETY: only async-signal-safe calls between fork and exec.
  unsafe {
    cmd.pre_exec(|| {
      libc::setsid();
      // Make the pty the controlling terminal so the child sees a real tty.
      libc::ioctl(1, libc::TIOCSCTTY as _, 0);
      Ok(())
    });
  }
  let child = cmd.spawn()?;
  Ok((name, child, master))
}

/// Allocate a pseudo-terminal pair with the given size.
fn openpty(cols: u16, rows: u16) -> io::Result<(OwnedFd, OwnedFd)> {
  let mut master = 0;
  let mut slave = 0;
  let ws = libc::winsize {
    ws_row: rows,
    ws_col: cols,
    ws_xpixel: 0,
    ws_ypixel: 0,
  };
  // SAFETY: valid out-pointers; openpty fills them on success.
  let rc = unsafe {
    libc::openpty(
      &mut master,
      &mut slave,
      std::ptr::null_mut(),
      std::ptr::null_mut(),
      &ws,
    )
  };
  if rc != 0 {
    return Err(io::Error::last_os_error());
  }
  // SAFETY: both descriptors were just returned by openpty and are owned here.
  Ok(unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) })
}

/// Routes pty output: finished lines come back to the caller for logging,
/// the still-changing part is either passed through to the user's terminal
/// (plain CLI run) or written to the live file for the TUI to display.
struct PtyOutput {
  screen: LiveScreen,
  live_file: Option<String>,
  last_live_write: Instant,
  passthrough: bool,
}

impl PtyOutput {
  fn new() -> Self {
    let live_file = std::env::var(LIVE_FILE_ENV).ok().filter(|p| !p.is_empty());
    Self {
      screen: LiveScreen::new(),
      passthrough: live_file.is_none() && io::stdout().is_terminal(),
      live_file,
      last_live_write: Instant::now() - LIVE_REFRESH,
    }
  }

  fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
    if self.passthrough {
      let mut out = io::stdout().lock();
      let _ = out.write_all(bytes);
      let _ = out.flush();
    }
    let finished = self.screen.feed(bytes);
    if self.live_file.is_some() && self.last_live_write.elapsed() >= LIVE_REFRESH {
      self.write_live();
    }
    finished
  }

  fn finish(&mut self) -> Vec<String> {
    let finished = self.screen.finish();
    if let Some(path) = &self.live_file {
      let _ = fs::remove_file(path);
    }
    finished
  }

  /// Atomically replace the live file with the current live rows.
  fn write_live(&mut self) {
    let Some(path) = &self.live_file else { return };
    let tmp = format!("{path}.tmp");
    let content = self.screen.live_rows().join("\n");
    if fs::write(&tmp, content)
      .and_then(|()| fs::rename(&tmp, path))
      .is_ok()
    {
      self.last_live_write = Instant::now();
    }
  }
}

/// Inspect one line of pacman output; demote the responsible mirror if it
/// reports a download, corruption or key failure. Returns true if the install
/// should be retried.
fn handle_mirror_problem(line: &str, pm_name: &str) -> bool {
  if line.contains("failed retrieving file") && line.contains("from") {
    let Some(mirror_name) = extract_mirror_name(line) else {
      return false;
    };
    let Some(mirrorlist_file) = find_mirrorlist_file(&mirror_name, pm_name) else {
      return false;
    };
    match move_server_line(mirrorlist_file, &mirror_name) {
      Ok(()) => {
        info!("Detected unstable mirror: {mirror_name}. Will retry with a new one...");
        true
      }
      Err(err) => {
        error!("Failed to move 'Server' line in {mirrorlist_file}: {err}");
        false
      }
    }
  } else if (line.contains("File") && line.contains("is corrupted")) || line.contains("invalid key")
  {
    let package_name = extract_package_name(line);
    let repository = get_repository_name(&package_name);
    let mirrorlist_filename = match (pm_name, repository.as_str()) {
      (_, "chaotic-aur") => "/etc/pacman.d/chaotic-mirrorlist",
      ("pacstrap", _) => "/etc/pacman.d/mirrorlist",
      _ => "/mnt/etc/pacman.d/mirrorlist",
    };
    match get_first_mirror_name(mirrorlist_filename)
      .and_then(|m| move_server_line(mirrorlist_filename, &m).map(|_| m))
    {
      Ok(mirror_name) => {
        info!("Detected issue on mirror: {mirror_name}. Will retry with a new one...");
        true
      }
      Err(err) => {
        error!("Could not rotate mirror in {mirrorlist_filename}: {err}");
        false
      }
    }
  } else {
    false
  }
}

/// The word following "from" in a pacman download error.
fn extract_mirror_name(error_message: &str) -> Option<String> {
  let mut words = error_message.split_whitespace();
  words.find(|w| *w == "from")?;
  words.next().map(str::to_string)
}

fn find_mirrorlist_file(mirror_name: &str, pkgmanager_name: &str) -> Option<&'static str> {
  let paths: &[&'static str] = match pkgmanager_name {
    "pacstrap" => &[
      "/etc/pacman.d/mirrorlist",
      "/etc/pacman.d/chaotic-mirrorlist",
    ],
    "pacman" => &[
      "/mnt/etc/pacman.d/mirrorlist",
      "/mnt/etc/pacman.d/chaotic-mirrorlist",
    ],
    _ => &[],
  };
  paths.iter().copied().find(|p| {
    fs::read_to_string(p)
      .map(|c| c.contains(mirror_name))
      .unwrap_or(false)
  })
}

/// Move the line mentioning `mirror_name` after the last `Server` line, so
/// pacman tries it last next time.
fn move_server_line(mirrorlist_path: &str, mirror_name: &str) -> io::Result<()> {
  let mut lines: Vec<String> = BufReader::new(File::open(mirrorlist_path)?)
    .lines()
    .collect::<io::Result<_>>()?;

  let Some(mirror_idx) = lines.iter().position(|line| line.contains(mirror_name)) else {
    return Ok(());
  };
  let mirror_line = lines.remove(mirror_idx);
  let insert_at = lines
    .iter()
    .rposition(|line| line.trim().starts_with("Server"))
    .map(|i| i + 1)
    .unwrap_or(lines.len());
  lines.insert(insert_at, mirror_line.clone());

  let mut file = OpenOptions::new()
    .write(true)
    .truncate(true)
    .open(mirrorlist_path)?;
  for line in &lines {
    writeln!(file, "{line}")?;
  }
  info!("'{mirror_line}' moved to the end of {mirrorlist_path}");
  Ok(())
}

fn get_first_mirror_name(filename: &str) -> io::Result<String> {
  for line in BufReader::new(File::open(filename)?).lines() {
    let line = line?;
    if let Some((key, url)) = line.split_once('=')
      && key.trim() == "Server"
    {
      return Ok(url.trim().to_string());
    }
  }
  Err(io::Error::new(io::ErrorKind::NotFound, "Mirror not found"))
}

/// Package name from either `/pkg/<name>-<ver>.pkg.tar` or `error: <name>: invalid key`.
fn extract_package_name(line: &str) -> String {
  static RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:/pkg/|error:\s)([a-zA-Z0-9\-_]+)").expect("regex"));
  let Some(captures) = RE.captures(line) else {
    return String::new();
  };
  let package_with_version = &captures[1];
  package_with_version
    .rsplit_once('-')
    .map(|(name, _version)| name.to_string())
    .unwrap_or_else(|| package_with_version.to_string())
}

fn get_repository_name(package_name: &str) -> String {
  let output = Command::new("pacman").arg("-Si").arg(package_name).output();
  match output {
    Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
      .lines()
      .find(|line| line.starts_with("Repository"))
      .and_then(|line| line.split(':').nth(1))
      .map(|r| r.trim().to_string())
      .unwrap_or_default(),
    Ok(_) => {
      error!("Package {package_name} not found");
      String::new()
    }
    Err(e) => {
      error!("Failed to execute pacman -Si: {e}");
      String::new()
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn mirror_name_extraction() {
    let line = "error: failed retrieving file 'core.db' from mirror.example.org : The requested URL returned error: 404";
    assert_eq!(
      extract_mirror_name(line).as_deref(),
      Some("mirror.example.org")
    );
    assert!(extract_mirror_name("nothing here").is_none());
  }

  #[test]
  fn package_name_extraction() {
    assert_eq!(
      extract_package_name("error: foo-bar: invalid key found"),
      "foo"
    );
    assert_eq!(
      extract_package_name("/var/cache/pacman/pkg/linux-6.1.0-1.pkg.tar"),
      "linux"
    );
  }
}
