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
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use tracing::warn;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, PAINTSTRUCT,
};
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW,
    GetSystemMetrics, GetWindowLongPtrW, IsWindowVisible, LoadCursorW, PostThreadMessageW,
    RegisterClassW, SetCursor, SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPos,
    ShowWindow, TranslateMessage, GWLP_USERDATA, HWND_TOP, IDC_HAND, LWA_ALPHA, MA_NOACTIVATE, MSG,
    SM_CXDRAG, SM_CYDRAG, SWP_NOACTIVATE, SWP_NOZORDER, SW_HIDE, SW_SHOWNOACTIVATE, WM_APP,
    WM_CAPTURECHANGED, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_PAINT,
    WM_SETCURSOR, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};

const PREVIEW_TARGET_CLASS: &str = "LeopardWMPreviewClickTarget";
/// Hover wash colour in BGR, matching the default active-border blue.
const HOVER_WASH_BGR: u32 = 0x00F4_7E43;
/// Wakes the pump after new desired state has been queued.
const WM_PREVIEW_SYNC: u32 = WM_APP + 0x51;
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
    pub gesture: PreviewGesture,
}

struct PreviewInput {
    thread_id: u32,
    desired: Mutex<Vec<PreviewClickTarget>>,
}

static INPUT: OnceLock<Option<PreviewInput>> = OnceLock::new();
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

fn emit_gesture(window_id: WindowId, gesture: PreviewGesture) {
    let guard = click_sender()
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex);
    if let Some(sender) = guard.as_ref() {
        let _ = sender.send(PreviewClickEvent { window_id, gesture });
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

/// Reconcile the overlays with the previews that are currently published.
///
/// Safe to call from any thread and on every applied frame: the desired state is
/// stored and the owning thread performs the window operations.
pub fn sync_preview_click_targets(targets: &[PreviewClickTarget]) {
    let Some(input) = input() else {
        return;
    };
    {
        let mut desired = input
            .desired
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex);
        if desired.as_slice() == targets {
            return;
        }
        *desired = targets.to_vec();
    }
    unsafe {
        let _ = PostThreadMessageW(input.thread_id, WM_PREVIEW_SYNC, WPARAM(0), LPARAM(0));
    }
}

/// Drop every overlay, e.g. when the last preview disappears or on shutdown.
pub fn clear_preview_click_targets() {
    sync_preview_click_targets(&[]);
}

fn input() -> Option<&'static PreviewInput> {
    INPUT
        .get_or_init(|| {
            #[cfg(test)]
            {
                // Overlays need a real message pump; unit tests exercise the
                // pure reconciliation instead.
                None
            }
            #[cfg(not(test))]
            match PreviewInput::spawn() {
                Ok(input) => Some(input),
                Err(error) => {
                    warn!("Preview click targets unavailable: {error}");
                    None
                }
            }
        })
        .as_ref()
}

impl PreviewInput {
    #[cfg_attr(test, allow(dead_code))]
    fn spawn() -> Result<Self, Win32Error> {
        let (tx, rx) = mpsc::channel::<u32>();
        std::thread::Builder::new()
            .name("leopardwm-preview-input".into())
            .spawn(move || unsafe {
                let class: Vec<u16> = format!("{PREVIEW_TARGET_CLASS}\0").encode_utf16().collect();
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(target_proc),
                    lpszClassName: windows::core::PCWSTR(class.as_ptr()),
                    ..Default::default()
                };
                RegisterClassW(&wc);
                let _ = tx.send(windows::Win32::System::Threading::GetCurrentThreadId());

                let mut windows_by_id: HashMap<WindowId, HWND> = HashMap::new();
                let mut message = MSG::default();
                // `GetMessageW` returns -1 on error, which `as_bool()` reports as
                // true; spinning on that would peg a core, so stop on anything
                // that is not a real message.
                loop {
                    let result = GetMessageW(&mut message, None, 0, 0).0;
                    if result <= 0 {
                        break;
                    }
                    if message.message == WM_PREVIEW_SYNC {
                        reconcile_targets(&class, &mut windows_by_id);
                        continue;
                    }
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                // Never leave an invisible topmost window behind: without this
                // an exiting pump would strand click-absorbing overlays for the
                // rest of the session.
                for hwnd in windows_by_id.values() {
                    let _ = DestroyWindow(*hwnd);
                }
            })
            .map_err(|error| {
                Win32Error::SetPositionFailed(format!("preview input thread: {error}"))
            })?;

        let thread_id = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| {
                Win32Error::SetPositionFailed(format!("preview input thread id: {error}"))
            })?;
        Ok(Self {
            thread_id,
            desired: Mutex::new(Vec::new()),
        })
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
unsafe fn reconcile_targets(class: &[u16], windows_by_id: &mut HashMap<WindowId, HWND>) {
    let Some(input) = input() else {
        return;
    };
    let desired = input
        .desired
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
        .clone();
    let existing: Vec<WindowId> = windows_by_id.keys().copied().collect();
    let (create, update, drop) = reconcile_plan(&existing, &desired);

    for window_id in drop {
        if let Some(hwnd) = windows_by_id.remove(&window_id) {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
    }

    for target in create {
        if target.rect.width <= 0 || target.rect.height <= 0 {
            continue;
        }
        // Defensive: a tracked HWND for this id would be orphaned by the insert
        // below and could never be destroyed again.
        if let Some(stale) = windows_by_id.remove(&target.window_id) {
            unsafe {
                let _ = DestroyWindow(stale);
            }
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
                let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), TARGET_ALPHA, LWA_ALPHA);
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOP),
                    target.rect.x,
                    target.rect.y,
                    target.rect.width,
                    target.rect.height,
                    SWP_NOACTIVATE,
                );
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                windows_by_id.insert(target.window_id, hwnd);
            },
            Err(error) => {
                warn!("Preview click target creation failed: {error}");
                // The desired state already records this target, so an identical
                // next publish would be deduplicated into a no-op and the click
                // target would stay dead. Forget it so the next frame retries.
                input
                    .desired
                    .lock()
                    .unwrap_or_else(crate::recover_poisoned_mutex)
                    .retain(|pending| pending.window_id != target.window_id);
            }
        }
    }

    for target in update {
        let Some(&hwnd) = windows_by_id.get(&target.window_id) else {
            continue;
        };
        unsafe {
            if target.rect.width <= 0 || target.rect.height <= 0 {
                let _ = ShowWindow(hwnd, SW_HIDE);
                continue;
            }
            // Re-assert the front of the normal band only when the overlay was
            // hidden: a window that was not visible may have lost its place,
            // while a per-frame re-pin would fight the tab strip.
            if !IsWindowVisible(hwnd).as_bool() {
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOP),
                    target.rect.x,
                    target.rect.y,
                    target.rect.width,
                    target.rect.height,
                    SWP_NOACTIVATE,
                );
            }
            // Move only. Re-pinning to the front of the band on every animation
            // frame would fight the tab strip, which pins itself the same way,
            // making a click on a tabbed edge column land nondeterministically
            // on the strip or on this overlay.
            let _ = SetWindowPos(
                hwnd,
                None,
                target.rect.x,
                target.rect.y,
                target.rect.width,
                target.rect.height,
                SWP_NOACTIVATE | SWP_NOZORDER,
            );
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
    }
}

/// Pointer state of the press in progress, in screen coordinates. Only the
/// overlay thread touches this, so a plain `Cell` pair is enough.
struct PressState {
    origin: (i32, i32),
    handed_off: bool,
}

thread_local! {
    static PRESS: std::cell::RefCell<Option<PressState>> = const { std::cell::RefCell::new(None) };
}

/// Turn the hover wash on or off, arming `WM_MOUSELEAVE` the first time the
/// pointer arrives so the wash cannot get stuck on.
fn set_hover(hwnd: HWND, hovering: bool) {
    let alpha = if hovering { HOVER_ALPHA } else { TARGET_ALPHA };
    unsafe {
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA);
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

fn cursor_screen_point(lparam: LPARAM) -> (i32, i32) {
    // WM_MOUSEMOVE carries client coordinates; the overlay is a plain popup, so
    // its client origin is its screen origin plus nothing, but read the real
    // cursor instead of trusting that.
    let mut point = POINT::default();
    if unsafe { GetCursorPos(&mut point) }.is_ok() {
        return (point.x, point.y);
    }
    let packed = lparam.0 as u32;
    (
        (packed & 0xFFFF) as i16 as i32,
        (packed >> 16) as i16 as i32,
    )
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
            // Capture so the drag threshold can be measured even after the
            // pointer leaves the strip, and release it before handing off.
            unsafe { SetCapture(hwnd) };
            PRESS.with(|press| {
                *press.borrow_mut() = Some(PressState {
                    origin: cursor_screen_point(lparam),
                    handed_off: false,
                });
            });
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            set_hover(hwnd, true);
            let travelled = PRESS.with(|press| {
                let mut press = press.borrow_mut();
                let Some(state) = press.as_mut() else {
                    return false;
                };
                if state.handed_off {
                    return false;
                }
                let threshold =
                    unsafe { (GetSystemMetrics(SM_CXDRAG), GetSystemMetrics(SM_CYDRAG)) };
                if travelled_past_drag_threshold(
                    state.origin,
                    cursor_screen_point(lparam),
                    threshold,
                ) {
                    state.handed_off = true;
                    true
                } else {
                    false
                }
            });
            if travelled {
                let window_id = window_id_of(hwnd);
                // Let go of the pointer first: the window's own move loop needs
                // the capture, and it starts as soon as the daemon hands it over.
                unsafe {
                    let _ = ReleaseCapture();
                }
                if window_id != 0 {
                    emit_gesture(window_id, PreviewGesture::Drag);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let was_click = PRESS.with(|press| {
                press
                    .borrow_mut()
                    .take()
                    .is_some_and(|state| !state.handed_off)
            });
            unsafe {
                let _ = ReleaseCapture();
            }
            if was_click {
                let window_id = window_id_of(hwnd);
                if window_id != 0 {
                    emit_gesture(window_id, PreviewGesture::Click);
                }
            }
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            PRESS.with(|press| *press.borrow_mut() = None);
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use super::{reconcile_plan, PreviewClickTarget};
    use leopardwm_core_layout::Rect;

    fn target(window_id: u64, x: i32, width: i32) -> PreviewClickTarget {
        PreviewClickTarget {
            window_id,
            rect: Rect::new(x, 100, width, 800),
        }
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
    fn clearing_drops_every_overlay() {
        let (create, update, drop) = reconcile_plan(&[7, 9], &[]);
        assert!(create.is_empty());
        assert!(update.is_empty());
        assert_eq!(drop.len(), 2);
    }
}
