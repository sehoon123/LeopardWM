//! Window style tweaks: DWM border color and WS_MAXIMIZEBOX (snap layout) management.

use crate::types::Win32Error;
use crate::window_id_to_hwnd;
use leopardwm_core_layout::WindowId;
use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::Mutex;
use windows::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows::Win32::UI::WindowsAndMessaging::IsWindow;

// ============================================================================
// Border color
// ============================================================================

/// Set the DWM border color for a window (Windows 11+).
///
/// Returns Ok(true) if the border was set, Ok(false) if the API is unsupported.
pub fn set_window_border_color(hwnd: WindowId, color: u32) -> Result<bool, Win32Error> {
    let window_id = hwnd;
    let hwnd = window_id_to_hwnd(window_id)?;
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return Err(Win32Error::WindowNotFound(window_id));
        }

        // DWMWA_BORDER_COLOR = 34
        const DWMWA_BORDER_COLOR: u32 = 34;
        let colorref = color;
        let result = DwmSetWindowAttribute(
            hwnd,
            windows::Win32::Graphics::Dwm::DWMWINDOWATTRIBUTE(DWMWA_BORDER_COLOR as i32),
            &colorref as *const u32 as *const c_void,
            std::mem::size_of::<u32>() as u32,
        );
        match result {
            Ok(()) => Ok(true),
            Err(e) => {
                if !IsWindow(Some(hwnd)).as_bool() {
                    return Err(Win32Error::WindowNotFound(window_id));
                }

                if is_border_color_unsupported_hresult(e.code()) {
                    return Ok(false);
                }

                Err(Win32Error::SetPositionFailed(format!(
                    "DwmSetWindowAttribute(DWMWA_BORDER_COLOR) failed for {:?}: {}",
                    hwnd, e
                )))
            }
        }
    }
}

/// Reset the DWM border color for a window to the default.
///
/// Returns Ok(true) if the border was reset, Ok(false) if the API is unsupported.
pub fn reset_window_border_color(hwnd: WindowId) -> Result<bool, Win32Error> {
    // DWMWA_COLOR_DEFAULT = 0xFFFFFFFF
    set_window_border_color(hwnd, 0xFFFFFFFF)
}

fn is_border_color_unsupported_hresult(code: windows::core::HRESULT) -> bool {
    const E_INVALIDARG_HRESULT: i32 = 0x8007_0057u32 as i32;
    const E_NOTIMPL_HRESULT: i32 = 0x8000_4001u32 as i32;
    code.0 == E_INVALIDARG_HRESULT || code.0 == E_NOTIMPL_HRESULT
}

// ============================================================================
// Snap layout suppression (WS_MAXIMIZEBOX removal)
// ============================================================================

const WS_MAXIMIZEBOX_STYLE: i32 = 0x0001_0000;

/// Global set of window IDs whose WS_MAXIMIZEBOX style has been removed.
/// Used for panic recovery when AppState may be poisoned/unavailable.
static SNAP_DISABLED_HWNDS: Mutex<Option<HashSet<WindowId>>> = Mutex::new(None);

fn lock_snap_disabled() -> std::sync::MutexGuard<'static, Option<HashSet<WindowId>>> {
    SNAP_DISABLED_HWNDS
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
}

fn remove_snap_receipt(window_id: WindowId) {
    if let Some(receipts) = lock_snap_disabled().as_mut() {
        receipts.remove(&window_id);
    }
}

/// Internal API boundary for the style/frame transaction. The production
/// implementation uses User32; tests inject failures at each physical step.
trait SnapStyleApi {
    fn get_style(&mut self) -> Result<i32, String>;
    fn set_style(&mut self, style: i32) -> Result<(), String>;
    fn commit_frame_change(&mut self) -> Result<(), String>;
}

struct Win32SnapStyleApi {
    hwnd: windows::Win32::Foundation::HWND,
}

impl SnapStyleApi for Win32SnapStyleApi {
    fn get_style(&mut self) -> Result<i32, String> {
        use windows::Win32::Foundation::{GetLastError, SetLastError, WIN32_ERROR};
        use windows::Win32::UI::WindowsAndMessaging::{GetWindowLongW, GWL_STYLE};

        unsafe {
            // GetWindowLongW may validly return zero. Clear last-error first
            // so zero is only accepted when User32 did not report failure.
            SetLastError(WIN32_ERROR(0));
            let style = GetWindowLongW(self.hwnd, GWL_STYLE);
            if style == 0 {
                let error = GetLastError();
                if error.0 != 0 {
                    return Err(format!("GetWindowLongW(GWL_STYLE) failed: {}", error.0));
                }
            }
            Ok(style)
        }
    }

    fn set_style(&mut self, style: i32) -> Result<(), String> {
        use windows::Win32::Foundation::{GetLastError, SetLastError, WIN32_ERROR};
        use windows::Win32::UI::WindowsAndMessaging::{SetWindowLongW, GWL_STYLE};

        unsafe {
            // SetWindowLongW returns the previous style, which may also be
            // zero; follow the documented last-error protocol for that case.
            SetLastError(WIN32_ERROR(0));
            let previous = SetWindowLongW(self.hwnd, GWL_STYLE, style);
            if previous == 0 {
                let error = GetLastError();
                if error.0 != 0 {
                    return Err(format!("SetWindowLongW(GWL_STYLE) failed: {}", error.0));
                }
            }
            Ok(())
        }
    }

    fn commit_frame_change(&mut self) -> Result<(), String> {
        use windows::Win32::UI::WindowsAndMessaging::{
            SetWindowPos, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
        };

        unsafe {
            SetWindowPos(
                self.hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            )
            .map_err(|error| format!("SetWindowPos(SWP_FRAMECHANGED) failed: {error}"))
        }
    }
}

/// Apply a style change as one physical transaction. A removal whose style
/// write reached User32 but whose frame notification failed is retained as a
/// recovery receipt; a restoration receipt is removed only after both the
/// style readback and frame change succeed.
fn set_maximizebox_state<A: SnapStyleApi>(
    api: &mut A,
    receipts: &mut HashSet<WindowId>,
    window_id: WindowId,
    want_maximizebox: bool,
) -> Result<bool, String> {
    let current = api.get_style()?;
    let has_maximizebox = current & WS_MAXIMIZEBOX_STYLE != 0;

    if has_maximizebox == want_maximizebox {
        // A previous restoration may have written the bit but failed to
        // commit the non-client frame. Finish that receipt before retiring it.
        if want_maximizebox && receipts.contains(&window_id) {
            api.commit_frame_change()?;
            receipts.remove(&window_id);
        }
        return Ok(false);
    }

    let wanted_style = if want_maximizebox {
        current | WS_MAXIMIZEBOX_STYLE
    } else {
        current & !WS_MAXIMIZEBOX_STYLE
    };

    // Read back even when SetWindowLongW reports an error: a racing or
    // partially reported call can still have changed the style, and a removed
    // bit must retain a recovery receipt rather than becoming ownerless.
    let set_result = api.set_style(wanted_style);
    let observed = api.get_style()?;
    let observed_maximizebox = observed & WS_MAXIMIZEBOX_STYLE != 0;
    if observed_maximizebox == want_maximizebox && (!want_maximizebox || set_result.is_err()) {
        receipts.insert(window_id);
    }

    set_result?;
    if observed_maximizebox != want_maximizebox {
        return Err(format!(
            "WS_MAXIMIZEBOX readback mismatch (wanted {}, observed {})",
            want_maximizebox, observed_maximizebox
        ));
    }

    if let Err(error) = api.commit_frame_change() {
        // The bit is known to be in the requested state, but the frame did
        // not commit. Keep (or create) a receipt so a later restore retries.
        receipts.insert(window_id);
        return Err(error);
    }

    if want_maximizebox {
        receipts.remove(&window_id);
    } else {
        receipts.insert(window_id);
    }
    Ok(true)
}

fn update_maximizebox(window_id: WindowId, want_maximizebox: bool) -> Result<bool, Win32Error> {
    let hwnd = window_id_to_hwnd(window_id)?;
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            // There is no live target left to restore, so retaining a numeric
            // receipt would risk applying it to a later recycled HWND.
            remove_snap_receipt(window_id);
            return Err(Win32Error::WindowNotFound(window_id));
        }
    }

    let result = {
        let mut receipts = lock_snap_disabled();
        let receipts = receipts.get_or_insert_with(HashSet::new);
        let mut api = Win32SnapStyleApi { hwnd };
        set_maximizebox_state(&mut api, receipts, window_id, want_maximizebox)
    };

    match result {
        Ok(changed) => Ok(changed),
        Err(error) => {
            unsafe {
                if !IsWindow(Some(hwnd)).as_bool() {
                    remove_snap_receipt(window_id);
                    return Err(Win32Error::WindowNotFound(window_id));
                }
            }
            Err(Win32Error::SetPositionFailed(format!(
                "WS_MAXIMIZEBOX {} failed for {:?}: {}",
                if want_maximizebox {
                    "restore"
                } else {
                    "removal"
                },
                hwnd,
                error
            )))
        }
    }
}

/// Remove `WS_MAXIMIZEBOX` from a window to disable Windows 11 Snap Layouts.
///
/// Returns `Ok(true)` only after the style readback and non-client frame
/// update both succeed. A partial removal retains a recovery receipt.
///
/// Uses `GetWindowLongW`/`SetWindowLongW` (32-bit) intentionally: on 64-bit
/// Windows this disables the DWM snap layout flyout while preserving the
/// maximize button and its click-to-maximize behavior.
pub fn remove_maximizebox(window_id: WindowId) -> Result<bool, Win32Error> {
    update_maximizebox(window_id, false)
}

/// Restore `WS_MAXIMIZEBOX` on a window, re-enabling Windows 11 Snap Layouts.
///
/// Returns `Ok(true)` only after the style readback and non-client frame
/// update both succeed. Recovery tracking is removed only at that point.
pub fn restore_maximizebox(window_id: WindowId) -> Result<bool, Win32Error> {
    update_maximizebox(window_id, true)
}

/// Best-effort bulk restore of `WS_MAXIMIZEBOX` for multiple windows.
/// Never panics — logs failures and continues.
pub fn restore_maximizebox_all(window_ids: &[WindowId]) {
    for &wid in window_ids {
        match restore_maximizebox(wid) {
            Ok(_) => {}
            Err(Win32Error::WindowNotFound(_)) => {
                // A dead HWND has no style left to restore; its receipt was
                // retired only after current liveness was checked.
            }
            Err(e) => {
                tracing::warn!("Failed to restore WS_MAXIMIZEBOX for window {}: {}", wid, e);
            }
        }
    }
}

/// Emergency restore of `WS_MAXIMIZEBOX` for all tracked windows.
/// Retains every receipt until an individually verified restore succeeds, so
/// a transient User32 failure remains retryable if panic recovery returns.
pub fn restore_maximizebox_panic_recovery() {
    let window_ids: Vec<WindowId> = lock_snap_disabled()
        .as_ref()
        .map(|set| set.iter().copied().collect())
        .unwrap_or_default();

    if window_ids.is_empty() {
        return;
    }

    eprintln!(
        "[leopardwm] Restoring WS_MAXIMIZEBOX for {} window(s) in panic recovery",
        window_ids.len()
    );

    restore_maximizebox_all(&window_ids);

    eprintln!(
        "[leopardwm] WS_MAXIMIZEBOX panic recovery complete ({} windows processed)",
        window_ids.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct FakeSnapStyleApi {
        style: i32,
        set_error: bool,
        set_changes_style_on_error: bool,
        frame_error: bool,
        frame_calls: usize,
    }

    impl FakeSnapStyleApi {
        fn with_maximizebox() -> Self {
            Self {
                style: WS_MAXIMIZEBOX_STYLE | 0x10,
                set_error: false,
                set_changes_style_on_error: false,
                frame_error: false,
                frame_calls: 0,
            }
        }
    }

    impl SnapStyleApi for FakeSnapStyleApi {
        fn get_style(&mut self) -> Result<i32, String> {
            Ok(self.style)
        }

        fn set_style(&mut self, style: i32) -> Result<(), String> {
            if !self.set_error || self.set_changes_style_on_error {
                self.style = style;
            }
            if self.set_error {
                Err("injected SetWindowLongW failure".into())
            } else {
                Ok(())
            }
        }

        fn commit_frame_change(&mut self) -> Result<(), String> {
            self.frame_calls += 1;
            if self.frame_error {
                Err("injected SWP_FRAMECHANGED failure".into())
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn test_is_border_color_unsupported_hresult_mapping() {
        assert!(is_border_color_unsupported_hresult(windows::core::HRESULT(
            0x8007_0057u32 as i32
        )));
        assert!(is_border_color_unsupported_hresult(windows::core::HRESULT(
            0x8000_4001u32 as i32
        )));
        assert!(!is_border_color_unsupported_hresult(
            windows::core::HRESULT(0x8000_4005u32 as i32)
        ));
    }

    #[test]
    fn removal_does_not_record_receipt_when_style_write_fails() {
        let wid = 41;
        let mut api = FakeSnapStyleApi {
            set_error: true,
            ..FakeSnapStyleApi::with_maximizebox()
        };
        let mut receipts = HashSet::new();

        assert!(set_maximizebox_state(&mut api, &mut receipts, wid, false).is_err());
        assert!(receipts.is_empty());
        assert_ne!(api.style & WS_MAXIMIZEBOX_STYLE, 0);
    }

    #[test]
    fn partial_removal_keeps_a_recovery_receipt() {
        let wid = 42;
        let mut api = FakeSnapStyleApi {
            frame_error: true,
            ..FakeSnapStyleApi::with_maximizebox()
        };
        let mut receipts = HashSet::new();

        assert!(set_maximizebox_state(&mut api, &mut receipts, wid, false).is_err());
        assert_eq!(api.style & WS_MAXIMIZEBOX_STYLE, 0);
        assert!(receipts.contains(&wid));
    }

    #[test]
    fn restoration_keeps_receipt_until_frame_commit_succeeds() {
        let wid = 43;
        let mut api = FakeSnapStyleApi {
            style: 0x10,
            frame_error: true,
            ..FakeSnapStyleApi::with_maximizebox()
        };
        let mut receipts = HashSet::from([wid]);

        assert!(set_maximizebox_state(&mut api, &mut receipts, wid, true).is_err());
        assert_ne!(api.style & WS_MAXIMIZEBOX_STYLE, 0);
        assert!(receipts.contains(&wid));

        api.frame_error = false;
        assert_eq!(
            set_maximizebox_state(&mut api, &mut receipts, wid, true),
            Ok(false),
            "the retry only needs to commit the pending frame"
        );
        assert!(!receipts.contains(&wid));
        assert_eq!(api.frame_calls, 2);
    }

    #[test]
    fn write_error_with_observed_removal_still_keeps_receipt() {
        let wid = 44;
        let mut api = FakeSnapStyleApi {
            set_error: true,
            set_changes_style_on_error: true,
            ..FakeSnapStyleApi::with_maximizebox()
        };
        let mut receipts = HashSet::new();

        assert!(set_maximizebox_state(&mut api, &mut receipts, wid, false).is_err());
        assert!(receipts.contains(&wid));
    }

    #[test]
    fn test_set_window_border_color_zero_fails() {
        let result = set_window_border_color(0, 0x4285F4);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Win32Error::WindowNotFound(0)));
    }

    #[test]
    fn test_set_window_border_color_invalid_hwnd_fails() {
        let result = set_window_border_color(u64::MAX, 0x4285F4);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Win32Error::WindowNotFound(u64::MAX)
        ));
    }

    #[test]
    fn test_reset_window_border_color_zero_fails() {
        let result = reset_window_border_color(0);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Win32Error::WindowNotFound(0)));
    }

    #[test]
    fn test_remove_maximizebox_zero_fails() {
        let result = remove_maximizebox(0);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Win32Error::WindowNotFound(0)));
    }

    #[test]
    fn test_remove_maximizebox_invalid_hwnd_fails() {
        let result = remove_maximizebox(u64::MAX);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Win32Error::WindowNotFound(u64::MAX)
        ));
    }

    #[test]
    fn test_restore_maximizebox_zero_fails() {
        let result = restore_maximizebox(0);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Win32Error::WindowNotFound(0)));
    }

    #[test]
    fn test_restore_maximizebox_invalid_hwnd_fails() {
        let result = restore_maximizebox(u64::MAX);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Win32Error::WindowNotFound(u64::MAX)
        ));
    }

    #[test]
    fn test_restore_maximizebox_all_empty_is_noop() {
        restore_maximizebox_all(&[]);
    }

    #[test]
    fn test_restore_maximizebox_panic_recovery_no_panic() {
        restore_maximizebox_panic_recovery();
    }
}
