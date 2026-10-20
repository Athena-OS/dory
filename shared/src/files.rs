use crate::strings::crash;
use glob::glob;
use log::info;
use regex::Regex;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Error, ErrorKind, Write};

pub fn create_file(path: &str) {
    match File::create(path) {
        Ok(_) => info!("Create {path}"),
        Err(e) => crash(format!("Create {path}: Failed with error {e}"), 1),
    }
}

/// Create (or truncate) `path` and write `content` to it in one call.
pub fn write_file(path: &str, content: &str) -> io::Result<()> {
    info!("Write {path}");
    fs::write(path, content)
}

pub fn copy_file(path: &str, destpath: &str) {
    match fs::copy(path, destpath) {
        Ok(_) => info!("Copy {path} to {destpath}"),
        Err(e) => crash(format!("Copy {path} to {destpath}: Failed with error {e}"), 1),
    }
}

pub fn copy_multiple_files(pattern: &str, dest_dir: &str) {
    if let Err(e) = create_directory(dest_dir) {
        crash(format!("Failed to create directory {dest_dir}: {e}"), 1);
    }

    let paths = match glob(pattern) {
        Ok(paths) => paths,
        Err(e) => crash(format!("Invalid glob pattern {pattern}: {e}"), 1),
    };
    for entry in paths {
        match entry {
            Ok(path) if path.is_file() => {
                let file_name = path.file_name().unwrap_or_default().to_string_lossy();
                copy_file(&path.to_string_lossy(), &format!("{dest_dir}/{file_name}"));
            }
            Ok(_) => {}
            Err(e) => crash(format!("Error processing pattern {pattern}: {e}"), 1),
        }
    }
}

pub fn rename_file(path: &str, destpath: &str) {
    match fs::rename(path, destpath) {
        Ok(_) => info!("Rename {path} to {destpath}"),
        Err(e) => crash(format!("Rename {path} to {destpath}: Failed with error {e}"), 1),
    }
}

pub fn remove_file(path: &str) {
    match fs::remove_file(path) {
        Ok(_) => info!("Remove {path}"),
        Err(e) => crash(format!("Remove {path}: Failed with error {e}"), 1),
    }
}

pub fn append_file(path: &str, content: &str) -> io::Result<()> {
    info!("Append '{}' to file {}", content.trim_end(), path);
    let mut file = OpenOptions::new().append(true).create(true).open(path)?;
    file.write_all(content.as_bytes())?;
    file.write_all(b"\n")
}

/// Regex find/replace over the whole file (`replace_all` semantics).
pub fn sed_file(path: &str, find: &str, replace: &str) -> io::Result<()> {
    info!("Sed '{find}' to '{replace}' in file {path}");
    let contents = fs::read_to_string(path)?;
    let regex = Regex::new(find).map_err(|e| Error::new(ErrorKind::InvalidInput, e.to_string()))?;
    let new_contents = regex.replace_all(&contents, replace);
    if new_contents != contents {
        fs::write(path, new_contents.as_bytes())?;
    }
    Ok(())
}

/// Replace every line containing `search` with `replacement`.
/// Deliberately not logging the contents: this is used for password hashes.
pub fn replace_line_in_file(path: &str, search: &str, replacement: &str) -> io::Result<()> {
    let contents = fs::read_to_string(path)?;
    let mut new_contents = String::with_capacity(contents.len());
    for line in contents.lines() {
        new_contents.push_str(if line.contains(search) { replacement } else { line });
        new_contents.push('\n');
    }
    fs::write(path, new_contents)
}

/// Create all missing dirs in the specified path.
pub fn create_directory(path: &str) -> io::Result<()> {
    fs::create_dir_all(path)
}
