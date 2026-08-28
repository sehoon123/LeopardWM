//! WinEvent hook installation and dispatch.

use crate::enumeration::{
    normalize_to_root_window, should_emit_window_event_for,
    should_filter_window_event_by_manageability,
};
use crate::recover_poisoned_mutex;
use leopardwm_core_layout::WindowId;
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use windows::core::w;
use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetClassNameW, GetForegroundWindow, GetMessageW, GetPropW,
    GetWindowThreadProcessId, IsWindow, PeekMessageW, PostThreadMessageW, SetPropW, MSG,
    PM_NOREMOVE, WM_USER,
};

// WinEvent constants (not all are exposed by windows-rs)
pub(crate) const EVENT_OBJECT_CREATE: u32 = 0x8000;
pub(crate) const EVENT_OBJECT_DESTROY: u32 = 0x8001;
pub(crate) const EVENT_OBJECT_SHOW: u32 = 0x8002;
pub(crate) const EVENT_OBJECT_HIDE: u32 = 0x8003;
pub(crate) const EVENT_OBJECT_FOCUS: u32 = 0x8005;
pub(crate) const EVENT_SYSTEM_FOREGROUND: u32 = 0x0003;
pub(crate) const EVENT_SYSTEM_MINIMIZESTART: u32 = 0x0016;
pub(crate) const EVENT_SYSTEM_MINIMIZEEND: u32 = 0x0017;
pub(crate) const EVENT_SYSTEM_MOVESIZESTART: u32 = 0x000A;
pub(crate) const EVENT_SYSTEM_MOVESIZEEND: u32 = 0x000B;
pub(crate) const EVENT_OBJECT_LOCATIONCHANGE: u32 = 0x800B;
pub(crate) const EVENT_OBJECT_NAMECHANGE: u32 = 0x800C;
const OBJID_WINDOW: i32 = 0;
const WINEVENT_OUTOFCONTEXT: u32 = 0x0000;
const WINEVENT_SKIPOWNPROCESS: u32 = 0x0002;

/// Custom message ID used to signal the WinEvent hook thread to exit.
const WM_QUIT_WINEVENT_THREAD: u32 = WM_USER + 3;

/// Window event types that the daemon needs to handle.
#[derive(Debug, Clone)]
pub enum WindowEvent {
    /// A new window was created.
    Created(WindowId),
    /// A window was destroyed.
    Destroyed(WindowId),
    /// A window was hidden (e.g., close-to-tray apps using ShowWindow(SW_HIDE)).
    Hidden(WindowId),
    /// A window received focus.
    Focused(WindowId),
    /// A window was minimized.
    Minimized(WindowId),
    /// A window was restored from minimized state.
    Restored(WindowId),
    /// A window was moved or resized by the user.
    MovedOrResized(WindowId),
    /// User started dragging/resizing a window.
    MoveSizeStart(WindowId),
    /// User finished dragging/resizing a window.
    MoveSizeEnd(WindowId),
    /// Display configuration changed (monitors added/removed/rearranged).
    DisplayChange,
    /// The desktop work area changed without a topology change (e.g. the
    /// taskbar toggled between auto-hide and always-on). Reconciled on a
    /// shorter debounce than `DisplayChange` since it settles quickly.
    WorkAreaChanged,
    /// System appearance changed (theme or high-contrast setting). This is
    /// separate from display topology so the daemon can refresh UI caches
    /// without a monitor reconcile.
    AppearanceChanged,
    /// Mouse cursor entered a window (for focus-follows-mouse).
    MouseEnterWindow(WindowId),
    /// Mouse cursor left a manageable window for a non-manageable one (the
    /// taskbar, a popup, etc.). Cancels any pending focus-follows-mouse focus
    /// so a debounced focus doesn't fire on a window the cursor has left.
    MouseLeftManaged,
    /// A window's title text changed. The daemon refreshes the tab
    /// strip overlay so tab labels stay in sync with the underlying
    /// window's title without waiting for the next layout-changing
    /// event.
    TitleChanged(WindowId),
}

/// Callback-captured HWND incarnation. The token is stored as a window
/// property, so a newly created same-process/same-thread/same-class HWND cannot
/// inherit an older numeric handle's ownership.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowEventIdentity {
    pub token: u64,
    pub process_id: u32,
    pub thread_id: u32,
    pub class_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum IdentityEventKind {
    Created,
    Destroyed,
    Hidden,
    Restored,
}

static NEXT_WINDOW_INCARNATION: AtomicU64 = AtomicU64::new(1);
static WINDOW_IDENTITIES: std::sync::Mutex<Option<HashMap<WindowId, WindowEventIdentity>>> =
    std::sync::Mutex::new(None);
type PendingIdentityQueues = HashMap<(IdentityEventKind, WindowId), VecDeque<WindowEventIdentity>>;
static PENDING_EVENT_IDENTITIES: std::sync::Mutex<Option<PendingIdentityQueues>> =
    std::sync::Mutex::new(None);

fn next_window_incarnation() -> u64 {
    NEXT_WINDOW_INCARNATION
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            Some(value.wrapping_add(1).max(1))
        })
        .unwrap_or_else(|value| value)
        .max(1)
}

fn identity_property_token(hwnd: HWND) -> u64 {
    unsafe { GetPropW(hwnd, w!("LeopardWM.WindowIncarnation.v1")) }.0 as u64
}

fn capture_live_window_identity(
    window_id: WindowId,
    force_new_if_unmarked: bool,
) -> Option<WindowEventIdentity> {
    let hwnd = HWND(window_id as *mut c_void);
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
    let class_name = String::from_utf16_lossy(&class[..class_len as usize]);
    let property_token = identity_property_token(hwnd);
    let existing = WINDOW_IDENTITIES
        .lock()
        .unwrap_or_else(recover_poisoned_mutex)
        .as_ref()
        .and_then(|identities| identities.get(&window_id).cloned());
    let token = if property_token != 0 {
        property_token
    } else if !force_new_if_unmarked {
        existing
            .filter(|identity| {
                identity.process_id == process_id
                    && identity.thread_id == thread_id
                    && identity.class_name == class_name
            })
            .map(|identity| identity.token)
            .unwrap_or_else(next_window_incarnation)
    } else {
        next_window_incarnation()
    };
    if property_token == 0 {
        // SetPropW may be denied for a higher-integrity foreign HWND. Those
        // windows are not managed, but retain the in-process token as a bounded
        // fallback so callback ordering remains explicit.
        let _ = unsafe {
            SetPropW(
                hwnd,
                w!("LeopardWM.WindowIncarnation.v1"),
                Some(HANDLE(token as usize as *mut c_void)),
            )
        };
    }
    let identity = WindowEventIdentity {
        token,
        process_id,
        thread_id,
        class_name,
    };
    WINDOW_IDENTITIES
        .lock()
        .unwrap_or_else(recover_poisoned_mutex)
        .get_or_insert_with(HashMap::new)
        .insert(window_id, identity.clone());
    Some(identity)
}

/// Ensure and return the nonzero token for a currently live HWND.
pub fn ensure_window_incarnation_token(window_id: WindowId) -> Option<u64> {
    capture_live_window_identity(window_id, false).map(|identity| identity.token)
}

/// Return the complete current live identity, assigning a property token when
/// this is the first observation in the process.
pub fn current_window_event_identity(window_id: WindowId) -> Option<WindowEventIdentity> {
    capture_live_window_identity(window_id, false)
}

fn identity_event_kind(event: &WindowEvent) -> Option<(IdentityEventKind, WindowId)> {
    match event {
        WindowEvent::Created(window_id) => Some((IdentityEventKind::Created, *window_id)),
        WindowEvent::Destroyed(window_id) => Some((IdentityEventKind::Destroyed, *window_id)),
        WindowEvent::Hidden(window_id) => Some((IdentityEventKind::Hidden, *window_id)),
        WindowEvent::Restored(window_id) => Some((IdentityEventKind::Restored, *window_id)),
        _ => None,
    }
}

fn queue_window_event_identity(event: &WindowEvent) {
    let Some((kind, window_id)) = identity_event_kind(event) else {
        return;
    };
    let identity = match kind {
        IdentityEventKind::Created => capture_live_window_identity(window_id, true),
        IdentityEventKind::Destroyed => WINDOW_IDENTITIES
            .lock()
            .unwrap_or_else(recover_poisoned_mutex)
            .get_or_insert_with(HashMap::new)
            .remove(&window_id),
        IdentityEventKind::Hidden | IdentityEventKind::Restored => {
            capture_live_window_identity(window_id, false)
        }
    };
    if let Some(identity) = identity {
        PENDING_EVENT_IDENTITIES
            .lock()
            .unwrap_or_else(recover_poisoned_mutex)
            .get_or_insert_with(HashMap::new)
            .entry((kind, window_id))
            .or_default()
            .push_back(identity);
    }
}

/// Consume the exact callback identity queued with this lifecycle event.
/// Synthetic/test events and non-WinEvent sources simply return `None`.
pub fn take_window_event_identity(event: &WindowEvent) -> Option<WindowEventIdentity> {
    let key = identity_event_kind(event)?;
    let mut pending = PENDING_EVENT_IDENTITIES
        .lock()
        .unwrap_or_else(recover_poisoned_mutex);
    let queue = pending.as_mut()?.get_mut(&key)?;
    let identity = queue.pop_front();
    if queue.is_empty() {
        pending.as_mut()?.remove(&key);
    }
    identity
}

/// Global sender for window events from WinEvent callbacks.
///
/// This uses a thread-safe channel because WinEvent callbacks run on Windows'
/// internal thread pool and we need to forward events to the async runtime.
static EVENT_SENDER: std::sync::Mutex<Option<mpsc::SyncSender<WindowEvent>>> =
    std::sync::Mutex::new(None);
static ACTIVE_EVENT_THREAD: AtomicU32 = AtomicU32::new(0);

pub(crate) fn set_event_sender(
    sender: mpsc::SyncSender<WindowEvent>,
) -> Result<(), crate::Win32Error> {
    let mut guard = EVENT_SENDER.lock().map_err(|_| {
        crate::Win32Error::HookInstallFailed("Event sender mutex poisoned".to_string())
    })?;
    if guard.is_some() {
        return Err(crate::Win32Error::HookInstallFailed(
            "Event sender already initialized - drop existing EventHookHandle first".to_string(),
        ));
    }
    *guard = Some(sender);
    Ok(())
}

pub(crate) fn clear_event_sender() {
    let mut guard = EVENT_SENDER.lock().unwrap_or_else(recover_poisoned_mutex);
    *guard = None;
}

pub(crate) fn clone_event_sender() -> Option<mpsc::SyncSender<WindowEvent>> {
    let guard = EVENT_SENDER.lock().unwrap_or_else(recover_poisoned_mutex);
    guard.as_ref().cloned()
}

fn retire_event_thread(thread_id: u32) {
    if ACTIVE_EVENT_THREAD
        .compare_exchange(thread_id, 0, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        clear_event_sender();
    }
}

/// Handle for installed event hooks.
///
/// Dropping this handle will unhook all installed event hooks.
pub struct EventHookHandle {
    thread_id: u32,
    _thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for EventHookHandle {
    fn drop(&mut self) {
        let posted = unsafe {
            PostThreadMessageW(
                self.thread_id,
                WM_QUIT_WINEVENT_THREAD,
                windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM(0),
            )
            .is_ok()
        };
        if let Some(handle) = self._thread.take() {
            if posted {
                for _ in 0..30 {
                    if handle.is_finished() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
            if handle.is_finished() {
                let _ = handle.join();
                tracing::debug!("WinEvent hook thread stopped");
            } else {
                // The owning thread clears globals only after unhooking. Until
                // then reinstall remains fail-closed instead of letting a stale
                // callback use replacement-generation state.
                tracing::warn!(
                    "WinEvent hook thread did not acknowledge shutdown; retaining singleton ownership"
                );
            }
        }
    }
}

/// Install WinEvent hooks to receive window lifecycle events.
///
/// Spawns a dedicated thread with a Win32 message pump so that
/// `WINEVENT_OUTOFCONTEXT` callbacks are dispatched reliably.
///
/// Returns a handle that must be kept alive to receive events.
/// Also returns a receiver channel for the events.
///
/// # Events Hooked
/// - Window creation (EVENT_OBJECT_CREATE)
/// - Window destruction (EVENT_OBJECT_DESTROY)
/// - Foreground change (EVENT_SYSTEM_FOREGROUND)
/// - Minimize/restore (EVENT_SYSTEM_MINIMIZESTART/END)
/// - Drag start/end (EVENT_SYSTEM_MOVESIZESTART/END)
/// - Move/resize (EVENT_OBJECT_LOCATIONCHANGE)
/// - Focus within app (EVENT_OBJECT_FOCUS)
pub fn install_event_hooks(
) -> Result<(EventHookHandle, mpsc::Receiver<WindowEvent>), crate::Win32Error> {
    // Create channel for events
    // Bound callback ingress. Lifecycle/focus edges use bounded backpressure;
    // high-frequency location/title noise is coalesced by dropping only when a
    // prior backlog already occupies the queue.
    let (tx, rx) = mpsc::sync_channel(512);

    // Store sender globally for callback access
    set_event_sender(tx)?;

    // Channel to receive init result from the dedicated thread
    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<u32, crate::Win32Error>>();

    let thread = std::thread::Builder::new()
        .name("winevent-pump".into())
        .spawn(move || {
            unsafe {
                let thread_id = GetCurrentThreadId();
                ACTIVE_EVENT_THREAD.store(thread_id, Ordering::Release);

                // Ensure message queue exists before installing hooks
                let mut msg = MSG::default();
                let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);

                // Define events to hook: (min_event, max_event)
                let event_ranges = [
                    (EVENT_OBJECT_CREATE, EVENT_OBJECT_HIDE),
                    (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
                    (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
                    (EVENT_SYSTEM_MOVESIZESTART, EVENT_SYSTEM_MOVESIZEEND),
                    (EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_LOCATIONCHANGE),
                    (EVENT_OBJECT_FOCUS, EVENT_OBJECT_FOCUS),
                    (EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_NAMECHANGE),
                ];

                let mut hooks = Vec::new();

                for (min_event, max_event) in event_ranges {
                    let hook = SetWinEventHook(
                        min_event,
                        max_event,
                        None,
                        Some(win_event_callback),
                        0,
                        0,
                        WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
                    );

                    if hook.is_invalid() {
                        for h in &hooks {
                            let _ = UnhookWinEvent(*h);
                        }
                        let _ = init_tx.send(Err(crate::Win32Error::HookInstallFailed(format!(
                            "SetWinEventHook failed for events {}-{}",
                            min_event, max_event
                        ))));
                        retire_event_thread(thread_id);
                        return;
                    }

                    hooks.push(hook);
                }

                tracing::info!("Installed {} WinEvent hooks", hooks.len());
                let _ = init_tx.send(Ok(thread_id));

                // Message pump — required for WINEVENT_OUTOFCONTEXT callbacks
                loop {
                    let ret = GetMessageW(&mut msg, None, 0, 0).0;
                    if ret <= 0 {
                        break;
                    }
                    if msg.message == WM_QUIT_WINEVENT_THREAD {
                        break;
                    }
                    let _ = DispatchMessageW(&msg);
                }

                // Clean up hooks
                for hook in &hooks {
                    if !UnhookWinEvent(*hook).as_bool() {
                        tracing::warn!("Failed to unhook WinEvent: {:?}", hook);
                    }
                }
                retire_event_thread(thread_id);
            }
        })
        .map_err(|e| {
            clear_event_sender();
            crate::Win32Error::HookInstallFailed(format!(
                "Failed to spawn winevent-pump thread: {}",
                e
            ))
        })?;

    // Wait for init result
    match init_rx.recv() {
        Ok(Ok(thread_id)) => Ok((
            EventHookHandle {
                thread_id,
                _thread: Some(thread),
            },
            rx,
        )),
        Ok(Err(e)) => {
            let _ = thread.join();
            clear_event_sender();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            clear_event_sender();
            Err(crate::Win32Error::HookInstallFailed(
                "WinEvent hook thread exited during init".to_string(),
            ))
        }
    }
}

/// Callback function for WinEvent hooks.
///
/// This runs on Windows' thread pool, so we forward events to the channel.
/// Wrapped with catch_unwind to prevent panics from crashing the application.
unsafe extern "system" fn win_event_callback(
    hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    id_event_thread: u32,
    dwms_event_time: u32,
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        win_event_callback_inner(
            hook,
            event,
            hwnd,
            id_object,
            id_child,
            id_event_thread,
            dwms_event_time,
        )
    }));

    if let Err(e) = result {
        // Can't use tracing here safely in all contexts, use eprintln
        eprintln!("Panic in win_event_callback: {:?}", e);
    }
}

/// Inner implementation of WinEvent callback.
fn win_event_callback_inner(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _id_event_thread: u32,
    _dwms_event_time: u32,
) {
    // Only handle window-level events (not child objects like menus).
    // Exception: EVENT_OBJECT_FOCUS fires with OBJID_CLIENT, and
    // EVENT_SYSTEM_* events always have id_object == 0, so we allow
    // focus/foreground events regardless of id_object.
    // EVENT_SYSTEM_FOREGROUND and EVENT_OBJECT_FOCUS fire with id_object == 0
    // or OBJID_CLIENT, so allow them regardless. But EVENT_OBJECT_SHOW/HIDE
    // must be OBJID_WINDOW only — child control visibility changes should not
    // be emitted as top-level window lifecycle events.
    let is_focus_event = matches!(event, EVENT_SYSTEM_FOREGROUND | EVENT_OBJECT_FOCUS);
    if id_object != OBJID_WINDOW && !is_focus_event {
        return;
    }

    // Ignore invalid HWNDs
    if hwnd.0.is_null() {
        return;
    }

    // For destroy/hide events, skip normalization — the window may already be gone,
    // and GetAncestor would return null. Use the HWND as-is.
    let hwnd = if matches!(event, EVENT_OBJECT_DESTROY | EVENT_OBJECT_HIDE) {
        hwnd
    } else {
        normalize_to_root_window(hwnd)
    };

    let window_id = hwnd.0 as WindowId;
    if event == EVENT_OBJECT_DESTROY {
        // Synchronous incarnation fence: preview input has a separate priority
        // lane and can otherwise overtake this queued destroy event.
        crate::thumbnail::invalidate_persistent_preview_source(window_id);
    }

    // Suppress LOCATIONCHANGE and SHOW events for windows currently cloaked
    // by our placement system. DWM cloaking fires EVENT_OBJECT_LOCATIONCHANGE
    // on both cloak and uncloak, which would cascade into snap-back loops.
    // SHOW fires on uncloak — harmless ("already managed") but noisy.
    // HIDE is NOT suppressed: cloaking doesn't fire HIDE, and real hide
    // events (minimize, close-to-tray) must reach the daemon.
    if matches!(event, EVENT_OBJECT_SHOW | EVENT_OBJECT_LOCATIONCHANGE)
        && (crate::is_placement_cloaked(window_id)
            || crate::thumbnail::has_persistent_preview(window_id))
    {
        return;
    }

    // Placement cloak/uncloak generates high-frequency LOCATIONCHANGE events.
    // Check the daemon-owned registry before the manageability policy, which
    // otherwise probes visibility, styles, DWM cloak state, title, and class
    // only to discard the event immediately afterward.
    if should_filter_window_event_by_manageability(event)
        && !should_emit_window_event_for(event, hwnd)
    {
        return;
    }

    // Map event to our WindowEvent type
    let window_event = match event {
        EVENT_OBJECT_CREATE | EVENT_OBJECT_SHOW => WindowEvent::Created(window_id),
        EVENT_OBJECT_DESTROY => WindowEvent::Destroyed(window_id),
        EVENT_OBJECT_HIDE => WindowEvent::Hidden(window_id),
        EVENT_SYSTEM_FOREGROUND => WindowEvent::Focused(window_id),
        EVENT_OBJECT_FOCUS => {
            // Only emit Focused for EVENT_OBJECT_FOCUS if the window is actually
            // the foreground window. This filters out spurious focus events from
            // Windows' "scroll inactive windows" feature — when the mouse wheel
            // is delivered to a non-foreground window (e.g., the other window in
            // a stacked column), some apps fire EVENT_OBJECT_FOCUS without the
            // window truly becoming foreground, causing the border to flicker.
            let fg = unsafe { GetForegroundWindow() };
            if fg != hwnd {
                return;
            }
            WindowEvent::Focused(window_id)
        }
        EVENT_SYSTEM_MINIMIZESTART => WindowEvent::Minimized(window_id),
        EVENT_SYSTEM_MINIMIZEEND => WindowEvent::Restored(window_id),
        EVENT_SYSTEM_MOVESIZESTART => WindowEvent::MoveSizeStart(window_id),
        EVENT_SYSTEM_MOVESIZEEND => WindowEvent::MoveSizeEnd(window_id),
        EVENT_OBJECT_LOCATIONCHANGE => WindowEvent::MovedOrResized(window_id),
        EVENT_OBJECT_NAMECHANGE => WindowEvent::TitleChanged(window_id),
        _ => return,
    };

    // Capture the callback's exact incarnation before the event can wait in a
    // forwarding queue. The daemon consumes the paired token on dispatch.
    let priority = !matches!(
        &window_event,
        WindowEvent::MovedOrResized(_) | WindowEvent::TitleChanged(_)
    );
    queue_window_event_identity(&window_event);
    if let Some(sender) = clone_event_sender() {
        match sender.try_send(window_event.clone()) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(event)) if priority => {
                // Never drop lifecycle/focus/drag-final edges. The finite queue
                // bounds memory; only a persistently stalled daemon can apply
                // backpressure to this callback.
                if sender.send(event).is_err() {
                    let _ = take_window_event_identity(&window_event);
                }
            }
            Err(mpsc::TrySendError::Full(_)) => {
                tracing::trace!("Coalescing high-frequency WinEvent backlog");
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                let _ = take_window_event_identity(&window_event);
            }
        }
    } else {
        let _ = take_window_event_identity(&window_event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static GLOBAL_SENDER_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_event_sender_can_be_reinstalled_after_clear() {
        let _guard = GLOBAL_SENDER_TEST_LOCK
            .lock()
            .unwrap_or_else(recover_poisoned_mutex);
        ACTIVE_EVENT_THREAD.store(0, Ordering::Release);
        clear_event_sender();

        let (first_tx, _first_rx) = mpsc::sync_channel::<WindowEvent>(4);
        assert!(set_event_sender(first_tx).is_ok());

        let (second_tx, _second_rx) = mpsc::sync_channel::<WindowEvent>(4);
        let err = set_event_sender(second_tx).unwrap_err();
        assert!(matches!(err, crate::Win32Error::HookInstallFailed(_)));

        clear_event_sender();

        let (third_tx, _third_rx) = mpsc::sync_channel::<WindowEvent>(4);
        assert!(set_event_sender(third_tx).is_ok());
        clear_event_sender();
    }

    #[test]
    fn stale_thread_cannot_clear_replacement_generation() {
        let _guard = GLOBAL_SENDER_TEST_LOCK
            .lock()
            .unwrap_or_else(recover_poisoned_mutex);
        clear_event_sender();
        let (tx, _rx) = mpsc::sync_channel(4);
        set_event_sender(tx).unwrap();
        ACTIVE_EVENT_THREAD.store(22, Ordering::Release);

        retire_event_thread(21);
        assert!(clone_event_sender().is_some());
        retire_event_thread(22);
        assert!(clone_event_sender().is_none());
    }

    #[test]
    fn queued_destroy_keeps_old_token_across_same_identity_reuse() {
        let window_id = 0x1234;
        let old = WindowEventIdentity {
            token: 41,
            process_id: 7,
            thread_id: 8,
            class_name: "SameClass".into(),
        };
        WINDOW_IDENTITIES
            .lock()
            .unwrap_or_else(recover_poisoned_mutex)
            .get_or_insert_with(HashMap::new)
            .insert(window_id, old.clone());

        let event = WindowEvent::Destroyed(window_id);
        queue_window_event_identity(&event);
        WINDOW_IDENTITIES
            .lock()
            .unwrap_or_else(recover_poisoned_mutex)
            .get_or_insert_with(HashMap::new)
            .insert(
                window_id,
                WindowEventIdentity {
                    token: 42,
                    ..old.clone()
                },
            );

        assert_eq!(take_window_event_identity(&event), Some(old));
        WINDOW_IDENTITIES
            .lock()
            .unwrap_or_else(recover_poisoned_mutex)
            .get_or_insert_with(HashMap::new)
            .remove(&window_id);
    }
}
