//! Crash-safe, same-directory file replacement for persisted daemon state.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const REPLACE_RETRIES: usize = 4;
const TEMP_CREATE_RETRIES: usize = 128;

pub(crate) fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> io::Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let (temp, file) = create_unique_sibling(path)?;
    let result = write_then_replace(file, &temp, path, contents.as_ref());
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn write_then_replace(
    mut file: File,
    temp: &Path,
    destination: &Path,
    contents: &[u8],
) -> io::Result<()> {
    file.write_all(contents)?;
    file.flush()?;
    file.sync_all()?;
    drop(file);
    let mut last_error = None;
    for attempt in 0..=REPLACE_RETRIES {
        match replace_file(temp, destination) {
            Ok(()) => return Ok(()),
            Err(error) if attempt < REPLACE_RETRIES && is_transient_replace_error(&error) => {
                last_error = Some(error);
                std::thread::sleep(Duration::from_millis(5_u64 << attempt));
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| io::Error::other("file replacement failed")))
}

fn create_unique_sibling(path: &Path) -> io::Result<(PathBuf, File)> {
    create_unique_sibling_with(path, std::process::id(), &TEMP_SEQUENCE)
}

fn create_unique_sibling_with(
    path: &Path,
    pid: u32,
    sequence: &AtomicU64,
) -> io::Result<(PathBuf, File)> {
    for _ in 0..TEMP_CREATE_RETRIES {
        let candidate = sibling_path(path, pid, sequence.fetch_add(1, Ordering::Relaxed));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique sibling temp file",
    ))
}

fn sibling_path(path: &Path, pid: u32, sequence: u64) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("leopardwm-state");
    path.with_file_name(format!(".{name}.{pid}.{sequence}.tmp"))
}

fn is_transient_replace_error(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(5 | 32 | 33))
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    unsafe {
        MoveFileExW(
            PCWSTR(source.as_ptr()),
            PCWSTR(destination.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
        .map_err(io::Error::from)
    }
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "leopardwm-atomic-file-{name}-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn creates_and_replaces_without_exposing_partial_contents() {
        let dir = test_dir("replace");
        let path = dir.join("state.json");
        write(&path, b"first").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"first");
        write(&path, b"second-state").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second-state");
        assert_eq!(
            fs::read_dir(&dir).unwrap().filter_map(Result::ok).count(),
            1
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn colliding_temp_is_preserved_and_a_new_sibling_is_used() {
        let dir = test_dir("collision");
        fs::create_dir_all(&dir).unwrap();
        let destination = dir.join("state.json");
        let sequence = AtomicU64::new(0);
        let collision = sibling_path(&destination, 42, 0);
        fs::write(&collision, b"other writer").unwrap();

        let (temp, file) = create_unique_sibling_with(&destination, 42, &sequence).unwrap();
        assert_ne!(temp, collision);
        write_then_replace(file, &temp, &destination, b"new state").unwrap();

        assert_eq!(fs::read(&collision).unwrap(), b"other writer");
        assert_eq!(fs::read(&destination).unwrap(), b"new state");
        assert!(!temp.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_destination_does_not_leave_temp_files() {
        let dir = test_dir("cleanup");
        fs::create_dir_all(&dir).unwrap();
        let destination = dir.join("is-a-directory");
        fs::create_dir_all(&destination).unwrap();
        assert!(write(&destination, b"data").is_err());
        assert!(fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .all(|entry| !entry.file_name().to_string_lossy().ends_with(".tmp")));
        fs::remove_dir_all(dir).unwrap();
    }
}
