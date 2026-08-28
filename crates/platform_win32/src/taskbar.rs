//! Hide/show taskbar buttons for managed windows via `ITaskbarList`.
//!
//! DWM cloaking can't hide an external window's taskbar button (it returns
//! `E_ACCESSDENIED` on another process's window) and off-screen positioning
//! doesn't reliably drop the button either, so `ITaskbarList::DeleteTab` /
//! `AddTab` is the only mechanism that works. The interface is apartment-model
//! COM, so a dedicated STA thread owns it and processes requests over a channel.
//!
//! Callable from anywhere via [`taskbar_hide`] / [`taskbar_show`] (a global
//! sender, like the cloak helpers). Every successful `DeleteTab` stays in the
//! worker's ledger until a successful `AddTab` acknowledges its restoration.

use crate::recover_poisoned_mutex;
use leopardwm_core_layout::WindowId;
use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::{mpsc, Mutex};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::{ITaskbarList, TaskbarList};

enum TaskbarCmd {
    Hide(WindowId),
    Show(WindowId),
    Restore(WindowId),
    Forget(WindowId),
    Shutdown(mpsc::Sender<ShutdownAck>),
}

#[derive(Debug, PartialEq, Eq)]
struct ShutdownAck {
    /// IDs for which shutdown attempted, but did not receive, an `AddTab`
    /// acknowledgement. They are deliberately reported rather than silently
    /// treated as restored.
    unrestored: Vec<WindowId>,
}

/// Global sender to the taskbar thread. `None` until `init_taskbar`, and again
/// after the handle is dropped. Free functions no-op when unset.
static TASKBAR_TX: Mutex<Option<mpsc::Sender<TaskbarCmd>>> = Mutex::new(None);

fn send_taskbar_command(command: TaskbarCmd) {
    let tx = TASKBAR_TX
        .lock()
        .unwrap_or_else(recover_poisoned_mutex)
        .as_ref()
        .cloned();
    if let Some(tx) = tx {
        if tx.send(command).is_err() {
            tracing::warn!("Taskbar controller is unavailable");
        }
    }
}

/// Remove `wid`'s taskbar button (best-effort; no-op if uninitialized).
pub fn taskbar_hide(wid: WindowId) {
    send_taskbar_command(TaskbarCmd::Hide(wid));
}

/// Restore `wid`'s taskbar button (only acts if we'd hidden it).
pub fn taskbar_show(wid: WindowId) {
    send_taskbar_command(TaskbarCmd::Show(wid));
}

/// Unconditionally re-add `wid`'s taskbar button. Used at startup to restore
/// buttons a crashed prior daemon instance may have left deleted (those aren't
/// in this process's hidden set, so `taskbar_show` wouldn't touch them).
pub fn taskbar_restore(wid: WindowId) {
    send_taskbar_command(TaskbarCmd::Restore(wid));
}

/// Drop `wid` from the hidden set without an `AddTab` (the window is gone).
/// Keeps the set from retaining a stale id whose HWND could be recycled, which
/// would otherwise make the change-gate skip re-hiding the new window.
pub fn taskbar_forget(wid: WindowId) {
    send_taskbar_command(TaskbarCmd::Forget(wid));
}

/// Owns the taskbar-control thread. Dropping it waits for the worker's explicit
/// shutdown acknowledgement after all queued work and restoration attempts.
pub struct TaskbarHandle {
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Spawn the COM thread and publish the global sender only after COM and
/// `ITaskbarList` initialization succeed. Returns `None` if initialization
/// fails; taskbar hiding is then a best-effort no-op.
pub fn init_taskbar() -> Option<TaskbarHandle> {
    if TASKBAR_TX
        .lock()
        .unwrap_or_else(recover_poisoned_mutex)
        .is_some()
    {
        tracing::warn!("Taskbar controller already initialized");
        return None;
    }

    let (tx, rx) = mpsc::channel::<TaskbarCmd>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
    let thread = std::thread::Builder::new()
        .name("taskbar-list".into())
        .spawn(move || run(rx, ready_tx))
        .map_err(|e| tracing::warn!("Failed to spawn taskbar thread: {}", e))
        .ok()?;

    match ready_rx.recv() {
        Ok(Ok(())) => {
            let mut slot = TASKBAR_TX.lock().unwrap_or_else(recover_poisoned_mutex);
            if slot.is_some() {
                // Another initializer won the race while this thread was
                // starting. Ask ours to stop cleanly before declining it.
                drop(slot);
                let (ack_tx, ack_rx) = mpsc::channel();
                let _ = tx.send(TaskbarCmd::Shutdown(ack_tx));
                let _ = ack_rx.recv();
                let _ = thread.join();
                tracing::warn!("Taskbar controller initialized concurrently");
                None
            } else {
                *slot = Some(tx);
                Some(TaskbarHandle {
                    thread: Some(thread),
                })
            }
        }
        Ok(Err(error)) => {
            tracing::warn!("Taskbar controller initialization failed: {error}");
            let _ = thread.join();
            None
        }
        Err(_) => {
            tracing::warn!("Taskbar controller exited before initialization completed");
            let _ = thread.join();
            None
        }
    }
}

impl Drop for TaskbarHandle {
    fn drop(&mut self) {
        // Remove public access first; the explicit command is ordered after all
        // earlier sends on this sender and gives the worker a completion edge.
        let tx = TASKBAR_TX
            .lock()
            .unwrap_or_else(recover_poisoned_mutex)
            .take();

        if let Some(tx) = tx {
            let (ack_tx, ack_rx) = mpsc::channel();
            match tx.send(TaskbarCmd::Shutdown(ack_tx)) {
                Ok(()) => match ack_rx.recv() {
                    Ok(ack) if ack.unrestored.is_empty() => {}
                    Ok(ack) => tracing::error!(
                        windows = ?ack.unrestored,
                        "Taskbar shutdown completed with unrestored tabs"
                    ),
                    Err(_) => tracing::error!(
                        "Taskbar controller exited without a shutdown acknowledgement"
                    ),
                },
                Err(_) => tracing::error!("Taskbar controller stopped before shutdown command"),
            }
        }

        if let Some(thread) = self.thread.take() {
            // An acknowledgement means the STA processed every queued command
            // and every restore attempt. Join instead of detaching so Drop
            // never falsely claims this owner has completed while it is live.
            let _ = thread.join();
        }
    }
}

fn hwnd_of(wid: WindowId) -> HWND {
    HWND(wid as *mut c_void)
}

/// Narrow adapter seam: all ledger transitions are unit-testable without COM.
trait TaskbarAdapter {
    fn delete_tab(&mut self, wid: WindowId) -> Result<(), String>;
    fn add_tab(&mut self, wid: WindowId) -> Result<(), String>;
}

struct ComTaskbarAdapter {
    taskbar: ITaskbarList,
}

impl TaskbarAdapter for ComTaskbarAdapter {
    fn delete_tab(&mut self, wid: WindowId) -> Result<(), String> {
        unsafe { self.taskbar.DeleteTab(hwnd_of(wid)) }.map_err(|error| error.to_string())
    }

    fn add_tab(&mut self, wid: WindowId) -> Result<(), String> {
        unsafe { self.taskbar.AddTab(hwnd_of(wid)) }.map_err(|error| error.to_string())
    }
}

fn hide_tab<A: TaskbarAdapter>(adapter: &mut A, hidden: &mut HashSet<WindowId>, wid: WindowId) {
    if hidden.contains(&wid) {
        return;
    }
    match adapter.delete_tab(wid) {
        Ok(()) => {
            hidden.insert(wid);
        }
        Err(error) => tracing::warn!("ITaskbarList::DeleteTab({wid}) failed: {error}"),
    }
}

fn show_tab<A: TaskbarAdapter>(adapter: &mut A, hidden: &mut HashSet<WindowId>, wid: WindowId) {
    if !hidden.contains(&wid) {
        return;
    }
    match adapter.add_tab(wid) {
        Ok(()) => {
            hidden.remove(&wid);
        }
        Err(error) => tracing::warn!("ITaskbarList::AddTab({wid}) failed: {error}"),
    }
}

fn restore_tab<A: TaskbarAdapter>(adapter: &mut A, hidden: &mut HashSet<WindowId>, wid: WindowId) {
    match adapter.add_tab(wid) {
        Ok(()) => {
            // Startup restore is unconditional, but it may also satisfy an
            // in-process receipt. Retire that receipt only after COM success.
            hidden.remove(&wid);
        }
        Err(error) => tracing::warn!("ITaskbarList::AddTab({wid}) failed: {error}"),
    }
}

fn restore_hidden_tabs<A: TaskbarAdapter>(
    adapter: &mut A,
    hidden: &mut HashSet<WindowId>,
) -> Vec<WindowId> {
    let pending: Vec<WindowId> = hidden.iter().copied().collect();
    for wid in pending {
        show_tab(adapter, hidden, wid);
    }
    hidden.iter().copied().collect()
}

fn run_commands<A: TaskbarAdapter>(rx: mpsc::Receiver<TaskbarCmd>, adapter: &mut A) {
    let mut hidden = HashSet::new();
    loop {
        match rx.recv() {
            Ok(TaskbarCmd::Hide(wid)) => hide_tab(adapter, &mut hidden, wid),
            Ok(TaskbarCmd::Show(wid)) => show_tab(adapter, &mut hidden, wid),
            Ok(TaskbarCmd::Restore(wid)) => restore_tab(adapter, &mut hidden, wid),
            Ok(TaskbarCmd::Forget(wid)) => {
                hidden.remove(&wid);
            }
            Ok(TaskbarCmd::Shutdown(ack_tx)) => {
                let unrestored = restore_hidden_tabs(adapter, &mut hidden);
                let _ = ack_tx.send(ShutdownAck { unrestored });
                break;
            }
            Err(_) => {
                let unrestored = restore_hidden_tabs(adapter, &mut hidden);
                if !unrestored.is_empty() {
                    tracing::error!(
                        windows = ?unrestored,
                        "Taskbar channel closed with unrestored tabs"
                    );
                }
                break;
            }
        }
    }
}

fn run(rx: mpsc::Receiver<TaskbarCmd>, ready_tx: mpsc::Sender<Result<(), String>>) {
    unsafe {
        let coinit = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        if coinit.is_err() {
            let _ = ready_tx.send(Err(format!("CoInitializeEx failed: {coinit:?}")));
            return;
        }

        let taskbar: ITaskbarList = match CoCreateInstance(&TaskbarList, None, CLSCTX_ALL) {
            Ok(taskbar) => taskbar,
            Err(error) => {
                let _ = ready_tx.send(Err(format!(
                    "CoCreateInstance(TaskbarList) failed: {error}"
                )));
                CoUninitialize();
                return;
            }
        };
        if let Err(error) = taskbar.HrInit() {
            let _ = ready_tx.send(Err(format!("ITaskbarList::HrInit failed: {error}")));
            drop(taskbar);
            CoUninitialize();
            return;
        }

        let _ = ready_tx.send(Ok(()));
        tracing::info!("Taskbar controller initialized");
        let mut adapter = ComTaskbarAdapter { taskbar };
        run_commands(rx, &mut adapter);
        drop(adapter);
        CoUninitialize();
        tracing::debug!("Taskbar controller stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeTaskbar {
        delete_failures: usize,
        add_failures: usize,
        calls: Vec<(&'static str, WindowId)>,
    }

    impl TaskbarAdapter for FakeTaskbar {
        fn delete_tab(&mut self, wid: WindowId) -> Result<(), String> {
            self.calls.push(("delete", wid));
            if self.delete_failures > 0 {
                self.delete_failures -= 1;
                Err("injected DeleteTab failure".into())
            } else {
                Ok(())
            }
        }

        fn add_tab(&mut self, wid: WindowId) -> Result<(), String> {
            self.calls.push(("add", wid));
            if self.add_failures > 0 {
                self.add_failures -= 1;
                Err("injected AddTab failure".into())
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn hide_records_only_successful_delete_tab() {
        let mut adapter = FakeTaskbar {
            delete_failures: 1,
            ..Default::default()
        };
        let mut hidden = HashSet::new();

        hide_tab(&mut adapter, &mut hidden, 7);
        assert!(!hidden.contains(&7));
        hide_tab(&mut adapter, &mut hidden, 7);
        assert!(hidden.contains(&7));
        assert_eq!(adapter.calls, vec![("delete", 7), ("delete", 7)]);
    }

    #[test]
    fn show_retains_receipt_until_add_tab_succeeds() {
        let mut adapter = FakeTaskbar {
            add_failures: 1,
            ..Default::default()
        };
        let mut hidden = HashSet::from([8]);

        show_tab(&mut adapter, &mut hidden, 8);
        assert!(hidden.contains(&8));
        show_tab(&mut adapter, &mut hidden, 8);
        assert!(!hidden.contains(&8));
        assert_eq!(adapter.calls, vec![("add", 8), ("add", 8)]);
    }

    #[test]
    fn shutdown_ack_follows_restore_attempt_and_reports_failure() {
        let mut adapter = FakeTaskbar {
            add_failures: 1,
            ..Default::default()
        };
        let (tx, rx) = mpsc::channel();
        let (ack_tx, ack_rx) = mpsc::channel();
        tx.send(TaskbarCmd::Hide(9)).unwrap();
        tx.send(TaskbarCmd::Shutdown(ack_tx)).unwrap();
        drop(tx);

        run_commands(rx, &mut adapter);

        assert_eq!(
            ack_rx.recv().unwrap(),
            ShutdownAck {
                unrestored: vec![9]
            }
        );
        assert_eq!(adapter.calls, vec![("delete", 9), ("add", 9)]);
    }

    #[test]
    fn shutdown_retries_a_retained_receipt_after_prior_add_failure() {
        let mut adapter = FakeTaskbar {
            add_failures: 1,
            ..Default::default()
        };
        let mut hidden = HashSet::from([10]);

        show_tab(&mut adapter, &mut hidden, 10);
        assert!(hidden.contains(&10));
        assert!(restore_hidden_tabs(&mut adapter, &mut hidden).is_empty());
        assert!(hidden.is_empty());
        assert_eq!(adapter.calls, vec![("add", 10), ("add", 10)]);
    }
}
