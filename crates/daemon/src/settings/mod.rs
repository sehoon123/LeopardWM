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
use std::sync::{mpsc, Mutex, OnceLock};

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

/// Timed-out settings threads remain owned here rather than being detached.
/// Their `SettingsOpenGuard` stays live until their COM/WebView teardown
/// genuinely finishes, so a replacement window cannot reuse global state.
static PENDING_SETTINGS_THREADS: OnceLock<Mutex<Vec<std::thread::JoinHandle<()>>>> =
    OnceLock::new();

fn pending_settings_threads() -> &'static Mutex<Vec<std::thread::JoinHandle<()>>> {
    PENDING_SETTINGS_THREADS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Join any retained settings threads that have completed. Daemon shutdown
/// should call this once after dropping its active settings handle so retained
/// COM/WebView ownership is recovered before process teardown when possible.
pub(crate) fn reap_finished_settings_threads() {
    let mut pending = pending_settings_threads()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut still_running = Vec::with_capacity(pending.len());
    for thread in pending.drain(..) {
        if thread.is_finished() {
            if thread.join().is_err() {
                warn!("Retained settings window thread panicked during shutdown");
            }
        } else {
            still_running.push(thread);
        }
    }
    *pending = still_running;
}

fn retain_settings_thread(thread: std::thread::JoinHandle<()>) {
    let mut pending = pending_settings_threads()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    pending.push(thread);
}

/// Resets the singleton flag even if the settings thread unwinds unexpectedly.
struct SettingsOpenGuard;

impl Drop for SettingsOpenGuard {
    fn drop(&mut self) {
        SETTINGS_OPEN.store(false, Ordering::SeqCst);
    }
}

/// Handle to the settings window thread.
pub struct SettingsWindowHandle {
    thread: Option<std::thread::JoinHandle<()>>,
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
        reap_finished_settings_threads();
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

        Some(SettingsWindowHandle {
            thread: Some(handle),
        })
    }
}

impl Drop for SettingsWindowHandle {
    fn drop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        // The HWND/thread queue appears after WebView construction. Allow a
        // short startup race, then ask the owning message loop to quit so COM
        // and the parent HWND are torn down on their creation thread.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !thread.is_finished() && std::time::Instant::now() < deadline {
            if win32::request_close() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let finish_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !thread.is_finished() && std::time::Instant::now() < finish_deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if thread.is_finished() {
            if thread.join().is_err() {
                warn!("Settings window thread panicked during shutdown");
            }
        } else {
            // Do not drop the JoinHandle: doing so detaches a thread that still
            // owns WebView2, COM, the HWND, and the singleton guard. Retaining
            // it also fences reopening until the original owner has exited.
            warn!("Settings window thread exceeded shutdown budget; retaining join ownership");
            retain_settings_thread(thread);
        }
        reap_finished_settings_threads();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unfinished_settings_thread_is_retained_until_it_can_be_joined() {
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            release_rx.recv().unwrap();
        });

        assert!(!thread.is_finished());
        retain_settings_thread(thread);
        assert_eq!(pending_settings_threads().lock().unwrap().len(), 1);

        release_tx.send(()).unwrap();
        for _ in 0..100 {
            reap_finished_settings_threads();
            if pending_settings_threads().lock().unwrap().is_empty() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("retained settings thread was not joined after completion");
    }
}
