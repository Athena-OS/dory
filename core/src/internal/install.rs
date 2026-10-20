use log::{debug, error, info};
use regex::Regex;
use shared::args::PackageManager;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::LazyLock;
use std::sync::mpsc::{self, Sender};
use std::thread;

const MAX_RETRIES: u32 = 15;

/// Run the package manager, streaming its output to the log.
///
/// For pacstrap/pacman, mirror and keyring failures are detected in the
/// output, the offending mirror is demoted in the relevant mirrorlist and the
/// command is retried (up to `MAX_RETRIES` times). Returns the last exit code.
pub fn install(pkgmanager: PackageManager, pkgs: &[String]) -> i32 {
    let mut last_exit = 0;

    for attempt in 1..=MAX_RETRIES {
        let (name, mut child) = match spawn(pkgmanager, pkgs) {
            Ok(v) => v,
            Err(e) => {
                error!("Failed to start package manager: {e}");
                return 1;
            }
        };

        // Both pipes feed a single channel so the mirror-handling below runs on
        // one thread: two reader threads editing the same mirrorlist raced.
        let (tx, rx) = mpsc::channel::<String>();
        let t1 = child.stdout.take().map(|p| spawn_reader(p, tx.clone()));
        let t2 = child.stderr.take().map(|p| spawn_reader(p, tx));
        // (the original `tx` was moved into t2; if stderr was absent it drops here)

        let mut retry = false;
        for line in rx {
            info!("{line}");
            if let Some(pm) = name
                && handle_mirror_problem(&line, pm)
            {
                // Do NOT break: keep draining the pipe so the child can exit.
                retry = true;
            }
        }
        for t in [t1, t2].into_iter().flatten() {
            let _ = t.join();
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

/// Spawn the package manager. Returns the name used for mirrorlist handling
/// (`Some` only for the Arch tools).
fn spawn(pkgmanager: PackageManager, pkgs: &[String]) -> io::Result<(Option<&'static str>, Child)> {
    let mut cmd = match pkgmanager {
        PackageManager::Pacstrap => {
            let mut c = Command::new("pacstrap");
            c.arg("/mnt").args(pkgs);
            c
        }
        PackageManager::Pacman => {
            let mut c = Command::new("arch-chroot");
            c.args(["/mnt", "pacman", "-Syyu", "--needed", "--noconfirm"]).args(pkgs);
            c
        }
        PackageManager::Nix => {
            let mut c = Command::new("nix-shell");
            c.args(["-p", "nixos-install-tools", "--run", "nixos-install --no-root-password --keep-going"]);
            c
        }
    };
    let name = match pkgmanager {
        PackageManager::Pacstrap => Some("pacstrap"),
        PackageManager::Pacman => Some("pacman"),
        PackageManager::Nix => None,
    };
    debug!("Spawning {cmd:?}");
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    Ok((name, child))
}

fn spawn_reader<R: io::Read + Send + 'static>(pipe: R, tx: Sender<String>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut reader = BufReader::new(pipe);
        let mut buf: Vec<u8> = Vec::with_capacity(16 * 1024);
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buf);
                    if tx.send(line.trim_end_matches(['\r', '\n']).to_string()).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    error!("read stream error: {e}");
                    break;
                }
            }
        }
    })
}

/// Inspect one line of pacman output; demote the responsible mirror if it
/// reports a download, corruption or key failure. Returns true if the install
/// should be retried.
fn handle_mirror_problem(line: &str, pm_name: &str) -> bool {
    if line.contains("failed retrieving file") && line.contains("from") {
        let Some(mirror_name) = extract_mirror_name(line) else { return false };
        let Some(mirrorlist_file) = find_mirrorlist_file(&mirror_name, pm_name) else { return false };
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
    } else if (line.contains("File") && line.contains("is corrupted")) || line.contains("invalid key") {
        let package_name = extract_package_name(line);
        let repository = get_repository_name(&package_name);
        let mirrorlist_filename = match (pm_name, repository.as_str()) {
            (_, "chaotic-aur") => "/etc/pacman.d/chaotic-mirrorlist",
            ("pacstrap", _) => "/etc/pacman.d/mirrorlist",
            _ => "/mnt/etc/pacman.d/mirrorlist",
        };
        match get_first_mirror_name(mirrorlist_filename).and_then(|m| move_server_line(mirrorlist_filename, &m).map(|_| m)) {
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
        "pacstrap" => &["/etc/pacman.d/mirrorlist", "/etc/pacman.d/chaotic-mirrorlist"],
        "pacman" => &["/mnt/etc/pacman.d/mirrorlist", "/mnt/etc/pacman.d/chaotic-mirrorlist"],
        _ => &[],
    };
    paths
        .iter()
        .copied()
        .find(|p| fs::read_to_string(p).map(|c| c.contains(mirror_name)).unwrap_or(false))
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

    let mut file = OpenOptions::new().write(true).truncate(true).open(mirrorlist_path)?;
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
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:/pkg/|error:\s)([a-zA-Z0-9\-_]+)").expect("regex"));
    let Some(captures) = RE.captures(line) else { return String::new() };
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
        assert_eq!(extract_mirror_name(line).as_deref(), Some("mirror.example.org"));
        assert!(extract_mirror_name("nothing here").is_none());
    }

    #[test]
    fn package_name_extraction() {
        assert_eq!(extract_package_name("error: foo-bar: invalid key found"), "foo");
        assert_eq!(extract_package_name("/var/cache/pacman/pkg/linux-6.1.0-1.pkg.tar"), "linux");
    }
}
