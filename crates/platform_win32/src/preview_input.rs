//! Click targets for monitor-edge previews.
//!
//! A preview is a DWM thumbnail: pixels only. The source HWND is parked clear
//! of every monitor, so a click on the strip cannot reach it the way it did when
//! the real window sat there under a `SetWindowRgn` clip. That lost the
//! scroll-first gesture of clicking a partially visible column to focus it.
//!
//! This module restores it with the same shape the tab strip already uses: a
//! tiny overlay per preview that drops `WS_EX_TRANSPARENT` so clicks land on it,
//! answers `WM_MOUSEACTIVATE` with `MA_NOACTIVATE` so clicking never activates
//! the overlay itself, and forwards the identity of the previewed window to the
//! daemon. Occlusion stays correct because these are real windows: anything
//! Windows puts above them keeps its own clicks.
//!
//! The overlays live on one dedicated thread with its own message pump. Callers
//! on the apply/animation worker only publish desired state through a channel,
//! so no window is ever created or moved from a thread that does not pump.

use crate::Win32Error;
use leopardwm_core_layout::{Rect, WindowId};
use std::collections::HashMap;
#[cfg(feature = "integration-probes")]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use tracing::{debug, warn};
use windows::Win32::Foundation::{
    GetLastError, COLORREF, ERROR_CLASS_ALREADY_EXISTS, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, ClientToScreen, CreateSolidBrush, DeleteObject, EndPaint, FillRect, PAINTSTRUCT,
};
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetCapture, ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE,
    TRACKMOUSEEVENT, VK_LBUTTON,
};
#[cfg(feature = "integration-probes")]
use windows::Win32::UI::WindowsAndMessaging::WM_QUIT;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClassInfoW, GetCursorPos,
    GetMessageW, GetWindow, GetWindowLongPtrW, GetWindowThreadProcessId, IsWindow, IsWindowVisible,
    KillTimer, LoadCursorW, PeekMessageW, PostThreadMessageW, RegisterClassW, SetCursor,
    SetLayeredWindowAttributes, SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    TranslateMessage, UnregisterClassW, GWLP_USERDATA, GW_HWNDNEXT, HTTRANSPARENT, HWND_TOP,
    IDC_HAND, LWA_ALPHA, MA_NOACTIVATE, MSG, PM_NOREMOVE, SM_CXDRAG, SM_CYDRAG, SWP_NOACTIVATE,
    SWP_NOZORDER, SW_HIDE, SW_SHOWNOACTIVATE, WM_APP, WM_CAPTURECHANGED, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_NCHITTEST, WM_PAINT, WM_SETCURSOR, WM_TIMER,
    WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};

const PREVIEW_TARGET_CLASS: &str = "LeopardWMPreviewClickTarget";
/// `MK_LBUTTON` from `WM_MOUSEMOVE`'s `wparam`. Declared here because the
/// `windows` crate exposes it behind a feature this crate does not need.
const MOUSE_MOVE_LBUTTON_DOWN: usize = 0x0001;
/// Hover wash colour in BGR, matching the default active-border blue.
const HOVER_WASH_BGR: u32 = 0x00F4_7E43;
/// Wakes the pump after new desired state has been queued.
const WM_PREVIEW_SYNC: u32 = WM_APP + 0x51;
/// Re-anchor existing overlays after an explicit tiled-window z-order raise.
const WM_PREVIEW_RAISE: u32 = WM_APP + 0x52;
const PRESS_TIMER_ID: usize = 0x4C57_5052; // "LWPR"
/// Nearly invisible, still hit-testable. Uniform-alpha layered windows keep
/// normal hit testing; only `WS_EX_TRANSPARENT` or per-pixel alpha of zero make
/// a window click-through, and 1/255 over a thumbnail is not perceivable.
const TARGET_ALPHA: u8 = 1;
/// Alpha while the pointer is over a preview. A preview is a still image of a
/// window that is not there, so nothing else signals that it can be clicked;
/// a light wash is the smallest honest affordance and doubles as the hit-test
/// boundary made visible.
const HOVER_ALPHA: u8 = 38;

/// One previewed window and the screen rectangle its thumbnail occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewClickTarget {
    pub window_id: WindowId,
    /// Process identity prevents a delayed press from targeting a newly-created
    /// window that reused the same numeric HWND.
    pub source_process_id: u32,
    pub publication_generation: u64,
    pub rect: Rect,
}

/// What the pointer did on a preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewGesture {
    /// Press and release without travelling: focus the column.
    Click,
    /// Press and drag past the system drag threshold: focus the column, then
    /// hand the pointer to the real window so Windows' own move loop takes over.
    Drag,
}

/// A pointer gesture on a preview. Carries the previewed window rather than a
/// screen point, so the daemon acts on the column the user actually saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewClickEvent {
    pub window_id: WindowId,
    pub source_process_id: u32,
    pub publication_generation: u64,
    pub preview_rect: Rect,
    pub gesture: PreviewGesture,
}

#[derive(Default)]
struct DesiredTargets {
    targets: Vec<PreviewClickTarget>,
    /// Monotonic intent generation. The pump publishes this into
    /// `applied_generation` only after every create/move/drop succeeded.
    generation: u64,
}

#[cfg_attr(test, allow(dead_code))]
struct PreviewInput {
    thread_id: u32,
    desired: Mutex<DesiredTargets>,
    applied_generation: AtomicU64,
    desired_raise_generation: AtomicU64,
    applied_raise_generation: AtomicU64,
    raise_host_raw: AtomicIsize,
    /// Window the host and its targets must stay below, normally the bottommost
    /// visible tiled HWND. Zero keeps the legacy band-top behavior for callers
    /// without an anchor.
    raise_anchor_raw: AtomicIsize,
    alive: Arc<AtomicBool>,
    thread_handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Drop for PreviewInput {
    fn drop(&mut self) {
        #[cfg(feature = "integration-probes")]
        LIVE_PREVIEW_INPUTS.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg_attr(test, allow(dead_code))]
static INPUT: Mutex<Option<Arc<PreviewInput>>> = Mutex::new(None);
/// Joined preview-input generations must release their state before a restart
/// publishes a replacement. The probe observes this directly instead of merely
/// proving that a new pump can be spawned.
#[cfg(feature = "integration-probes")]
static LIVE_PREVIEW_INPUTS: AtomicUsize = AtomicUsize::new(0);
/// Shared with the DWM host lifecycle. A nonzero value arms hit testing only
/// while it exactly matches the current preview lifecycle epoch.
static TARGETS_ARMED_EPOCH: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "integration-probes")]
static FORCE_RETAIN_CAPTURED_TARGET: AtomicU64 = AtomicU64::new(0);
static CLICK_SENDER: OnceLock<Mutex<Option<mpsc::Sender<PreviewClickEvent>>>> = OnceLock::new();

fn click_sender() -> &'static Mutex<Option<mpsc::Sender<PreviewClickEvent>>> {
    CLICK_SENDER.get_or_init(|| Mutex::new(None))
}

/// Route preview clicks to `sender`. Called once during daemon startup.
pub fn set_click_sender(sender: mpsc::Sender<PreviewClickEvent>) {
    *click_sender()
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex) = Some(sender);
}

/// Drop the click route so its forwarder thread can finish. Called during
/// shutdown: the receiver only ends when the last sender goes away, and a
/// forwarder blocked in `recv()` would otherwise consume the join budget.
pub fn clear_click_sender() {
    *click_sender()
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex) = None;
}

fn emit_gesture(target: PreviewClickTarget, gesture: PreviewGesture) {
    let window_id = target.window_id;
    let guard = click_sender()
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex);
    // A gesture that goes nowhere looks exactly like a dead preview to the user,
    // and both ways to lose it here are silent, so both are logged: the daemon
    // may not have installed a route yet, and its forwarder may have exited.
    let Some(sender) = guard.as_ref() else {
        warn!("Preview {gesture:?} on {window_id:#x} dropped: no click route installed");
        return;
    };
    match sender.send(PreviewClickEvent {
        window_id,
        source_process_id: target.source_process_id,
        publication_generation: target.publication_generation,
        preview_rect: target.rect,
        gesture,
    }) {
        Ok(()) => debug!("Preview {gesture:?} on {window_id:#x} sent"),
        Err(error) => warn!("Preview {gesture:?} on {window_id:#x} dropped: {error}"),
    }
}

/// Whether pointer travel since the press exceeds the system drag threshold.
///
/// `SM_CXDRAG`/`SM_CYDRAG` is the same threshold Explorer uses, so a click that
/// wobbles by a pixel stays a click.
pub(crate) fn travelled_past_drag_threshold(
    press: (i32, i32),
    current: (i32, i32),
    threshold: (i32, i32),
) -> bool {
    (current.0 - press.0).abs() > threshold.0.max(1)
        || (current.1 - press.1).abs() > threshold.1.max(1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PressMoveDecision {
    Continue,
    Cancel,
    Drag,
}

fn press_move_decision(
    origin: (i32, i32),
    current: (i32, i32),
    threshold: (i32, i32),
    handed_off: bool,
    message_button_down: bool,
    physical_button_down: bool,
) -> PressMoveDecision {
    if !message_button_down && !physical_button_down {
        PressMoveDecision::Cancel
    } else if !handed_off && travelled_past_drag_threshold(origin, current, threshold) {
        PressMoveDecision::Drag
    } else {
        PressMoveDecision::Continue
    }
}

fn record_desired_targets(desired: &mut DesiredTargets, targets: &[PreviewClickTarget]) -> u64 {
    if desired.targets.as_slice() != targets {
        desired.targets = targets.to_vec();
        desired.generation = desired.generation.wrapping_add(1).max(1);
    }
    desired.generation
}

fn generation_needs_reconcile(applied: u64, desired: u64) -> bool {
    applied != desired
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GenerationAckState {
    Exact,
    Pending,
    Superseded,
}

fn generation_ack_state(applied: u64, desired: u64, requested: u64) -> GenerationAckState {
    if applied == requested {
        GenerationAckState::Exact
    } else if desired != requested {
        GenerationAckState::Superseded
    } else {
        GenerationAckState::Pending
    }
}

/// Reconcile the overlays with the previews that are currently published.
///
/// Safe to call from any thread and on every applied frame: the desired state is
/// stored and the owning thread performs the window operations.
pub fn sync_preview_click_targets(targets: &[PreviewClickTarget]) -> Option<u64> {
    // Empty desired state must not make ordinary tiling depend on starting an
    // optional input pump. If no live pump exists, there can be no owned target
    // HWNDs to remove; generation zero is an already-applied empty receipt.
    #[cfg(not(test))]
    if targets.is_empty() {
        let mut slot = INPUT.lock().unwrap_or_else(crate::recover_poisoned_mutex);
        match slot.as_ref() {
            Some(input) if input.alive.load(Ordering::Acquire) => {}
            Some(_) => {
                *slot = None;
                return Some(0);
            }
            None => return Some(0),
        }
    }
    let input = input()?;
    let generation = {
        let mut desired = input
            .desired
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex);
        record_desired_targets(&mut desired, targets)
    };
    // Deduplicate only a state the owning thread has actually reconciled. A
    // failed/lost wake leaves `applied_generation` behind, so an identical next
    // publish retries instead of becoming a permanent no-op (including clear).
    if generation_needs_reconcile(input.applied_generation.load(Ordering::Acquire), generation) {
        post_sync(&input);
    }
    Some(generation)
}

/// Wait until the owning window thread has fully reconciled a requested
/// generation. Used only at exact landings/retries, never on every animation
/// frame, so input and pixels can be acknowledged as one settled surface.
pub fn wait_for_applied_generation(generation: u64, timeout: std::time::Duration) -> bool {
    if generation == 0 {
        return true;
    }
    let Some(input) = input() else {
        return false;
    };
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let applied = input.applied_generation.load(Ordering::Acquire);
        let desired = input
            .desired
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex)
            .generation;
        match generation_ack_state(applied, desired, generation) {
            GenerationAckState::Exact => return true,
            GenerationAckState::Superseded => return false,
            GenerationAckState::Pending => {}
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// Drop every overlay, e.g. when the last preview disappears or on shutdown.
pub fn clear_preview_click_targets() {
    TARGETS_ARMED_EPOCH.store(0, Ordering::Release);
    let _ = sync_preview_click_targets(&[]);
}

pub(crate) fn set_preview_targets_armed(armed: bool) {
    let epoch = if armed {
        crate::thumbnail::preview_lifecycle_epoch()
    } else {
        0
    };
    TARGETS_ARMED_EPOCH.store(epoch, Ordering::Release);
}

pub(crate) fn set_preview_targets_armed_for_epoch(epoch: u64) {
    TARGETS_ARMED_EPOCH.store(epoch.max(1), Ordering::Release);
}

fn targets_are_armed_for_lifecycle(armed_epoch: u64, lifecycle_epoch: u64) -> bool {
    armed_epoch != 0 && armed_epoch == lifecycle_epoch
}

/// Drop the overlays whose window is not in `keep`, leaving the rest in place.
///
/// Used when a single preview disappears: the surviving previews are still on
/// screen, so taking their overlays away would make them look dead until some
/// later frame republished them.
pub fn retain_preview_click_targets(keep: &[WindowId]) {
    let Some(input) = input() else {
        return;
    };
    let remaining: Vec<PreviewClickTarget> = {
        let desired = input
            .desired
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex);
        desired
            .targets
            .iter()
            .copied()
            .filter(|target| keep.contains(&target.window_id))
            .collect()
    };
    let _ = sync_preview_click_targets(&remaining);
}

/// Order the live overlays against the host and, when given, the band anchor.
///
/// Returns an exact generation that is acknowledged only after every target
/// accepted its z-order update. `anchor_raw` is the window the whole preview
/// group must stay below: the group would otherwise be ordered to the top of
/// the normal band, where it paints over and steals input from windows that own
/// those pixels.
pub fn raise_preview_click_targets(host_raw: isize, anchor_raw: isize) -> Option<u64> {
    let input = input()?;
    input.raise_host_raw.store(host_raw, Ordering::Release);
    input.raise_anchor_raw.store(anchor_raw, Ordering::Release);
    let generation = input
        .desired_raise_generation
        .fetch_add(1, Ordering::AcqRel)
        .wrapping_add(1)
        .max(1);
    if let Err(error) =
        unsafe { PostThreadMessageW(input.thread_id, WM_PREVIEW_RAISE, WPARAM(0), LPARAM(0)) }
    {
        warn!("Preview click target z-order request failed: {error}");
        return None;
    }
    Some(generation)
}

#[cfg(feature = "integration-probes")]
pub(crate) fn integration_probe_force_retained_capture(window_id: Option<WindowId>) {
    FORCE_RETAIN_CAPTURED_TARGET.store(window_id.unwrap_or(0), Ordering::Release);
}

#[cfg(feature = "integration-probes")]
pub fn integration_probe_restart_input_pump() -> bool {
    let first_target = PreviewClickTarget {
        window_id: 0x7fff_0001,
        source_process_id: std::process::id(),
        publication_generation: 1,
        rect: Rect::new(10, 10, 20, 20),
    };
    let Some(first_generation) = sync_preview_click_targets(&[first_target]) else {
        return false;
    };
    if !wait_for_applied_generation(first_generation, std::time::Duration::from_secs(2)) {
        return false;
    }
    let (old_thread_id, old_alive) = {
        let slot = INPUT.lock().unwrap_or_else(crate::recover_poisoned_mutex);
        let Some(input) = slot.as_ref() else {
            return false;
        };
        (input.thread_id, input.alive.clone())
    };
    let _ = unsafe { PostThreadMessageW(old_thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while old_alive.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if old_alive.load(Ordering::Acquire) {
        return false;
    }

    let second_target = PreviewClickTarget {
        window_id: 0x7fff_0002,
        publication_generation: 2,
        ..first_target
    };
    let Some(second_generation) = sync_preview_click_targets(&[second_target]) else {
        return false;
    };
    if !wait_for_applied_generation(second_generation, std::time::Duration::from_secs(2)) {
        return false;
    }
    let new_thread_id = INPUT
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
        .as_ref()
        .map(|input| input.thread_id);
    let clear_generation = sync_preview_click_targets(&[]).unwrap_or(0);
    let cleared = wait_for_applied_generation(clear_generation, std::time::Duration::from_secs(2));
    cleared
        && new_thread_id.is_some_and(|thread_id| thread_id != old_thread_id)
        && LIVE_PREVIEW_INPUTS.load(Ordering::Acquire) == 1
}

/// Outcome of waiting for one exact z-order generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaiseAck {
    /// The pump ordered and verified this exact generation.
    Applied,
    /// A newer publication replaced this request. That newer pass owns the
    /// surface, so this one must stop without tearing the surface down.
    Superseded,
    /// The pump neither applied nor replaced it within the timeout.
    NotAcknowledged,
}

pub fn wait_for_applied_raise_generation(
    generation: u64,
    timeout: std::time::Duration,
) -> RaiseAck {
    let Some(input) = input() else {
        return RaiseAck::NotAcknowledged;
    };
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if input.applied_raise_generation.load(Ordering::Acquire) == generation {
            return RaiseAck::Applied;
        }
        if input.desired_raise_generation.load(Ordering::Acquire) != generation {
            return RaiseAck::Superseded;
        }
        if std::time::Instant::now() >= deadline {
            return RaiseAck::NotAcknowledged;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

fn post_sync(input: &PreviewInput) {
    if let Err(error) =
        unsafe { PostThreadMessageW(input.thread_id, WM_PREVIEW_SYNC, WPARAM(0), LPARAM(0)) }
    {
        // Keep desired/applied generations different. Every future publish will
        // retry, and a mouse message already queued to an old overlay also runs a
        // reconcile directly after dispatch in the owning pump.
        warn!("Preview click target sync could not be posted: {error}");
    }
}

fn input() -> Option<Arc<PreviewInput>> {
    #[cfg(test)]
    {
        None
    }
    #[cfg(not(test))]
    {
        let mut slot = INPUT.lock().unwrap_or_else(crate::recover_poisoned_mutex);
        if let Some(input) = slot.as_ref() {
            if input.alive.load(Ordering::Acquire) {
                return Some(Arc::clone(input));
            }
            warn!("Preview input pump exited; restarting");
            if let Some(handle) = input
                .thread_handle
                .lock()
                .unwrap_or_else(crate::recover_poisoned_mutex)
                .take()
            {
                let _ = handle.join();
            }
            // Replacing the Arc releases the entire retired generation once
            // this local borrow ends; unlike Box::leak it cannot accumulate
            // mutexes, desired vectors, and lifecycle atomics across restarts.
            *slot = None;
        }
        match PreviewInput::spawn() {
            Ok(input) => {
                let input = Arc::new(input);
                *slot = Some(Arc::clone(&input));
                Some(input)
            }
            Err(error) => {
                warn!("Preview click targets unavailable: {error}");
                None
            }
        }
    }
}

/// Whether `above` precedes `below` in the same z-order band.
pub(crate) unsafe fn window_is_above(above: HWND, below: HWND) -> bool {
    let mut cursor = above;
    // EnumWindows-sized top-level chains are finite; the bound also protects
    // against a corrupted/recycled HWND producing an unexpected cycle.
    for _ in 0..16_384 {
        let Ok(next) = GetWindow(cursor, GW_HWNDNEXT) else {
            return false;
        };
        if next == below {
            return true;
        }
        if next.is_invalid() {
            return false;
        }
        cursor = next;
    }
    false
}

impl PreviewInput {
    #[cfg_attr(test, allow(dead_code))]
    fn spawn() -> Result<Self, Win32Error> {
        let (tx, rx) = mpsc::channel::<Result<u32, Win32Error>>();
        // The worker must receive this second acknowledgement before entering
        // its unbounded message loop. A caller readiness timeout therefore
        // closes the channel and makes the late worker exit instead of orphaning.
        let (startup_ack_tx, startup_ack_rx) = mpsc::channel::<()>();
        let alive = Arc::new(AtomicBool::new(true));
        let thread_alive = alive.clone();
        let startup_cancel = Arc::new(AtomicBool::new(false));
        let thread_startup_cancel = startup_cancel.clone();
        let thread_handle = std::thread::Builder::new()
            .name("leopardwm-preview-input".into())
            .spawn(move || unsafe {
                let class: Vec<u16> = format!("{PREVIEW_TARGET_CLASS}\0").encode_utf16().collect();
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(target_proc),
                    lpszClassName: windows::core::PCWSTR(class.as_ptr()),
                    ..Default::default()
                };
                if RegisterClassW(&wc) == 0 {
                    let last_error = GetLastError();
                    let mut existing = WNDCLASSW::default();
                    let compatible_existing = last_error == ERROR_CLASS_ALREADY_EXISTS
                        && GetClassInfoW(
                            None,
                            windows::core::PCWSTR(class.as_ptr()),
                            &mut existing,
                        )
                        .is_ok()
                        && existing.lpfnWndProc.map(|proc| proc as usize)
                            == wc.lpfnWndProc.map(|proc| proc as usize);
                    if !compatible_existing {
                        let error = windows::core::Error::from_thread();
                        let _ = tx.send(Err(Win32Error::SetPositionFailed(format!(
                            "preview input class registration: {error}"
                        ))));
                        thread_alive.store(false, Ordering::Release);
                        return;
                    }
                }

                let mut windows_by_id: HashMap<WindowId, HWND> = HashMap::new();
                let mut message = MSG::default();
                // `PostThreadMessageW` fails until the destination thread has a
                // message queue. Create it before publishing the thread id; the
                // previous ordering left a startup race that could lose the first
                // preview sync for the whole settled layout.
                let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
                if thread_startup_cancel.load(Ordering::Acquire) {
                    thread_alive.store(false, Ordering::Release);
                    return;
                }
                if tx
                    .send(Ok(windows::Win32::System::Threading::GetCurrentThreadId()))
                    .is_err()
                    || startup_ack_rx
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .is_err()
                {
                    thread_alive.store(false, Ordering::Release);
                    return;
                }
                // `GetMessageW` returns -1 on error, which `as_bool()` reports as
                // true; spinning on that would peg a core, so stop on anything
                // that is not a real message.
                loop {
                    let result = GetMessageW(&mut message, None, 0, 0).0;
                    if result <= 0 {
                        if result < 0 {
                            warn!("Preview input message pump failed");
                        }
                        break;
                    }
                    if message.message == WM_PREVIEW_SYNC {
                        reconcile_pending(&class, &mut windows_by_id);
                        continue;
                    }
                    if message.message == WM_PREVIEW_RAISE {
                        let Some(input) = input() else {
                            continue;
                        };
                        let host_raw = input.raise_host_raw.load(Ordering::Acquire);
                        if host_raw == 0 || windows_by_id.is_empty() {
                            continue;
                        }
                        let host = HWND(host_raw as *mut std::ffi::c_void);
                        let anchor_raw = input.raise_anchor_raw.load(Ordering::Acquire);
                        let anchor = Some(HWND(anchor_raw as *mut std::ffi::c_void))
                            .filter(|_| anchor_raw != 0)
                            .filter(|anchor| IsWindow(Some(*anchor)).as_bool());
                        // Reference only our own windows: UIPI refuses a
                        // higher-integrity window as the z-order reference, and
                        // an absolute HWND_TOP would lift the whole group over
                        // the windows that own those pixels. Inserting every
                        // target directly below the host and then moving the
                        // host below the deepest target reorders the group in
                        // place, wherever the host was anchored.
                        // Each insert lands directly below the host, so the
                        // first one ends up deepest; the host must then move
                        // below that deepest target, not the last inserted one.
                        let mut deepest_target = None;
                        let mut raised = true;
                        for hwnd in windows_by_id.values() {
                            if let Err(error) = SetWindowPos(
                                *hwnd,
                                Some(host),
                                0,
                                0,
                                0,
                                0,
                                SWP_NOACTIVATE
                                    | windows::Win32::UI::WindowsAndMessaging::SWP_NOMOVE
                                    | windows::Win32::UI::WindowsAndMessaging::SWP_NOSIZE,
                            ) {
                                raised = false;
                                warn!("Preview click target z-order failed: {error}");
                                break;
                            }
                            deepest_target.get_or_insert(*hwnd);
                        }
                        if raised {
                            if let Err(error) = SetWindowPos(
                                host,
                                Some(deepest_target.unwrap_or(host)),
                                0,
                                0,
                                0,
                                0,
                                SWP_NOACTIVATE
                                    | windows::Win32::UI::WindowsAndMessaging::SWP_NOMOVE
                                    | windows::Win32::UI::WindowsAndMessaging::SWP_NOSIZE,
                            ) {
                                raised = false;
                                warn!("Preview host/target relative z-order failed: {error}");
                            } else if !windows_by_id
                                .values()
                                .all(|target| window_is_above(*target, host))
                            {
                                raised = false;
                                warn!("Preview host/target z-order verification failed");
                            } else if anchor.is_some_and(|anchor| !window_is_above(anchor, host)) {
                                raised = false;
                                warn!("Preview group did not stay below its band anchor");
                            }
                        }
                        if raised {
                            let generation = input.desired_raise_generation.load(Ordering::Acquire);
                            input
                                .applied_raise_generation
                                .store(generation, Ordering::Release);
                        }
                        continue;
                    }
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                    // A release/capture-change may have made a deferred drop safe.
                    // Reconcile on the owning thread immediately after dispatch;
                    // it must not depend on another fallible posted wake.
                    reconcile_pending(&class, &mut windows_by_id);
                }
                // Never leave an invisible topmost window behind: without this
                // an exiting pump would strand click-absorbing overlays for the
                // rest of the session.
                for hwnd in windows_by_id.values() {
                    destroy_target(*hwnd);
                }
                let _ = UnregisterClassW(windows::core::PCWSTR(class.as_ptr()), None);
                thread_alive.store(false, Ordering::Release);
            })
            .map_err(|error| {
                Win32Error::SetPositionFailed(format!("preview input thread: {error}"))
            })?;

        let thread_id = match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok(thread_id)) => thread_id,
            Ok(Err(error)) => {
                let _ = thread_handle.join();
                return Err(error);
            }
            Err(error) => {
                startup_cancel.store(true, Ordering::Release);
                drop(startup_ack_tx);
                let _ = thread_handle.join();
                return Err(Win32Error::SetPositionFailed(format!(
                    "preview input thread readiness: {error}"
                )));
            }
        };
        if startup_ack_tx.send(()).is_err() {
            let _ = thread_handle.join();
            return Err(Win32Error::SetPositionFailed(
                "preview input startup acknowledgement failed".into(),
            ));
        }
        let input = Self {
            thread_id,
            desired: Mutex::new(DesiredTargets::default()),
            applied_generation: AtomicU64::new(0),
            desired_raise_generation: AtomicU64::new(0),
            applied_raise_generation: AtomicU64::new(0),
            raise_host_raw: AtomicIsize::new(0),
            raise_anchor_raw: AtomicIsize::new(0),
            alive,
            thread_handle: Mutex::new(Some(thread_handle)),
        };
        #[cfg(feature = "integration-probes")]
        LIVE_PREVIEW_INPUTS.fetch_add(1, Ordering::AcqRel);
        Ok(input)
    }
}

/// Which overlays to create, move, or drop for `desired`, given what exists.
///
/// Pure so the lifecycle is testable without a message pump.
pub(crate) fn reconcile_plan(
    existing: &[WindowId],
    desired: &[PreviewClickTarget],
) -> (
    Vec<PreviewClickTarget>,
    Vec<PreviewClickTarget>,
    Vec<WindowId>,
) {
    let mut create = Vec::new();
    let mut update = Vec::new();
    for target in desired {
        if existing.contains(&target.window_id) {
            update.push(*target);
        } else {
            create.push(*target);
        }
    }
    let drop = existing
        .iter()
        .copied()
        .filter(|window_id| !desired.iter().any(|target| target.window_id == *window_id))
        .collect();
    (create, update, drop)
}

#[cfg_attr(test, allow(dead_code))]
fn reconcile_pending(class: &[u16], windows_by_id: &mut HashMap<WindowId, HWND>) {
    let Some(input) = input() else {
        return;
    };
    let (desired, generation) = {
        let desired = input
            .desired
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex);
        (desired.targets.clone(), desired.generation)
    };
    if input.applied_generation.load(Ordering::Acquire) == generation {
        return;
    }
    if unsafe { reconcile_targets(class, windows_by_id, &desired) } {
        input
            .applied_generation
            .store(generation, Ordering::Release);
    }
}

#[cfg_attr(test, allow(dead_code))]
unsafe fn reconcile_targets(
    class: &[u16],
    windows_by_id: &mut HashMap<WindowId, HWND>,
    desired: &[PreviewClickTarget],
) -> bool {
    let existing: Vec<WindowId> = windows_by_id.keys().copied().collect();
    let (create, update, drop) = reconcile_plan(&existing, desired);
    let mut complete = true;

    // A background capture can miss both button-up and capture-changed after the
    // pointer leaves our windows. Never let that stale state protect an obsolete
    // overlay forever. Take the state before ReleaseCapture: the resulting
    // WM_CAPTURECHANGED is synchronous and may re-enter the WndProc.
    let mut pressed = PRESS.with(|press| press.borrow().as_ref().map(|state| state.hwnd));
    if pressed.is_some() && !left_button_is_down() {
        let ended = PRESS.with(|press| press.borrow_mut().take());
        unsafe {
            if let Some(ref ended) = ended {
                let _ = KillTimer(Some(ended.hwnd), PRESS_TIMER_ID);
            }
            let _ = ReleaseCapture();
        }
        pressed = None;
    }
    #[cfg(feature = "integration-probes")]
    {
        let forced = FORCE_RETAIN_CAPTURED_TARGET.load(Ordering::Acquire);
        if forced != 0 {
            pressed = windows_by_id.get(&forced).copied();
        }
    }

    for window_id in drop {
        let Some(&hwnd) = windows_by_id.get(&window_id) else {
            continue;
        };
        // Destroying the capture owner erases the gesture. Leave this generation
        // dirty and retry immediately after the release has been dispatched.
        if pressed == Some(hwnd) {
            complete = false;
            continue;
        }
        windows_by_id.remove(&window_id);
        unsafe { destroy_target(hwnd) };
    }

    for target in create {
        complete &= unsafe { create_target(class, target, windows_by_id) };
    }

    for target in update {
        let Some(&hwnd) = windows_by_id.get(&target.window_id) else {
            complete = false;
            continue;
        };
        // A destroyed overlay would stay recorded forever unless recreated.
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            windows_by_id.remove(&target.window_id);
            complete &= unsafe { create_target(class, target, windows_by_id) };
            continue;
        }
        unsafe {
            if target.rect.width <= 0 || target.rect.height <= 0 {
                let _ = ShowWindow(hwnd, SW_HIDE);
                continue;
            }
            if SetWindowPos(
                hwnd,
                None,
                target.rect.x,
                target.rect.y,
                target.rect.width,
                target.rect.height,
                SWP_NOACTIVATE | SWP_NOZORDER,
            )
            .is_err()
            {
                // Recreating the capture owner mid-press would swallow the click.
                // Keep this generation dirty and retry after the press instead.
                if pressed == Some(hwnd) {
                    complete = false;
                    continue;
                }
                windows_by_id.remove(&target.window_id);
                destroy_target(hwnd);
                complete &= create_target(class, target, windows_by_id);
                continue;
            }
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            if !IsWindowVisible(hwnd).as_bool() {
                complete = false;
            } else {
                TARGET_METADATA.with(|targets| {
                    targets.borrow_mut().insert(hwnd.0 as isize, target);
                });
            }
        }
    }
    complete
}

/// Create one overlay and record it, or forget the desired entry so the next
/// publish retries instead of being deduplicated into a no-op.
#[cfg_attr(test, allow(dead_code))]
unsafe fn create_target(
    class: &[u16],
    target: PreviewClickTarget,
    windows_by_id: &mut HashMap<WindowId, HWND>,
) -> bool {
    if target.rect.width <= 0 || target.rect.height <= 0 {
        return true;
    }
    // Defensive: a tracked HWND for this id would be orphaned by the insert
    // below and could never be destroyed again.
    if let Some(stale) = windows_by_id.remove(&target.window_id) {
        unsafe { destroy_target(stale) };
    }
    let created = unsafe {
        CreateWindowExW(
            // Normal band, deliberately not topmost: a preview stands in for
            // a tiled window, and a floating window sits above the tiled
            // layer. In the topmost band this overlay would take clicks that
            // belong to a float covering the strip.
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            windows::core::PCWSTR(class.as_ptr()),
            None,
            WS_POPUP,
            target.rect.x,
            target.rect.y,
            target.rect.width,
            target.rect.height,
            None,
            None,
            None,
            None,
        )
    };
    match created {
        Ok(hwnd) => unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, target.window_id as isize);
            let initialized = window_id_of(hwnd) == target.window_id
                && SetLayeredWindowAttributes(hwnd, COLORREF(0), TARGET_ALPHA, LWA_ALPHA).is_ok()
                && SetWindowPos(
                    hwnd,
                    Some(HWND_TOP),
                    target.rect.x,
                    target.rect.y,
                    target.rect.width,
                    target.rect.height,
                    SWP_NOACTIVATE,
                )
                .is_ok();
            if initialized {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            }
            if !initialized || !IsWindowVisible(hwnd).as_bool() {
                warn!(
                    "Preview click target {:#x} could not be initialized at {:?}",
                    target.window_id, target.rect
                );
                destroy_target(hwnd);
                false
            } else {
                TARGET_METADATA.with(|targets| {
                    targets.borrow_mut().insert(hwnd.0 as isize, target);
                });
                windows_by_id.insert(target.window_id, hwnd);
                true
            }
        },
        Err(error) => {
            warn!("Preview click target creation failed: {error}");
            false
        }
    }
}

unsafe fn destroy_target(hwnd: HWND) {
    TARGET_METADATA.with(|targets| {
        targets.borrow_mut().remove(&(hwnd.0 as isize));
    });
    // A dead HWND left in HOVERED can be recycled for a future overlay. That
    // overlay would then look already hovered, skip TrackMouseEvent, and keep a
    // stale wash forever. Clear/unwash ownership before destruction.
    HOVERED.with(|hovered| {
        if hovered.get() == hwnd.0 as isize {
            hovered.set(0);
        }
    });
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

/// Pointer state of the press in progress, in screen coordinates. Only the
/// overlay thread touches this, so a plain `RefCell` is enough.
struct PressState {
    /// The exact publication visible at button-down. It remains authoritative if
    /// the overlay is moved or destroyed before release.
    hwnd: HWND,
    target: PreviewClickTarget,
    origin: (i32, i32),
    drag_threshold: (i32, i32),
    handed_off: bool,
}

thread_local! {
    static PRESS: std::cell::RefCell<Option<PressState>> = const { std::cell::RefCell::new(None) };
    static TARGET_METADATA: std::cell::RefCell<HashMap<isize, PreviewClickTarget>> =
        std::cell::RefCell::new(HashMap::new());
}

thread_local! {
    /// The overlay currently washed, so the wash is pushed on transitions only.
    /// Re-pushing `SetLayeredWindowAttributes` on every pointer move makes
    /// Windows post synthetic `WM_MOUSEMOVE`s under a stationary cursor, and
    /// those carry no button state, which used to look like a released button.
    static HOVERED: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
}

/// Turn the hover wash on or off, arming `WM_MOUSELEAVE` the first time the
/// pointer arrives so the wash cannot get stuck on.
fn set_hover(hwnd: HWND, hovering: bool) {
    let raw = hwnd.0 as isize;
    let previous = HOVERED.with(|hovered| hovered.get());
    if hovering {
        if previous == raw {
            return;
        }
        // If Windows delivered enter on B before leave on A, un-wash A first.
        if previous != 0 {
            let old = HWND(previous as *mut std::ffi::c_void);
            if unsafe { IsWindow(Some(old)) }.as_bool() {
                unsafe {
                    let _ = SetLayeredWindowAttributes(old, COLORREF(0), TARGET_ALPHA, LWA_ALPHA);
                }
            }
        }
        HOVERED.with(|hovered| hovered.set(raw));
        unsafe {
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), HOVER_ALPHA, LWA_ALPHA);
        }
    } else {
        // A delayed leave for A must always un-wash A, even if B has already
        // become the tracked hover. Only clear ownership when A still owns it.
        unsafe {
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), TARGET_ALPHA, LWA_ALPHA);
        }
        if previous == raw {
            HOVERED.with(|hovered| hovered.set(0));
        }
        return;
    }
    if hovering {
        let mut track = TRACKMOUSEEVENT {
            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: hwnd,
            dwHoverTime: 0,
        };
        unsafe {
            let _ = TrackMouseEvent(&mut track);
        }
    }
}

fn window_id_of(hwnd: HWND) -> WindowId {
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as WindowId }
}

fn target_of(hwnd: HWND) -> Option<PreviewClickTarget> {
    TARGET_METADATA.with(|targets| targets.borrow().get(&(hwnd.0 as isize)).copied())
}

fn source_process_still_matches(target: PreviewClickTarget) -> bool {
    let source = HWND(target.window_id as *mut std::ffi::c_void);
    if !unsafe { IsWindow(Some(source)) }.as_bool() {
        return false;
    }
    let mut process_id = 0u32;
    unsafe {
        GetWindowThreadProcessId(source, Some(&mut process_id));
    }
    // Geometry may legitimately move while held, but the registration token
    // must still own an anchored publication. PID alone cannot distinguish a
    // same-process numeric HWND reuse.
    process_id != 0
        && process_id == target.source_process_id
        && crate::thumbnail::current_persistent_preview_rect(
            target.window_id,
            target.source_process_id,
            target.publication_generation,
        )
        .is_some()
}

/// The real left-button state, independent of what a message claims.
fn left_button_is_down() -> bool {
    // High bit set means down. Swapped buttons are irrelevant: `VK_LBUTTON` is
    // the primary button, which is the one that produced `WM_LBUTTONDOWN`.
    unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 & 0x8000 != 0 }
}

fn drag_threshold_for_window(hwnd: HWND) -> (i32, i32) {
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    unsafe {
        (
            GetSystemMetricsForDpi(SM_CXDRAG, dpi).max(1),
            GetSystemMetricsForDpi(SM_CYDRAG, dpi).max(1),
        )
    }
}

fn cursor_screen_point(hwnd: HWND, lparam: LPARAM) -> Option<(i32, i32)> {
    let mut point = POINT::default();
    if unsafe { GetCursorPos(&mut point) }.is_ok() {
        return Some((point.x, point.y));
    }
    // Mouse lparam is client-relative. Convert it rather than mixing it with the
    // screen-space press origin, which would turn a tiny wobble into a huge drag
    // on a monitor far from the virtual origin.
    let packed = lparam.0 as u32;
    point.x = (packed & 0xFFFF) as i16 as i32;
    point.y = (packed >> 16) as i16 as i32;
    unsafe { ClientToScreen(hwnd, &mut point) }
        .as_bool()
        .then_some((point.x, point.y))
}

unsafe extern "system" fn target_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // Interacting with a preview must not activate this overlay, or the pointer
    // would steal focus from the window the daemon is about to focus.
    if message == WM_MOUSEACTIVATE {
        return LRESULT(MA_NOACTIVATE as isize);
    }
    if message == WM_NCHITTEST {
        let armed_epoch = TARGETS_ARMED_EPOCH.load(Ordering::Acquire);
        // Validate the exact publication incarnation, not merely the numeric
        // HWND. A retained capture target must stay transparent even if a fresh
        // registration later supersedes its destroy tombstone.
        let target_is_current = target_of(hwnd).is_some_and(source_process_still_matches);
        if !target_is_current
            || !targets_are_armed_for_lifecycle(
                armed_epoch,
                crate::thumbnail::preview_lifecycle_epoch(),
            )
        {
            return LRESULT(HTTRANSPARENT as isize);
        }
    }

    match message {
        WM_PAINT => {
            // The wash is only visible while hovering, because the window's
            // uniform alpha is 1/255 otherwise.
            let mut paint = PAINTSTRUCT::default();
            let hdc = unsafe { BeginPaint(hwnd, &mut paint) };
            if !hdc.is_invalid() {
                let brush = unsafe { CreateSolidBrush(COLORREF(HOVER_WASH_BGR)) };
                if !brush.is_invalid() {
                    unsafe {
                        let _ = FillRect(hdc, &paint.rcPaint, brush);
                        let _ = DeleteObject(brush.into());
                    }
                }
                unsafe {
                    let _ = EndPaint(hwnd, &paint);
                }
            }
            LRESULT(0)
        }
        WM_SETCURSOR => {
            // A hand says "this is a control", which is exactly what a preview
            // is: clicking focuses its column.
            if let Ok(cursor) = unsafe { LoadCursorW(None, IDC_HAND) } {
                unsafe { SetCursor(Some(cursor)) };
                return LRESULT(1);
            }
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
        WM_MOUSELEAVE => {
            set_hover(hwnd, false);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let Some(target) = target_of(hwnd) else {
                warn!("Preview press dropped: overlay {hwnd:?} has no publication metadata");
                return LRESULT(0);
            };
            let window_id = target.window_id;
            debug!(
                "Preview Down on {window_id:#x} generation {} rect {:?}",
                target.publication_generation, target.rect
            );
            if !source_process_still_matches(target) {
                warn!("Preview press dropped: source identity for {window_id:#x} is stale");
                return LRESULT(0);
            }
            // Capture is best-effort for a non-activating background overlay; the
            // state below therefore remains authoritative across neighbouring
            // overlays and the physical button state closes a lost release.
            unsafe { SetCapture(hwnd) };
            if unsafe { GetCapture() } != hwnd {
                warn!("Preview press on {window_id:#x}: mouse capture was not acquired");
            }
            let Some(origin) = cursor_screen_point(hwnd, lparam) else {
                unsafe {
                    let _ = ReleaseCapture();
                }
                warn!("Preview press on {window_id:#x} dropped: cursor coordinates unavailable");
                return LRESULT(0);
            };
            PRESS.with(|press| {
                *press.borrow_mut() = Some(PressState {
                    hwnd,
                    target,
                    origin,
                    drag_threshold: drag_threshold_for_window(hwnd),
                    handed_off: false,
                });
            });
            let timer = unsafe { SetTimer(Some(hwnd), PRESS_TIMER_ID, 50, None) };
            if timer == 0 {
                PRESS.with(|press| *press.borrow_mut() = None);
                unsafe {
                    let _ = ReleaseCapture();
                }
                warn!("Preview press on {window_id:#x} cancelled: release timer creation failed");
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            set_hover(hwnd, true);
            let Some(current_point) = cursor_screen_point(hwnd, lparam) else {
                let ended = PRESS.with(|press| press.borrow_mut().take());
                unsafe {
                    if let Some(ref ended) = ended {
                        let _ = KillTimer(Some(ended.hwnd), PRESS_TIMER_ID);
                    }
                    let _ = ReleaseCapture();
                }
                return LRESULT(0);
            };
            let decision = PRESS.with(|press| {
                press.borrow().as_ref().map(|state| {
                    press_move_decision(
                        state.origin,
                        current_point,
                        state.drag_threshold,
                        state.handed_off,
                        wparam.0 & MOUSE_MOVE_LBUTTON_DOWN != 0,
                        left_button_is_down(),
                    )
                })
            });
            match decision {
                Some(PressMoveDecision::Cancel) => {
                    let ended = PRESS.with(|press| press.borrow_mut().take());
                    unsafe {
                        if let Some(ref ended) = ended {
                            let _ = KillTimer(Some(ended.hwnd), PRESS_TIMER_ID);
                        }
                        let _ = ReleaseCapture();
                    }
                }
                Some(PressMoveDecision::Drag) => {
                    let drag = PRESS.with(|press| {
                        let mut press = press.borrow_mut();
                        let state = press.as_mut()?;
                        state.handed_off = true;
                        Some((state.hwnd, state.target))
                    });
                    if let Some((pressed_hwnd, target)) = drag {
                        // Let go first: the real window's move loop needs capture.
                        unsafe {
                            let _ = KillTimer(Some(pressed_hwnd), PRESS_TIMER_ID);
                            let _ = ReleaseCapture();
                        }
                        if source_process_still_matches(target) {
                            emit_gesture(target, PreviewGesture::Drag);
                        } else {
                            warn!("Preview drag dropped: source publication became stale");
                        }
                    }
                }
                Some(PressMoveDecision::Continue) | None => {}
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            debug!("Preview Up received by overlay {:?}", hwnd);
            // The press may have started on a neighbouring overlay, so the
            // gesture belongs to the window recorded with it, not to the window
            // that happens to receive the release.
            let pressed = PRESS.with(|press| press.borrow_mut().take());
            unsafe {
                if let Some(ref pressed) = pressed {
                    let _ = KillTimer(Some(pressed.hwnd), PRESS_TIMER_ID);
                }
                let _ = ReleaseCapture();
            }
            if let Some(target) = pressed
                .filter(|state| !state.handed_off)
                .map(|state| state.target)
            {
                if source_process_still_matches(target) {
                    emit_gesture(target, PreviewGesture::Click);
                } else {
                    warn!("Preview click dropped: source publication became stale");
                }
            }
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            debug!("Preview capture changed for overlay {:?}", hwnd);
            // Losing capture ends the gesture: the pointer now belongs to
            // something else, so a later release is not ours to interpret.
            if let Some(press) = PRESS.with(|press| press.borrow_mut().take()) {
                unsafe {
                    let _ = KillTimer(Some(press.hwnd), PRESS_TIMER_ID);
                }
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == PRESS_TIMER_ID => {
            if !left_button_is_down() {
                if let Some(press) = PRESS.with(|press| press.borrow_mut().take()) {
                    unsafe {
                        let _ = KillTimer(Some(press.hwnd), PRESS_TIMER_ID);
                        let _ = ReleaseCapture();
                    }
                    let mut point = POINT::default();
                    let released_as_click = !press.handed_off
                        && unsafe { GetCursorPos(&mut point) }.is_ok()
                        && !travelled_past_drag_threshold(
                            press.origin,
                            (point.x, point.y),
                            press.drag_threshold,
                        )
                        && source_process_still_matches(press.target);
                    if released_as_click {
                        debug!(
                            "Preview release recovered outside overlay for generation {}",
                            press.target.publication_generation
                        );
                        emit_gesture(press.target, PreviewGesture::Click);
                    } else {
                        debug!(
                            "Preview press generation {} cancelled after release left overlay",
                            press.target.publication_generation
                        );
                    }
                }
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use super::{
        generation_ack_state, generation_needs_reconcile, press_move_decision, reconcile_plan,
        record_desired_targets, targets_are_armed_for_lifecycle, DesiredTargets,
        GenerationAckState, PressMoveDecision, PreviewClickTarget,
    };
    use leopardwm_core_layout::Rect;

    fn target(window_id: u64, x: i32, width: i32) -> PreviewClickTarget {
        PreviewClickTarget {
            window_id,
            source_process_id: 42,
            publication_generation: window_id,
            rect: Rect::new(x, 100, width, 800),
        }
    }

    #[test]
    fn desired_and_applied_generations_keep_lost_syncs_dirty() {
        let preview = target(1, 0, 250);
        let mut desired = DesiredTargets::default();
        assert_eq!(record_desired_targets(&mut desired, &[]), 0);
        assert!(!generation_needs_reconcile(0, 0));

        let nonempty = record_desired_targets(&mut desired, &[preview]);
        assert_eq!(nonempty, 1);
        assert!(generation_needs_reconcile(0, nonempty));
        assert_eq!(record_desired_targets(&mut desired, &[preview]), nonempty);
        assert!(generation_needs_reconcile(0, nonempty));
        assert!(!generation_needs_reconcile(nonempty, nonempty));

        let clear = record_desired_targets(&mut desired, &[]);
        assert_eq!(clear, 2);
        assert!(generation_needs_reconcile(nonempty, clear));
    }

    #[test]
    fn stale_activation_epoch_cannot_rearm_after_invalidation() {
        assert!(targets_are_armed_for_lifecycle(7, 7));
        assert!(!targets_are_armed_for_lifecycle(7, 8));
        assert!(!targets_are_armed_for_lifecycle(0, 8));
    }

    #[test]
    fn superseding_generation_never_acknowledges_old_surface() {
        assert_eq!(generation_ack_state(7, 7, 7), GenerationAckState::Exact);
        assert_eq!(generation_ack_state(6, 7, 7), GenerationAckState::Pending);
        assert_eq!(
            generation_ack_state(8, 8, 7),
            GenerationAckState::Superseded
        );
        // Wrapping does not rely on numeric ordering.
        assert_eq!(
            generation_ack_state(u64::MAX, 1, 1),
            GenerationAckState::Pending
        );
    }

    #[test]
    fn first_previews_are_created() {
        let (create, update, drop) =
            reconcile_plan(&[], &[target(1, 0, 250), target(2, 1670, 250)]);
        assert_eq!(create.len(), 2);
        assert!(update.is_empty());
        assert!(drop.is_empty());
    }

    #[test]
    fn moving_a_preview_reuses_its_overlay() {
        let (create, update, drop) = reconcile_plan(&[1], &[target(1, 40, 300)]);
        assert!(create.is_empty());
        assert_eq!(update, vec![target(1, 40, 300)]);
        assert!(drop.is_empty());
    }

    #[test]
    fn previews_that_scrolled_away_drop_their_overlay() {
        let (create, update, drop) = reconcile_plan(&[1, 2], &[target(2, 1670, 250)]);
        assert!(create.is_empty());
        assert_eq!(update, vec![target(2, 1670, 250)]);
        assert_eq!(drop, vec![1]);
    }

    #[test]
    fn a_wobbling_press_stays_a_click() {
        assert!(!super::travelled_past_drag_threshold(
            (100, 100),
            (103, 101),
            (4, 4)
        ));
        assert!(super::travelled_past_drag_threshold(
            (100, 100),
            (120, 100),
            (4, 4)
        ));
        assert!(super::travelled_past_drag_threshold(
            (100, 100),
            (100, 80),
            (4, 4)
        ));
        // A zero threshold from a broken metric must not make every move a drag.
        assert!(!super::travelled_past_drag_threshold(
            (100, 100),
            (101, 100),
            (0, 0)
        ));
    }

    #[test]
    fn press_move_reducer_handles_cross_overlay_drag_and_lost_release() {
        let origin = (100, 100);
        let threshold = (4, 4);
        assert_eq!(
            press_move_decision(origin, (102, 103), threshold, false, true, true),
            PressMoveDecision::Continue
        );
        // The receiving HWND is deliberately absent from the reducer: movement
        // over an adjacent overlay still belongs to the captured press.
        assert_eq!(
            press_move_decision(origin, (120, 100), threshold, false, true, true),
            PressMoveDecision::Drag
        );
        // Synthetic moves may omit the message flag while the physical button
        // remains down; that is not cancellation.
        assert_eq!(
            press_move_decision(origin, (102, 100), threshold, false, false, true),
            PressMoveDecision::Continue
        );
        assert_eq!(
            press_move_decision(origin, (102, 100), threshold, false, false, false),
            PressMoveDecision::Cancel
        );
        assert_eq!(
            press_move_decision(origin, (120, 100), threshold, true, true, true),
            PressMoveDecision::Continue,
            "a handed-off drag emits only once"
        );
    }

    #[test]
    fn clearing_drops_every_overlay() {
        let (create, update, drop) = reconcile_plan(&[7, 9], &[]);
        assert!(create.is_empty());
        assert!(update.is_empty());
        assert_eq!(drop.len(), 2);
    }
}
