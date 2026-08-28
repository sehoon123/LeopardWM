//! Windows auto-start integration via `HKCU\...\Run` registry key.
//!
//! When the value `LeopardWM` is present under
//! `Software\Microsoft\Windows\CurrentVersion\Run`, Windows launches the
//! daemon at user sign-in. Removing the value disables auto-start.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use winreg::enums::*;
use winreg::RegKey;

const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const REG_VALUE: &str = "LeopardWM";

fn open_run_key() -> Result<RegKey> {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN_SUBKEY, KEY_READ | KEY_WRITE)
        .context("Failed to open Registry Run key")
}

/// Returns `true` if the auto-start registry value is present.
pub fn get_autostart() -> Result<bool> {
    let run_key = open_run_key()?;
    match run_key.get_value::<String, _>(REG_VALUE) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).context("Failed to read Registry value"),
    }
}

/// Resolve the supervised login target for a daemon executable. Installed
/// packages place the watchdog beside the daemon; partial development builds
/// safely fall back to the supplied daemon path.
pub fn preferred_autostart_executable(daemon_path: &Path) -> PathBuf {
    let Some(directory) = daemon_path.parent() else {
        return daemon_path.to_path_buf();
    };
    let watchdog = directory.join("leopardwm-watchdog.exe");
    if daemon_path.is_file() && watchdog.is_file() {
        watchdog
    } else {
        daemon_path.to_path_buf()
    }
}

/// Enable auto-start by writing the quoted `exe_path` to the Run key.
///
/// Refuses to enable when `exe_path` is inside the system temp directory —
/// pointing the registry at a transient binary would silently break after
/// the next reboot or rebuild. Callers can detect this via
/// [`is_in_temp_dir`] and surface a helpful message.
pub fn enable_autostart(exe_path: &Path) -> Result<()> {
    if is_in_temp_dir(exe_path) {
        return Err(anyhow!(
            "Refusing to enable auto-start for a binary in TEMP ({}); install LeopardWM to a stable location first",
            exe_path.display()
        ));
    }
    let run_key = open_run_key()?;
    let quoted = format!("\"{}\"", exe_path.display());
    run_key
        .set_value(REG_VALUE, &quoted)
        .context("Failed to set Registry value")
}

/// Disable auto-start by removing the Run-key value. A missing value is
/// treated as success.
pub fn disable_autostart() -> Result<()> {
    let run_key = open_run_key()?;
    match run_key.delete_value(REG_VALUE) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("Failed to remove Registry value"),
    }
}

/// Returns `true` if `path` is under the system temp directory.
pub fn is_in_temp_dir(path: &Path) -> bool {
    let temp = std::env::temp_dir();
    path.starts_with(&temp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autostart_prefers_sibling_watchdog_and_falls_back() {
        let root = std::env::temp_dir().join(format!(
            "leopardwm-autostart-resolver-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let daemon = root.join("leopardwm.exe");
        let watchdog = root.join("leopardwm-watchdog.exe");
        std::fs::write(&daemon, b"daemon").unwrap();
        assert_eq!(preferred_autostart_executable(&daemon), daemon);
        std::fs::write(&watchdog, b"watchdog").unwrap();
        assert_eq!(preferred_autostart_executable(&daemon), watchdog);
        let _ = std::fs::remove_dir_all(root);
    }
}
