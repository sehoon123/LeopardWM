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
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{mpsc, Mutex};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::{ITaskbarList, TaskbarList};
use windows::Win32::UI::WindowsAndMessaging::{GetClassNameW, GetWindowThreadProcessId, IsWindow};

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

#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskbarWindowIdentity {
    process_id: u32,
    thread_id: u32,
    class_name: String,
    incarnation_token: u64,
}

fn taskbar_window_identity(wid: WindowId) -> Option<TaskbarWindowIdentity> {
    let hwnd = hwnd_of(wid);
    if !unsafe { IsWindow(Some(hwnd)).as_bool() } {
        return None;
    }
    let mut process_id = 0u32;
    let thread_id = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
    let mut class = [0u16; 256];
    let class_len = unsafe { GetClassNameW(hwnd, &mut class) };
    if process_id == 0 || thread_id == 0 || class_len <= 0 {
        return None;
    }
    Some(TaskbarWindowIdentity {
        process_id,
        thread_id,
        class_name: String::from_utf16_lossy(&class[..class_len as usize]),
        incarnation_token: crate::event_hooks::ensure_window_incarnation_token(wid)?,
    })
}

/// Narrow adapter seam: all ledger transitions are unit-testable without COM.
trait TaskbarAdapter {
    fn identity(&mut self, wid: WindowId) -> Option<TaskbarWindowIdentity>;
    fn delete_tab(&mut self, wid: WindowId) -> Result<(), String>;
    fn add_tab(&mut self, wid: WindowId) -> Result<(), String>;
}

struct ComTaskbarAdapter {
    taskbar: ITaskbarList,
}

impl TaskbarAdapter for ComTaskbarAdapter {
    fn identity(&mut self, wid: WindowId) -> Option<TaskbarWindowIdentity> {
        taskbar_window_identity(wid)
    }

    fn delete_tab(&mut self, wid: WindowId) -> Result<(), String> {
        unsafe { self.taskbar.DeleteTab(hwnd_of(wid)) }.map_err(|error| error.to_string())
    }

    fn add_tab(&mut self, wid: WindowId) -> Result<(), String> {
        unsafe { self.taskbar.AddTab(hwnd_of(wid)) }.map_err(|error| error.to_string())
    }
}

type HiddenTaskbarTabs = HashMap<WindowId, TaskbarWindowIdentity>;

fn hide_tab<A: TaskbarAdapter>(adapter: &mut A, hidden: &mut HiddenTaskbarTabs, wid: WindowId) {
    if hidden.contains_key(&wid) {
        return;
    }
    let Some(identity) = adapter.identity(wid) else {
        tracing::debug!("Skipping taskbar hide for unavailable window {wid}");
        return;
    };
    match adapter.delete_tab(wid) {
        Ok(()) if adapter.identity(wid).as_ref() == Some(&identity) => {
            hidden.insert(wid, identity);
        }
        Ok(()) => tracing::warn!(
            "Window {wid} changed incarnation during DeleteTab; refusing a stale receipt"
        ),
        Err(error) => tracing::warn!("ITaskbarList::DeleteTab({wid}) failed: {error}"),
    }
}

fn show_tab<A: TaskbarAdapter>(adapter: &mut A, hidden: &mut HiddenTaskbarTabs, wid: WindowId) {
    let Some(expected) = hidden.get(&wid).cloned() else {
        return;
    };
    if adapter.identity(wid).as_ref() != Some(&expected) {
        // The original HWND is gone. Never apply its receipt to a replacement
        // that inherited the same numeric handle.
        hidden.remove(&wid);
        return;
    }
    match adapter.add_tab(wid) {
        Ok(()) => {
            hidden.remove(&wid);
        }
        Err(error) => tracing::warn!("ITaskbarList::AddTab({wid}) failed: {error}"),
    }
}

fn restore_tab<A: TaskbarAdapter>(adapter: &mut A, hidden: &mut HiddenTaskbarTabs, wid: WindowId) {
    match adapter.add_tab(wid) {
        Ok(()) => {
            // Startup restore is unconditional, but it may also satisfy an
            // identity-matching in-process receipt.
            if hidden
                .get(&wid)
                .is_some_and(|expected| adapter.identity(wid).as_ref() == Some(expected))
            {
                hidden.remove(&wid);
            }
        }
        Err(error) => tracing::warn!("ITaskbarList::AddTab({wid}) failed: {error}"),
    }
}

fn restore_hidden_tabs<A: TaskbarAdapter>(
    adapter: &mut A,
    hidden: &mut HiddenTaskbarTabs,
) -> Vec<WindowId> {
    let pending: Vec<WindowId> = hidden.keys().copied().collect();
    for wid in pending {
        show_tab(adapter, hidden, wid);
    }
    hidden.keys().copied().collect()
}

fn restore_hidden_tabs_until_complete<A: TaskbarAdapter>(
    adapter: &mut A,
    hidden: &mut HiddenTaskbarTabs,
) {
    let mut attempts = 0u32;
    while !hidden.is_empty() {
        attempts = attempts.saturating_add(1);
        let remaining = restore_hidden_tabs(adapter, hidden);
        if remaining.is_empty() {
            break;
        }
        if attempts == 1 || attempts.is_multiple_of(50) {
            tracing::error!(
                windows = ?remaining,
                attempts,
                "Taskbar shutdown is waiting for verified AddTab recovery"
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn run_commands<A: TaskbarAdapter>(rx: mpsc::Receiver<TaskbarCmd>, adapter: &mut A) {
    let mut hidden = HiddenTaskbarTabs::new();
    loop {
        match rx.recv() {
            Ok(TaskbarCmd::Hide(wid)) => hide_tab(adapter, &mut hidden, wid),
            Ok(TaskbarCmd::Show(wid)) => show_tab(adapter, &mut hidden, wid),
            Ok(TaskbarCmd::Restore(wid)) => restore_tab(adapter, &mut hidden, wid),
            Ok(TaskbarCmd::Forget(wid)) => {
                hidden.remove(&wid);
            }
            Ok(TaskbarCmd::Shutdown(ack_tx)) => {
                restore_hidden_tabs_until_complete(adapter, &mut hidden);
                let _ = ack_tx.send(ShutdownAck {
                    unrestored: Vec::new(),
                });
                break;
            }
            Err(_) => {
                restore_hidden_tabs_until_complete(adapter, &mut hidden);
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
        identity_generation: u64,
        calls: Vec<(&'static str, WindowId)>,
    }

    fn fake_identity(wid: WindowId, generation: u64) -> TaskbarWindowIdentity {
        TaskbarWindowIdentity {
            process_id: wid as u32,
            thread_id: 1,
            class_name: "FakeTaskbarWindow".into(),
            incarnation_token: generation.max(1),
        }
    }

    impl TaskbarAdapter for FakeTaskbar {
        fn identity(&mut self, wid: WindowId) -> Option<TaskbarWindowIdentity> {
            Some(fake_identity(wid, self.identity_generation.max(1)))
        }

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
        let mut hidden = HiddenTaskbarTabs::new();

        hide_tab(&mut adapter, &mut hidden, 7);
        assert!(!hidden.contains_key(&7));
        hide_tab(&mut adapter, &mut hidden, 7);
        assert!(hidden.contains_key(&7));
        assert_eq!(adapter.calls, vec![("delete", 7), ("delete", 7)]);
    }

    #[test]
    fn show_retains_receipt_until_add_tab_succeeds() {
        let mut adapter = FakeTaskbar {
            add_failures: 1,
            ..Default::default()
        };
        let mut hidden = HiddenTaskbarTabs::from([(8, fake_identity(8, 1))]);

        show_tab(&mut adapter, &mut hidden, 8);
        assert!(hidden.contains_key(&8));
        show_tab(&mut adapter, &mut hidden, 8);
        assert!(!hidden.contains_key(&8));
        assert_eq!(adapter.calls, vec![("add", 8), ("add", 8)]);
    }

    #[test]
    fn shutdown_ack_waits_until_restore_is_verified() {
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
                unrestored: Vec::new()
            }
        );
        assert_eq!(adapter.calls, vec![("delete", 9), ("add", 9), ("add", 9)]);
    }

    #[test]
    fn shutdown_retries_a_retained_receipt_after_prior_add_failure() {
        let mut adapter = FakeTaskbar {
            add_failures: 1,
            ..Default::default()
        };
        let mut hidden = HiddenTaskbarTabs::from([(10, fake_identity(10, 1))]);

        show_tab(&mut adapter, &mut hidden, 10);
        assert!(hidden.contains_key(&10));
        assert!(restore_hidden_tabs(&mut adapter, &mut hidden).is_empty());
        assert!(hidden.is_empty());
        assert_eq!(adapter.calls, vec![("add", 10), ("add", 10)]);
    }

    #[test]
    fn recycled_hwnd_retires_receipt_without_touching_replacement() {
        let mut adapter = FakeTaskbar {
            identity_generation: 2,
            ..Default::default()
        };
        let mut hidden = HiddenTaskbarTabs::from([(11, fake_identity(11, 1))]);

        show_tab(&mut adapter, &mut hidden, 11);

        assert!(hidden.is_empty());
        assert!(adapter.calls.is_empty());
    }
}
