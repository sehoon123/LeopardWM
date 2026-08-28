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
use windows::core::w;
use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::{ITaskbarList, TaskbarList};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetPropW, GetWindowThreadProcessId, IsWindow, RemovePropW, SetPropW,
};

enum TaskbarCmd {
    Hide(WindowId),
    Show(WindowId),
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
const TASKBAR_SHUTDOWN_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(750);
const TASKBAR_INIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

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

/// Retire `wid` from taskbar ownership. A matching live hidden incarnation is
/// restored first; dead/recycled incarnations are dropped without touching the
/// replacement.
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

    match ready_rx.recv_timeout(TASKBAR_INIT_TIMEOUT) {
        Ok(Ok(())) => {
            let mut slot = TASKBAR_TX.lock().unwrap_or_else(recover_poisoned_mutex);
            if slot.is_some() {
                // Another initializer won the race while this thread was
                // starting. Ask ours to stop cleanly before declining it.
                drop(slot);
                let (ack_tx, ack_rx) = mpsc::channel();
                let _ = tx.send(TaskbarCmd::Shutdown(ack_tx));
                let _ = ack_rx.recv_timeout(TASKBAR_SHUTDOWN_ACK_TIMEOUT);
                if thread.is_finished() {
                    let _ = thread.join();
                }
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
        Err(mpsc::RecvTimeoutError::Timeout) => {
            tracing::warn!("Taskbar controller initialization timed out; continuing without it");
            drop(tx);
            if thread.is_finished() {
                let _ = thread.join();
            }
            None
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            tracing::warn!("Taskbar controller exited before initialization completed");
            if thread.is_finished() {
                let _ = thread.join();
            }
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
                Ok(()) => match ack_rx.recv_timeout(TASKBAR_SHUTDOWN_ACK_TIMEOUT) {
                    Ok(ack) if ack.unrestored.is_empty() => {}
                    Ok(ack) => tracing::error!(
                        windows = ?ack.unrestored,
                        "Taskbar shutdown completed with durable recovery pending"
                    ),
                    Err(mpsc::RecvTimeoutError::Timeout) => tracing::error!(
                        "Taskbar shutdown timed out; durable HWND markers remain for watchdog recovery"
                    ),
                    Err(mpsc::RecvTimeoutError::Disconnected) => tracing::error!(
                        "Taskbar controller exited without a shutdown acknowledgement"
                    ),
                },
                Err(_) => tracing::error!("Taskbar controller stopped before shutdown command"),
            }
        }

        if let Some(thread) = self.thread.take() {
            for _ in 0..10 {
                if thread.is_finished() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            if thread.is_finished() {
                let _ = thread.join();
            } else {
                // Remaining worker operations are AddTab-only. Detach rather
                // than blocking daemon exit; the watchdog consumes markers
                // after process termination.
                tracing::warn!("Detaching unfinished taskbar recovery worker");
            }
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

fn taskbar_marker_token(wid: WindowId) -> u64 {
    unsafe { GetPropW(hwnd_of(wid), w!("LeopardWM.TaskbarHidden.v1")) }.0 as u64
}

fn install_taskbar_marker(wid: WindowId, identity: &TaskbarWindowIdentity) -> bool {
    if !crate::event_hooks::window_incarnation_property_matches(wid, identity.incarnation_token) {
        return false;
    }
    unsafe {
        SetPropW(
            hwnd_of(wid),
            w!("LeopardWM.TaskbarHidden.v1"),
            Some(HANDLE(identity.incarnation_token as usize as *mut c_void)),
        )
    }
    .is_ok()
}

fn clear_taskbar_marker_if_owned(wid: WindowId, token: u64) {
    if taskbar_marker_token(wid) == token {
        unsafe {
            let _ = RemovePropW(hwnd_of(wid), w!("LeopardWM.TaskbarHidden.v1"));
        }
    }
}

/// Narrow adapter seam: all ledger transitions are unit-testable without COM.
trait TaskbarAdapter {
    fn identity(&mut self, wid: WindowId) -> Option<TaskbarWindowIdentity>;
    fn install_marker(&mut self, wid: WindowId, identity: &TaskbarWindowIdentity) -> bool;
    fn marker_token(&mut self, wid: WindowId) -> u64;
    fn clear_marker(&mut self, wid: WindowId, token: u64);
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

    fn install_marker(&mut self, wid: WindowId, identity: &TaskbarWindowIdentity) -> bool {
        install_taskbar_marker(wid, identity)
    }

    fn marker_token(&mut self, wid: WindowId) -> u64 {
        taskbar_marker_token(wid)
    }

    fn clear_marker(&mut self, wid: WindowId, token: u64) {
        clear_taskbar_marker_if_owned(wid, token);
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
    if !adapter.install_marker(wid, &identity)
        || adapter.identity(wid).as_ref() != Some(&identity)
        || adapter.marker_token(wid) != identity.incarnation_token
    {
        adapter.clear_marker(wid, identity.incarnation_token);
        tracing::warn!("Could not establish taskbar ownership for window {wid}");
        return;
    }
    match adapter.delete_tab(wid) {
        Ok(())
            if adapter.identity(wid).as_ref() == Some(&identity)
                && adapter.marker_token(wid) == identity.incarnation_token =>
        {
            hidden.insert(wid, identity);
        }
        Ok(()) => {
            // DeleteTab may have raced HWND reuse. AddTab is compensating and
            // monotonic (show-only), so the replacement cannot remain hidden by
            // an operation intended for the destroyed source.
            let _ = adapter.add_tab(wid);
            adapter.clear_marker(wid, identity.incarnation_token);
            tracing::warn!("Compensated taskbar hide across HWND incarnation change for {wid}");
        }
        Err(error) => {
            adapter.clear_marker(wid, identity.incarnation_token);
            tracing::warn!("ITaskbarList::DeleteTab({wid}) failed: {error}");
        }
    }
}

fn show_tab<A: TaskbarAdapter>(adapter: &mut A, hidden: &mut HiddenTaskbarTabs, wid: WindowId) {
    let Some(expected) = hidden.get(&wid).cloned() else {
        return;
    };
    if adapter.identity(wid).as_ref() != Some(&expected)
        || adapter.marker_token(wid) != expected.incarnation_token
    {
        adapter.clear_marker(wid, expected.incarnation_token);
        hidden.remove(&wid);
        return;
    }
    match adapter.add_tab(wid) {
        Ok(()) => {
            adapter.clear_marker(wid, expected.incarnation_token);
            hidden.remove(&wid);
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

fn restore_hidden_tabs_with_policy<A: TaskbarAdapter>(
    adapter: &mut A,
    hidden: &mut HiddenTaskbarTabs,
    max_attempts: u32,
    retry_delay: std::time::Duration,
) -> Vec<WindowId> {
    let mut remaining = Vec::new();
    for attempt in 1..=max_attempts {
        remaining = restore_hidden_tabs(adapter, hidden);
        if remaining.is_empty() {
            break;
        }
        if attempt == 1 || attempt == max_attempts {
            tracing::error!(
                windows = ?remaining,
                attempt,
                "Taskbar shutdown has pending durable AddTab recovery"
            );
        }
        if attempt != max_attempts {
            std::thread::sleep(retry_delay);
        }
    }
    remaining
}

fn restore_hidden_tabs_bounded<A: TaskbarAdapter>(
    adapter: &mut A,
    hidden: &mut HiddenTaskbarTabs,
) -> Vec<WindowId> {
    restore_hidden_tabs_with_policy(adapter, hidden, 50, std::time::Duration::from_millis(100))
}

fn run_commands<A: TaskbarAdapter>(rx: mpsc::Receiver<TaskbarCmd>, adapter: &mut A) {
    let mut hidden = HiddenTaskbarTabs::new();
    loop {
        match rx.recv() {
            Ok(TaskbarCmd::Hide(wid)) => hide_tab(adapter, &mut hidden, wid),
            Ok(TaskbarCmd::Show(wid)) => show_tab(adapter, &mut hidden, wid),
            Ok(TaskbarCmd::Forget(wid)) => show_tab(adapter, &mut hidden, wid),
            Ok(TaskbarCmd::Shutdown(ack_tx)) => {
                let unrestored = restore_hidden_tabs_bounded(adapter, &mut hidden);
                let _ = ack_tx.send(ShutdownAck { unrestored });
                break;
            }
            Err(_) => {
                let unrestored = restore_hidden_tabs_bounded(adapter, &mut hidden);
                if !unrestored.is_empty() {
                    tracing::error!(windows = ?unrestored, "Taskbar channel closed with durable recovery pending");
                }
                break;
            }
        }
    }
}

fn restore_marked_taskbar_tabs_with(taskbar: &ITaskbarList) -> usize {
    let mut restored = 0;
    for wid in crate::enumeration::collect_all_top_level_window_ids() {
        let token = taskbar_marker_token(wid);
        if token == 0 {
            continue;
        }
        match unsafe { taskbar.AddTab(hwnd_of(wid)) } {
            Ok(()) => {
                clear_taskbar_marker_if_owned(wid, token);
                if taskbar_marker_token(wid) == 0 {
                    restored += 1;
                }
            }
            Err(error) => tracing::warn!("Marked taskbar recovery failed for {wid:#x}: {error}"),
        }
    }
    restored
}

/// Cross-process recovery for taskbar hides whose durable HWND marker survived
/// a daemon exit. `AddTab` is show-only, so a concurrent HWND replacement is
/// never hidden; marker clearing remains token-qualified.
pub fn restore_marked_taskbar_tabs_best_effort() -> usize {
    unsafe {
        let coinit = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        if coinit.is_err() {
            tracing::warn!("Taskbar recovery COM initialization failed: {coinit:?}");
            return 0;
        }
        let result = (|| -> Result<usize, windows::core::Error> {
            let taskbar: ITaskbarList = CoCreateInstance(&TaskbarList, None, CLSCTX_ALL)?;
            taskbar.HrInit()?;
            Ok(restore_marked_taskbar_tabs_with(&taskbar))
        })();
        CoUninitialize();
        match result {
            Ok(restored) => restored,
            Err(error) => {
                tracing::warn!("Marked taskbar recovery failed: {error}");
                0
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

        let recovered = restore_marked_taskbar_tabs_with(&taskbar);
        let _ = ready_tx.send(Ok(()));
        tracing::info!(recovered, "Taskbar controller initialized");
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
        reuse_on_delete: bool,
        markers: HashMap<WindowId, u64>,
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

        fn install_marker(&mut self, wid: WindowId, identity: &TaskbarWindowIdentity) -> bool {
            self.markers.insert(wid, identity.incarnation_token);
            true
        }

        fn marker_token(&mut self, wid: WindowId) -> u64 {
            self.markers.get(&wid).copied().unwrap_or(0)
        }

        fn clear_marker(&mut self, wid: WindowId, token: u64) {
            if self.markers.get(&wid).copied() == Some(token) {
                self.markers.remove(&wid);
            }
        }

        fn delete_tab(&mut self, wid: WindowId) -> Result<(), String> {
            self.calls.push(("delete", wid));
            if self.delete_failures > 0 {
                self.delete_failures -= 1;
                Err("injected DeleteTab failure".into())
            } else {
                if self.reuse_on_delete {
                    self.identity_generation = self.identity_generation.max(1) + 1;
                    self.markers.remove(&wid);
                }
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
            markers: HashMap::from([(8, 1)]),
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
    fn forget_restores_a_matching_live_hidden_tab() {
        let mut adapter = FakeTaskbar::default();
        let (tx, rx) = mpsc::channel();
        let (ack_tx, ack_rx) = mpsc::channel();
        tx.send(TaskbarCmd::Hide(14)).unwrap();
        tx.send(TaskbarCmd::Forget(14)).unwrap();
        tx.send(TaskbarCmd::Shutdown(ack_tx)).unwrap();
        drop(tx);

        run_commands(rx, &mut adapter);

        assert!(ack_rx.recv().unwrap().unrestored.is_empty());
        assert_eq!(adapter.calls, vec![("delete", 14), ("add", 14)]);
        assert!(!adapter.markers.contains_key(&14));
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
            markers: HashMap::from([(10, 1)]),
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
    fn handle_drop_has_a_wall_clock_bound_without_worker_ack() {
        let (tx, rx) = mpsc::channel();
        *TASKBAR_TX.lock().unwrap_or_else(recover_poisoned_mutex) = Some(tx);
        let worker = std::thread::spawn(move || {
            let _rx = rx;
            std::thread::sleep(std::time::Duration::from_secs(1));
        });
        let handle = TaskbarHandle {
            thread: Some(worker),
        };

        let start = std::time::Instant::now();
        drop(handle);
        assert!(start.elapsed() < std::time::Duration::from_millis(950));
    }

    #[test]
    fn permanent_add_failure_is_bounded_and_handed_off() {
        let mut adapter = FakeTaskbar {
            add_failures: usize::MAX,
            markers: HashMap::from([(12, 1)]),
            ..Default::default()
        };
        let mut hidden = HiddenTaskbarTabs::from([(12, fake_identity(12, 1))]);

        let remaining = restore_hidden_tabs_with_policy(
            &mut adapter,
            &mut hidden,
            2,
            std::time::Duration::ZERO,
        );

        assert_eq!(remaining, vec![12]);
        assert_eq!(adapter.calls, vec![("add", 12), ("add", 12)]);
        assert_eq!(adapter.markers.get(&12), Some(&1));
    }

    #[test]
    fn delete_tab_hwnd_reuse_is_compensated_with_show_only_add() {
        let mut adapter = FakeTaskbar {
            reuse_on_delete: true,
            ..Default::default()
        };
        let mut hidden = HiddenTaskbarTabs::new();

        hide_tab(&mut adapter, &mut hidden, 13);

        assert!(hidden.is_empty());
        assert_eq!(adapter.calls, vec![("delete", 13), ("add", 13)]);
    }

    #[test]
    fn recycled_hwnd_retires_receipt_without_touching_replacement() {
        let mut adapter = FakeTaskbar {
            identity_generation: 2,
            markers: HashMap::from([(11, 1)]),
            ..Default::default()
        };
        let mut hidden = HiddenTaskbarTabs::from([(11, fake_identity(11, 1))]);

        show_tab(&mut adapter, &mut hidden, 11);

        assert!(hidden.is_empty());
        assert!(adapter.calls.is_empty());
    }
}
