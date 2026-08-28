//! Window style tweaks: DWM border color and WS_MAXIMIZEBOX (snap layout) management.

use crate::types::Win32Error;
use crate::window_id_to_hwnd;
use leopardwm_core_layout::WindowId;
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use windows::core::w;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows::Win32::UI::WindowsAndMessaging::{GetPropW, IsWindow, RemovePropW, SetPropW};

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

#[derive(Debug, Clone, PartialEq, Eq)]
struct SnapWindowIdentity {
    incarnation_token: u64,
    process_id: u32,
    thread_id: u32,
    class_name: String,
}

#[derive(Debug, Clone)]
struct SnapReceipt {
    identity: SnapWindowIdentity,
    token: usize,
}

/// Identity-protected recovery receipts. Numeric HWND values are recyclable;
/// both the captured identity and a per-HWND property must match before a
/// restore may touch the current window.
static SNAP_DISABLED_HWNDS: Mutex<Option<HashMap<WindowId, SnapReceipt>>> = Mutex::new(None);
static SNAP_COMMIT: Mutex<()> = Mutex::new(());
static NEXT_SNAP_TOKEN: AtomicUsize = AtomicUsize::new(1);

fn lock_snap_disabled() -> std::sync::MutexGuard<'static, Option<HashMap<WindowId, SnapReceipt>>> {
    SNAP_DISABLED_HWNDS
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
}

fn snap_identity(hwnd: windows::Win32::Foundation::HWND) -> Option<SnapWindowIdentity> {
    let identity = crate::event_hooks::current_window_event_identity(hwnd.0 as usize as u64)?;
    Some(SnapWindowIdentity {
        incarnation_token: identity.token,
        process_id: identity.process_id,
        thread_id: identity.thread_id,
        class_name: identity.class_name,
    })
}

fn next_snap_token() -> usize {
    NEXT_SNAP_TOKEN
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            Some(value.wrapping_add(1).max(1))
        })
        .unwrap_or_else(|value| value)
        .max(1)
}

fn snap_property_token(hwnd: windows::Win32::Foundation::HWND) -> usize {
    unsafe { GetPropW(hwnd, w!("LeopardWM.SnapStyle.v1")) }.0 as usize
}

fn install_snap_property(hwnd: windows::Win32::Foundation::HWND, token: usize) -> bool {
    unsafe {
        SetPropW(
            hwnd,
            w!("LeopardWM.SnapStyle.v1"),
            Some(HANDLE(token as *mut c_void)),
        )
    }
    .is_ok()
}

fn clear_snap_property_if_owned(hwnd: windows::Win32::Foundation::HWND, token: usize) {
    if snap_property_token(hwnd) == token {
        unsafe {
            let _ = RemovePropW(hwnd, w!("LeopardWM.SnapStyle.v1"));
        }
    }
}

fn remove_snap_receipt(window_id: WindowId) {
    let receipt = lock_snap_disabled()
        .as_mut()
        .and_then(|receipts| receipts.remove(&window_id));
    if let (Some(receipt), Ok(hwnd)) = (receipt, window_id_to_hwnd(window_id)) {
        clear_snap_property_if_owned(hwnd, receipt.token);
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
    let _commit = SNAP_COMMIT
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex);
    let hwnd = window_id_to_hwnd(window_id)?;
    let Some(identity) = snap_identity(hwnd) else {
        remove_snap_receipt(window_id);
        return Err(Win32Error::WindowNotFound(window_id));
    };

    let existing = lock_snap_disabled()
        .as_ref()
        .and_then(|receipts| receipts.get(&window_id).cloned());
    if let Some(receipt) = existing.as_ref() {
        if receipt.identity != identity || snap_property_token(hwnd) != receipt.token {
            remove_snap_receipt(window_id);
            return Err(Win32Error::WindowNotFound(window_id));
        }
    } else if want_maximizebox {
        // No platform ownership receipt means this call cannot prove that it
        // removed the style from the current HWND incarnation.
        return Ok(false);
    }

    if existing.is_none() && !want_maximizebox {
        let mut api = Win32SnapStyleApi { hwnd };
        let original_style = api.get_style().map_err(|error| {
            Win32Error::SetPositionFailed(format!(
                "Could not inspect original snap style for {window_id:#x}: {error}"
            ))
        })?;
        if original_style & WS_MAXIMIZEBOX_STYLE == 0 {
            // Do not publish a durable recovery marker for a style LeopardWM
            // did not own. A crash in the following transaction may safely
            // restore the bit because this readback proved it was originally set.
            return Ok(false);
        }
    }

    let prepared_receipt = if let Some(receipt) = existing {
        receipt
    } else {
        let token = next_snap_token();
        if !install_snap_property(hwnd, token) {
            return Err(Win32Error::SetPositionFailed(format!(
                "Could not publish snap-style ownership for {window_id:#x}"
            )));
        }
        SnapReceipt { identity, token }
    };

    if snap_identity(hwnd).as_ref() != Some(&prepared_receipt.identity)
        || snap_property_token(hwnd) != prepared_receipt.token
    {
        clear_snap_property_if_owned(hwnd, prepared_receipt.token);
        remove_snap_receipt(window_id);
        return Err(Win32Error::WindowNotFound(window_id));
    }

    let mut receipt_ids: HashSet<WindowId> = lock_snap_disabled()
        .as_ref()
        .map(|receipts| receipts.keys().copied().collect())
        .unwrap_or_default();
    let mut api = Win32SnapStyleApi { hwnd };
    let result = set_maximizebox_state(&mut api, &mut receipt_ids, window_id, want_maximizebox);
    if snap_identity(hwnd).as_ref() != Some(&prepared_receipt.identity)
        || snap_property_token(hwnd) != prepared_receipt.token
    {
        // Never compensate through a recycled numeric HWND: even a show-like
        // style write would mutate a replacement whose original policy is unknown.
        clear_snap_property_if_owned(hwnd, prepared_receipt.token);
        remove_snap_receipt(window_id);
        return Err(Win32Error::WindowNotFound(window_id));
    }
    let receipt_still_owned = receipt_ids.contains(&window_id);
    if receipt_still_owned {
        lock_snap_disabled()
            .get_or_insert_with(HashMap::new)
            .insert(window_id, prepared_receipt.clone());
    } else {
        lock_snap_disabled()
            .get_or_insert_with(HashMap::new)
            .remove(&window_id);
        clear_snap_property_if_owned(hwnd, prepared_receipt.token);
    }

    match result {
        // An already-removed style with a retained platform receipt still
        // represents LeopardWM ownership. Report it as active so the daemon
        // re-arms its ordinary pause/shutdown retry set.
        Ok(changed) => Ok(changed || (!want_maximizebox && receipt_still_owned)),
        Err(error) => {
            if snap_identity(hwnd).as_ref() != Some(&prepared_receipt.identity)
                || snap_property_token(hwnd) != prepared_receipt.token
            {
                remove_snap_receipt(window_id);
                return Err(Win32Error::WindowNotFound(window_id));
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
        .map(|receipts| receipts.keys().copied().collect())
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

/// Cross-process hard-crash recovery for snap styles. The HWND property is the
/// durable ownership receipt: properties disappear with the original HWND, so
/// a recycled numeric handle cannot inherit this marker.
pub fn restore_marked_maximizeboxes_best_effort() -> usize {
    let mut restored = 0usize;
    for window_id in crate::enumeration::collect_all_top_level_window_ids() {
        let Ok(hwnd) = window_id_to_hwnd(window_id) else {
            continue;
        };
        let token = snap_property_token(hwnd);
        if token == 0 {
            continue;
        }
        let _commit = SNAP_COMMIT
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex);
        let mut receipt_ids = HashSet::from([window_id]);
        let mut api = Win32SnapStyleApi { hwnd };
        match set_maximizebox_state(&mut api, &mut receipt_ids, window_id, true) {
            Ok(_) if !receipt_ids.contains(&window_id) => {
                clear_snap_property_if_owned(hwnd, token);
                restored += 1;
            }
            Ok(_) => tracing::warn!(
                "Marked snap-style recovery retained an unexpected receipt for {window_id:#x}"
            ),
            Err(error) => {
                tracing::warn!("Marked snap-style recovery failed for {window_id:#x}: {error}")
            }
        }
    }
    restored
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
