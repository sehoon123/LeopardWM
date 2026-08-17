//! Settings window for LeopardWM daemon.
//!
//! Native Win32 settings window using the `windows` crate.
//! Tabbed form with one tab per config section. Runs on a dedicated thread
//! with its own message loop. An `AtomicBool` singleton guard prevents
//! multiple windows.

mod html;
mod win32;

pub use win32::push_failed_binds;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use tracing::{info, warn};

use crate::config::Config;

/// Events emitted by the settings window back to the daemon.
#[derive(Debug, Clone)]
pub enum SettingsEvent {
    /// The user saved the config (already written to disk).
    Saved,
    /// The hotkey recorder started (`true`) or stopped (`false`). While
    /// recording, global hotkeys are suspended so the combo being captured
    /// doesn't also fire its action.
    SetRecording(bool),
    /// The settings window closed. Used as a safety net to resume hotkeys if
    /// the window was closed mid-recording.
    Closed,
}

/// Singleton guard — only one settings window at a time.
static SETTINGS_OPEN: AtomicBool = AtomicBool::new(false);

/// Resets the singleton flag even if the settings thread unwinds unexpectedly.
struct SettingsOpenGuard;

impl Drop for SettingsOpenGuard {
    fn drop(&mut self) {
        SETTINGS_OPEN.store(false, Ordering::SeqCst);
    }
}

/// Handle to the settings window thread.
pub struct SettingsWindowHandle {
    _thread: std::thread::JoinHandle<()>,
}

impl SettingsWindowHandle {
    /// Open the settings window on a dedicated thread.
    ///
    /// `initial_section` optionally navigates to a specific tab (e.g., `"about"`).
    /// Returns `None` if a settings window is already open.
    pub fn open(
        config: Config,
        event_tx: mpsc::Sender<SettingsEvent>,
        initial_section: Option<&str>,
        high_contrast: bool,
        failed_binds: Vec<String>,
    ) -> Option<Self> {
        if SETTINGS_OPEN
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            if win32::focus_existing_window() {
                info!("Settings window already open — focused existing window");
            } else {
                info!("Settings window already open — it is still initializing");
            }
            return None;
        }

        let section = initial_section.map(String::from);
        let handle = match std::thread::Builder::new()
            .name("settings-window".into())
            .spawn(move || {
                let _open_guard = SettingsOpenGuard;
                if let Err(e) = win32::run_settings_window(
                    config,
                    event_tx,
                    section.as_deref(),
                    high_contrast,
                    failed_binds,
                ) {
                    warn!("Settings window error: {}", e);
                }
            }) {
            Ok(handle) => handle,
            Err(error) => {
                SETTINGS_OPEN.store(false, Ordering::SeqCst);
                warn!("Failed to spawn settings window thread: {}", error);
                return None;
            }
        };

        Some(SettingsWindowHandle { _thread: handle })
    }
}
