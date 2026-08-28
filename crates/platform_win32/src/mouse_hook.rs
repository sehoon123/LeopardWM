//! Low-level mouse hook for focus-follows-mouse.

use crate::{
    normalize_to_root_window, recover_poisoned_mutex, should_emit_window_event, Win32Error,
    WindowEvent, WM_QUIT_LLHOOK_THREAD,
};
use leopardwm_core_layout::WindowId;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use windows::Win32::Foundation::POINT;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetCursorPos, GetMessageW, KillTimer, PeekMessageW,
    PostThreadMessageW, SetTimer, SetWindowsHookExW, UnhookWindowsHookEx, WindowFromPoint, MSG,
    MSLLHOOKSTRUCT, PM_NOREMOVE, WH_MOUSE_LL, WM_MOUSEMOVE, WM_TIMER,
};

/// Global sender for mouse enter events.
static MOUSE_EVENT_SENDER: std::sync::Mutex<Option<mpsc::SyncSender<WindowEvent>>> =
    std::sync::Mutex::new(None);

/// Track the window the mouse is currently over.
static CURRENT_MOUSE_WINDOW: std::sync::Mutex<Option<WindowId>> = std::sync::Mutex::new(None);

/// Whether the currently-tracked window is one we'd manage (i.e. we emitted a
/// `MouseEnterWindow` for it). Lets us fire `MouseLeftManaged` exactly on the
/// managed -> non-manageable transition, not on every move over the taskbar.
static CURRENT_OVER_MANAGED: std::sync::Mutex<bool> = std::sync::Mutex::new(false);
static ACTIVE_MOUSE_THREAD: AtomicU32 = AtomicU32::new(0);
static MOUSE_RETRY_REQUIRED: AtomicBool = AtomicBool::new(false);

fn clear_mouse_globals() {
    MOUSE_RETRY_REQUIRED.store(false, Ordering::Release);
    *MOUSE_EVENT_SENDER
        .lock()
        .unwrap_or_else(recover_poisoned_mutex) = None;
    *CURRENT_MOUSE_WINDOW
        .lock()
        .unwrap_or_else(recover_poisoned_mutex) = None;
    *CURRENT_OVER_MANAGED
        .lock()
        .unwrap_or_else(recover_poisoned_mutex) = false;
}

fn retire_mouse_thread(thread_id: u32) {
    if ACTIVE_MOUSE_THREAD
        .compare_exchange(thread_id, 0, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        clear_mouse_globals();
    }
}

/// Handle for the low-level mouse hook.
///
/// Dropping this handle will signal the dedicated message-pump thread to
/// unhook and exit.
pub struct MouseHookHandle {
    thread_id: u32,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for MouseHookHandle {
    fn drop(&mut self) {
        let posted = unsafe {
            PostThreadMessageW(
                self.thread_id,
                WM_QUIT_LLHOOK_THREAD,
                windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM(0),
            )
            .is_ok()
        };
        if let Some(thread) = self.thread.take() {
            if posted {
                for _ in 0..30 {
                    if thread.is_finished() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
            if thread.is_finished() {
                let _ = thread.join();
                tracing::debug!("Mouse hook uninstalled");
            } else {
                tracing::warn!(
                    "Mouse hook did not acknowledge shutdown; retaining singleton globals until unhook"
                );
            }
        }
    }
}

/// Install a low-level mouse hook for focus-follows-mouse functionality.
///
/// Spawns a dedicated thread with a Win32 message pump so that `WH_MOUSE_LL`
/// callbacks are dispatched promptly.
///
/// # Arguments
/// * `event_sender` - Sender for WindowEvent (specifically MouseEnterWindow)
pub fn install_mouse_hook(
    event_sender: mpsc::SyncSender<WindowEvent>,
) -> Result<MouseHookHandle, Win32Error> {
    {
        let mut sender = MOUSE_EVENT_SENDER.lock().map_err(|_| {
            Win32Error::HookInstallFailed("Mouse sender mutex poisoned".to_string())
        })?;
        if sender.is_some() {
            return Err(Win32Error::HookInstallFailed(
                "Mouse sender already initialized - drop existing MouseHookHandle first"
                    .to_string(),
            ));
        }
        *sender = Some(event_sender);
    }

    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<u32, Win32Error>>();

    let thread = std::thread::Builder::new()
        .name("mouse-hook".into())
        .spawn(move || {
            unsafe {
                let thread_id = GetCurrentThreadId();
                ACTIVE_MOUSE_THREAD.store(thread_id, Ordering::Release);

                // PeekMessageW forces the queue to exist before the hook installs.
                let mut msg = MSG::default();
                let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);

                let hook = match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_ll_hook_proc), None, 0) {
                    Ok(h) => h,
                    Err(e) => {
                        let _ = init_tx.send(Err(Win32Error::HookInstallFailed(format!(
                            "SetWindowsHookExW failed: {}",
                            e
                        ))));
                        retire_mouse_thread(thread_id);
                        return;
                    }
                };

                let timer_id = SetTimer(None, 0, 25, None);
                if timer_id == 0 {
                    let _ = UnhookWindowsHookEx(hook);
                    let _ = init_tx.send(Err(Win32Error::HookInstallFailed(
                        "Could not create mouse reconciliation timer".into(),
                    )));
                    retire_mouse_thread(thread_id);
                    return;
                }

                let _ = init_tx.send(Ok(thread_id));

                // Message pump — required for WH_MOUSE_LL callbacks
                loop {
                    let ret = GetMessageW(&mut msg, None, 0, 0).0;
                    if ret <= 0 {
                        break;
                    }
                    if msg.message == WM_QUIT_LLHOOK_THREAD {
                        break;
                    }
                    if msg.message == WM_TIMER {
                        retry_current_mouse_position();
                        continue;
                    }
                    let _ = DispatchMessageW(&msg);
                }

                let _ = KillTimer(None, timer_id);
                let _ = UnhookWindowsHookEx(hook);
                retire_mouse_thread(thread_id);
            }
        })
        .map_err(|e| {
            clear_mouse_globals();
            Win32Error::HookInstallFailed(format!("Failed to spawn mouse hook thread: {}", e))
        })?;

    let thread_id = match init_rx.recv() {
        Ok(Ok(thread_id)) => thread_id,
        Ok(Err(error)) => {
            let _ = thread.join();
            return Err(error);
        }
        Err(_) => {
            if thread.is_finished() {
                let _ = thread.join();
            }
            return Err(Win32Error::HookInstallFailed(
                "Mouse hook thread initialization failed".to_string(),
            ));
        }
    };

    tracing::info!("Low-level mouse hook installed for focus-follows-mouse");

    Ok(MouseHookHandle {
        thread_id,
        thread: Some(thread),
    })
}

fn reconcile_mouse_position(point: POINT) {
    let raw_hwnd = unsafe { WindowFromPoint(point) };
    let candidate_hwnd = if raw_hwnd.is_invalid() {
        None
    } else {
        let normalized = normalize_to_root_window(raw_hwnd);
        (!normalized.is_invalid()).then_some(normalized)
    };
    let candidate_window_id = candidate_hwnd.map(|hwnd| hwnd.0 as WindowId);

    let mut current = match CURRENT_MOUSE_WINDOW.try_lock() {
        Ok(current) => current,
        Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            MOUSE_RETRY_REQUIRED.store(true, Ordering::Release);
            return;
        }
    };
    if *current == candidate_window_id {
        MOUSE_RETRY_REQUIRED.store(false, Ordering::Release);
        return;
    }
    let candidate_managed = candidate_hwnd.is_some_and(should_emit_window_event);
    let mut over_managed = match CURRENT_OVER_MANAGED.try_lock() {
        Ok(managed) => managed,
        Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            MOUSE_RETRY_REQUIRED.store(true, Ordering::Release);
            return;
        }
    };
    let event = if candidate_managed {
        candidate_hwnd.map(|hwnd| WindowEvent::MouseEnterWindow(hwnd.0 as WindowId))
    } else if *over_managed {
        Some(WindowEvent::MouseLeftManaged)
    } else {
        None
    };
    let reconciled = match MOUSE_EVENT_SENDER.try_lock() {
        Ok(sender) => {
            event.is_none()
                || sender.as_ref().is_none_or(|sender| {
                    match sender.try_send(event.expect("event exists")) {
                        Ok(()) | Err(mpsc::TrySendError::Disconnected(_)) => true,
                        Err(mpsc::TrySendError::Full(_)) => false,
                    }
                })
        }
        Err(std::sync::TryLockError::Poisoned(error)) => {
            let sender = error.into_inner();
            event.is_none()
                || sender.as_ref().is_none_or(|sender| {
                    match sender.try_send(event.expect("event exists")) {
                        Ok(()) | Err(mpsc::TrySendError::Disconnected(_)) => true,
                        Err(mpsc::TrySendError::Full(_)) => false,
                    }
                })
        }
        Err(std::sync::TryLockError::WouldBlock) => false,
    };
    if reconciled {
        *current = candidate_window_id;
        *over_managed = candidate_managed;
    }
    MOUSE_RETRY_REQUIRED.store(!reconciled, Ordering::Release);
}

fn retry_current_mouse_position() {
    if !MOUSE_RETRY_REQUIRED.load(Ordering::Acquire) {
        return;
    }
    let mut point = POINT::default();
    if unsafe { GetCursorPos(&mut point) }.is_ok() {
        reconcile_mouse_position(point);
    }
}

/// Low-level mouse hook callback.
///
/// Tracks mouse movement and sends MouseEnterWindow events when the cursor
/// enters a different window.
unsafe extern "system" fn mouse_ll_hook_proc(
    ncode: i32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    // ncode < 0 or a retired installer generation: do not process, just chain.
    if ncode < 0 || ACTIVE_MOUSE_THREAD.load(Ordering::Acquire) != GetCurrentThreadId() {
        return CallNextHookEx(None, ncode, wparam, lparam);
    }

    if wparam.0 as u32 == WM_MOUSEMOVE {
        let mouse_struct = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        reconcile_mouse_position(mouse_struct.pt);
    }

    CallNextHookEx(None, ncode, wparam, lparam)
}

#[cfg(test)]
mod tests {
    use super::*;

    static MOUSE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn teardown_resets_pointer_state_and_stale_generation_is_inert() {
        let _guard = MOUSE_TEST_LOCK
            .lock()
            .unwrap_or_else(recover_poisoned_mutex);
        let (tx, _rx) = mpsc::sync_channel(4);
        *MOUSE_EVENT_SENDER
            .lock()
            .unwrap_or_else(recover_poisoned_mutex) = Some(tx);
        *CURRENT_MOUSE_WINDOW
            .lock()
            .unwrap_or_else(recover_poisoned_mutex) = Some(77);
        *CURRENT_OVER_MANAGED
            .lock()
            .unwrap_or_else(recover_poisoned_mutex) = true;
        ACTIVE_MOUSE_THREAD.store(12, Ordering::Release);

        retire_mouse_thread(11);
        assert!(MOUSE_EVENT_SENDER
            .lock()
            .unwrap_or_else(recover_poisoned_mutex)
            .is_some());
        retire_mouse_thread(12);
        assert!(MOUSE_EVENT_SENDER
            .lock()
            .unwrap_or_else(recover_poisoned_mutex)
            .is_none());
        assert_eq!(
            *CURRENT_MOUSE_WINDOW
                .lock()
                .unwrap_or_else(recover_poisoned_mutex),
            None
        );
        assert!(!*CURRENT_OVER_MANAGED
            .lock()
            .unwrap_or_else(recover_poisoned_mutex));
    }
}
