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
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetWindowLongPtrW, PostThreadMessageW, RegisterClassW, SetLayeredWindowAttributes,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage, GWLP_USERDATA, HWND_TOPMOST,
    LWA_ALPHA, MA_NOACTIVATE, MSG, SWP_NOACTIVATE, SWP_NOZORDER, SW_HIDE, SW_SHOWNOACTIVATE,
    WM_APP, WM_LBUTTONDOWN, WM_MOUSEACTIVATE, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

const PREVIEW_TARGET_CLASS: &str = "LeopardWMPreviewClickTarget";
/// Wakes the pump after new desired state has been queued.
const WM_PREVIEW_SYNC: u32 = WM_APP + 0x51;
/// Nearly invisible, still hit-testable. Uniform-alpha layered windows keep
/// normal hit testing; only `WS_EX_TRANSPARENT` or per-pixel alpha of zero make
/// a window click-through, and 1/255 over a thumbnail is not perceivable.
const TARGET_ALPHA: u8 = 1;

/// One previewed window and the screen rectangle its thumbnail occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewClickTarget {
    pub window_id: WindowId,
    pub rect: Rect,
}

/// A click on a preview. Carries the previewed window, not a screen point, so
/// the daemon focuses the column the user actually saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewClickEvent {
    pub window_id: WindowId,
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

fn emit_click(window_id: WindowId) {
    let guard = click_sender()
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex);
    if let Some(sender) = guard.as_ref() {
        let _ = sender.send(PreviewClickEvent { window_id });
    }
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
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
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
                    Some(HWND_TOPMOST),
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
            // Move only. Re-pinning to the top of the topmost band on every
            // animation frame would fight the tab strip, which pins itself the
            // same way, making a click on a tabbed edge column land
            // nondeterministically on the strip or on this overlay.
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

unsafe extern "system" fn target_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // Clicking a preview must not activate this overlay, or the click would
    // steal focus from the window the daemon is about to focus.
    if message == WM_MOUSEACTIVATE {
        return LRESULT(MA_NOACTIVATE as isize);
    }
    if message == WM_LBUTTONDOWN {
        let window_id = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as WindowId;
        if window_id != 0 {
            emit_click(window_id);
        }
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
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
    fn clearing_drops_every_overlay() {
        let (create, update, drop) = reconcile_plan(&[7, 9], &[]);
        assert!(create.is_empty());
        assert!(update.is_empty());
        assert_eq!(drop.len(), 2);
    }
}
