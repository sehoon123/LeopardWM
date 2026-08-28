//! Off-screen sentinel parking, restore/uncloak recovery, and window positioning.

use crate::enumerate_monitors;
use crate::enumeration::{collect_all_top_level_window_ids, get_primary_monitor};
use crate::placement::apply_placements;
use crate::types::{PlatformConfig, Win32Error};
use crate::window_style::reset_window_border_color;
use crate::MOVE_OFFSCREEN_SENTINEL_COORD;
use crate::{combine_operation_failures, is_benign_side_effect_error, window_id_to_hwnd};
use leopardwm_core_layout::{Rect, Visibility, WindowId, WindowPlacement};
use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::Mutex;
use windows::core::w;
use windows::Win32::Foundation::{HANDLE, HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::{
    GetPropW, GetWindowRect, IsIconic, IsWindow, IsWindowVisible, RemovePropW, SetPropW,
    SetWindowPos, ShowWindow, HWND_TOP, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    SWP_SHOWWINDOW, SW_RESTORE, SW_SHOWNOACTIVATE,
};

// ============================================================================
// Offscreen sentinel helpers
// ============================================================================

const MOVE_OFFSCREEN_OWNER_MAGIC: usize = 0x4c57_4d4f; // "LWMO"
static MOVE_OFFSCREEN_OWNED: Mutex<Option<HashSet<WindowId>>> = Mutex::new(None);
// User32 clamps -100000 to the signed virtual-coordinate floor on current
// Windows builds. Keep legacy crash recovery able to recognize that landing.
const EFFECTIVE_SENTINEL_THRESHOLD: i32 = -32_768;

fn has_move_offscreen_marker(hwnd: HWND) -> bool {
    unsafe { GetPropW(hwnd, w!("LeopardWM.MoveOffscreen.v1")) }.0 as usize
        == MOVE_OFFSCREEN_OWNER_MAGIC
}

/// Whether this HWND is still owned by a verified `MoveOffScreen` park.
/// Placement samples this before moving a window back so compositor-return
/// repair cannot lose its only durable evidence.
pub fn has_move_offscreen_ownership(window_id: WindowId) -> bool {
    let owned = MOVE_OFFSCREEN_OWNED
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex);
    let runtime_owned = owned
        .as_ref()
        .is_some_and(|windows| windows.contains(&window_id));
    runtime_owned || window_id_to_hwnd(window_id).is_ok_and(has_move_offscreen_marker)
}

fn set_move_offscreen_marker(hwnd: HWND, window_id: WindowId) -> bool {
    let mut owned = MOVE_OFFSCREEN_OWNED
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex);
    let property_set = unsafe {
        SetPropW(
            hwnd,
            w!("LeopardWM.MoveOffscreen.v1"),
            Some(HANDLE(MOVE_OFFSCREEN_OWNER_MAGIC as *mut c_void)),
        )
    }
    .is_ok();
    if property_set {
        owned.get_or_insert_with(HashSet::new).insert(window_id);
    }
    property_set
}

pub(crate) fn clear_move_offscreen_marker(window_id: WindowId) {
    let mut owned = MOVE_OFFSCREEN_OWNED
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex);
    let property_cleared = if let Ok(hwnd) = window_id_to_hwnd(window_id) {
        unsafe {
            let _ = RemovePropW(hwnd, w!("LeopardWM.MoveOffscreen.v1"));
        }
        !has_move_offscreen_marker(hwnd)
    } else {
        true
    };
    if property_cleared {
        if let Some(windows) = owned.as_mut() {
            windows.remove(&window_id);
        }
    }
}

/// Check whether coordinates indicate a requested or User32-clamped sentinel.
pub fn is_move_offscreen_sentinel_position(x: i32, y: i32) -> bool {
    x <= EFFECTIVE_SENTINEL_THRESHOLD && y <= EFFECTIVE_SENTINEL_THRESHOLD
}

/// Check whether a rectangle indicates MoveOffScreen sentinel placement.
pub fn is_move_offscreen_sentinel_rect(rect: &Rect) -> bool {
    is_move_offscreen_sentinel_position(rect.x, rect.y)
}

/// Move a single window to the off-screen sentinel position.
/// Used by workspace switching to hide inactive workspace windows.
pub fn move_window_offscreen(window_id: WindowId) -> Result<(), Win32Error> {
    let hwnd = window_id_to_hwnd(window_id)?;
    let mut original = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut original) }.map_err(|error| {
        Win32Error::SetPositionFailed(format!(
            "Could not capture window {window_id} before offscreen move: {error}"
        ))
    })?;
    unsafe {
        SetWindowPos(
            hwnd,
            None,
            MOVE_OFFSCREEN_SENTINEL_COORD,
            MOVE_OFFSCREEN_SENTINEL_COORD,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        )
    }
    .map_err(|error| {
        Win32Error::SetPositionFailed(format!(
            "Failed to move window {window_id} offscreen: {error}"
        ))
    })?;

    let mut actual = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut actual) }.map_err(|error| {
        Win32Error::SetPositionFailed(format!(
            "Could not verify window {window_id} offscreen: {error}"
        ))
    })?;
    let actual = Rect::new(
        actual.left,
        actual.top,
        actual.right.saturating_sub(actual.left),
        actual.bottom.saturating_sub(actual.top),
    );
    let clears_every_monitor = actual.width > 0
        && actual.height > 0
        && enumerate_monitors().is_ok_and(|monitors| {
            monitors
                .iter()
                .all(|monitor| !actual.intersects(&monitor.rect))
        });
    if !clears_every_monitor || !set_move_offscreen_marker(hwnd, window_id) {
        // Without both physical proof and a crash-surviving ownership marker,
        // retaining transition ownership is safer than claiming the park. A
        // rollback request alone is not a receipt either: verify all edges so
        // a rejected rollback cannot silently strand the HWND at the sentinel.
        let rollback_requested = Rect::new(
            original.left,
            original.top,
            original.right.saturating_sub(original.left).max(1),
            original.bottom.saturating_sub(original.top).max(1),
        );
        let rollback_verified = unsafe {
            SetWindowPos(
                hwnd,
                None,
                rollback_requested.x,
                rollback_requested.y,
                rollback_requested.width,
                rollback_requested.height,
                SWP_NOZORDER | SWP_NOACTIVATE,
            )
        }
        .is_ok()
            && {
                let mut restored = RECT::default();
                unsafe { GetWindowRect(hwnd, &mut restored) }.is_ok()
                    && positioned_rect_matches(
                        Rect::new(
                            restored.left,
                            restored.top,
                            restored.right.saturating_sub(restored.left),
                            restored.bottom.saturating_sub(restored.top),
                        ),
                        rollback_requested,
                    )
            };
        return Err(Win32Error::SetPositionFailed(format!(
            "window {window_id} did not accept a verifiable offscreen park{}",
            if rollback_verified {
                " (rollback verified)"
            } else {
                " (rollback also unverified)"
            }
        )));
    }
    // Release a monitor-overflow clip only after the window has reached the
    // sentinel and carries a recovery marker.
    let _ = crate::window_region::restore_window_region(window_id, false);
    Ok(())
}

fn position_window_flags() -> windows::Win32::UI::WindowsAndMessaging::SET_WINDOW_POS_FLAGS {
    SWP_NOACTIVATE | SWP_SHOWWINDOW
}

fn positioned_rect_matches(actual: Rect, requested: Rect) -> bool {
    let epsilon = crate::placement::EDGE_EPSILON_PX;
    (actual.x - requested.x).abs() <= epsilon
        && (actual.y - requested.y).abs() <= epsilon
        && (actual.right() - requested.right()).abs() <= epsilon
        && (actual.bottom() - requested.bottom()).abs() <= epsilon
}

/// Synchronously show, move, and resize a window to `rect`, then raise it to
/// the top of the normal (non-topmost) window band. No activation, no async.
///
/// `SWP_SHOWWINDOW` matters when the application itself hid a shown scratchpad:
/// uncloaking and positioning alone do not clear the HWND's hidden state.
/// Raising matters for a freshly-summoned scratchpad: the focus border
/// tracks the window at its own z-level, so the window must be above the
/// previously-focused window for the border to be visible. The move is
/// synchronous so the window's rect is correct immediately (the async
/// layout pass would otherwise leave it stale).
pub fn position_window(window_id: WindowId, rect: Rect) -> Result<(), Win32Error> {
    let _ = crate::window_region::restore_window_region(window_id, false);
    let hwnd = window_id_to_hwnd(window_id)?;
    unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            position_window_flags(),
        )
        .map_err(|e| {
            Win32Error::SetPositionFailed(format!("Failed to position window {}: {}", window_id, e))
        })?;
    }
    let mut actual = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut actual) }.map_err(|error| {
        Win32Error::SetPositionFailed(format!(
            "Could not verify positioned window {window_id}: {error}"
        ))
    })?;
    let actual = Rect::new(
        actual.left,
        actual.top,
        actual.right.saturating_sub(actual.left),
        actual.bottom.saturating_sub(actual.top),
    );
    if !unsafe { IsWindow(Some(hwnd)).as_bool() }
        || !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(hwnd).as_bool() }
        || !positioned_rect_matches(actual, rect)
    {
        return Err(Win32Error::SetPositionFailed(format!(
            "window {window_id} did not accept visible position {rect:?}; actual={actual:?}"
        )));
    }
    // Do not retire a pre-existing MoveOffScreen receipt here. This helper can
    // verify geometry but cannot perform the renderer size-delta repair; the
    // next exact placement consumes that evidence and clears it only after the
    // compositor repair commits.
    Ok(())
}

/// Ensure an application-hidden scratchpad is visible without activating or
/// moving it. The subsequent verified placement owns geometry.
pub fn show_window_no_activate(window_id: WindowId) -> Result<(), Win32Error> {
    let hwnd = window_id_to_hwnd(window_id)?;
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        if !IsWindow(Some(hwnd)).as_bool() || !IsWindowVisible(hwnd).as_bool() {
            return Err(Win32Error::SetPositionFailed(format!(
                "window {window_id} did not become visible"
            )));
        }
    }
    Ok(())
}

/// Raise a window to the top of the normal (non-topmost) band without moving,
/// resizing or activating it.
///
/// Focus and z-order are separate in Windows: a floating window sits above the
/// tiled band by design, so focusing a tiled column through an explicit pointer
/// action would otherwise leave the clicked window behind the float that was
/// there before. Raising is reversible — clicking the float brings it back.
pub fn raise_window(window_id: WindowId) -> Result<(), Win32Error> {
    let hwnd = window_id_to_hwnd(window_id)?;
    unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )
        .map_err(|e| {
            Win32Error::SetPositionFailed(format!("Failed to raise window {window_id}: {e}"))
        })
    }
}

#[allow(dead_code)]
pub(crate) fn move_offscreen_rect_for(rect: &Rect) -> Rect {
    Rect::new(
        MOVE_OFFSCREEN_SENTINEL_COORD,
        MOVE_OFFSCREEN_SENTINEL_COORD,
        rect.width,
        rect.height,
    )
}

fn compute_restore_rect_from_offscreen(current_rect: &Rect, work_area: &Rect) -> Rect {
    let max_width = work_area.width.max(1);
    let max_height = work_area.height.max(1);
    let width = current_rect.width.max(1).min(max_width);
    let height = current_rect.height.max(1).min(max_height);
    Rect::new(work_area.x, work_area.y, width, height)
}

fn restored_rect_is_on_a_monitor(rect: Rect, monitors: &[crate::MonitorInfo]) -> bool {
    rect.width > 0
        && rect.height > 0
        && !is_move_offscreen_sentinel_rect(&rect)
        && monitors
            .iter()
            .any(|monitor| rect.intersects(&monitor.rect))
}

fn restore_window_if_offscreen_to_work_area(
    window_id: WindowId,
    work_area: &Rect,
) -> Result<bool, Win32Error> {
    let hwnd = window_id_to_hwnd(window_id)?;

    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return Err(Win32Error::WindowNotFound(window_id));
        }

        let mut current_rect = RECT::default();
        GetWindowRect(hwnd, &mut current_rect).map_err(|e| {
            Win32Error::SetPositionFailed(format!(
                "GetWindowRect failed for window {}: {}",
                window_id, e
            ))
        })?;

        let current_rect = Rect::new(
            current_rect.left,
            current_rect.top,
            current_rect.right - current_rect.left,
            current_rect.bottom - current_rect.top,
        );

        if !has_move_offscreen_ownership(window_id)
            && !is_move_offscreen_sentinel_rect(&current_rect)
        {
            return Ok(false);
        }

        let restore_rect = compute_restore_rect_from_offscreen(&current_rect, work_area);

        if let Err(e) = SetWindowPos(
            hwnd,
            None,
            restore_rect.x,
            restore_rect.y,
            restore_rect.width,
            restore_rect.height,
            SWP_NOZORDER | SWP_NOACTIVATE,
        ) {
            if !IsWindow(Some(hwnd)).as_bool() {
                return Err(Win32Error::WindowNotFound(window_id));
            }
            return Err(Win32Error::SetPositionFailed(format!(
                "Failed to restore off-screen window {}: {}",
                window_id, e
            )));
        }
        let mut landed = RECT::default();
        GetWindowRect(hwnd, &mut landed).map_err(|error| {
            Win32Error::SetPositionFailed(format!(
                "Could not verify restored window {window_id}: {error}"
            ))
        })?;
        let landed = Rect::new(
            landed.left,
            landed.top,
            landed.right.saturating_sub(landed.left),
            landed.bottom.saturating_sub(landed.top),
        );
        let monitors = enumerate_monitors().map_err(|error| {
            Win32Error::SetPositionFailed(format!(
                "Could not verify restore monitors for {window_id}: {error}"
            ))
        })?;
        if !restored_rect_is_on_a_monitor(landed, &monitors) {
            return Err(Win32Error::SetPositionFailed(format!(
                "window {window_id} did not accept a verified on-monitor restore; actual={landed:?}"
            )));
        }
    }
    clear_move_offscreen_marker(window_id);
    Ok(true)
}

// ============================================================================
// Restore / uncloak
// ============================================================================

/// Restore one window from MoveOffScreen sentinel coordinates to the primary monitor.
///
/// Returns `Ok(true)` if the window was restored, `Ok(false)` if it was not at
/// sentinel coordinates, and `Err` if restore operations failed.
pub fn restore_window_moved_offscreen(window_id: WindowId) -> Result<bool, Win32Error> {
    let _ = crate::window_region::restore_window_region(window_id, false);
    let primary = get_primary_monitor()?;
    restore_window_if_offscreen_to_work_area(window_id, &primary.work_area)
}

pub(crate) fn restore_windows_moved_offscreen_with_work_area<F>(
    window_ids: &[WindowId],
    work_area: &Rect,
    mut restore_one: F,
) -> (usize, Vec<String>)
where
    F: FnMut(WindowId, &Rect) -> Result<bool, Win32Error>,
{
    let mut restored_count: usize = 0;
    let mut failures: Vec<String> = Vec::new();

    for &window_id in window_ids {
        match restore_one(window_id, work_area) {
            Ok(true) => restored_count += 1,
            Ok(false) => {}
            Err(e) if is_benign_side_effect_error(&e) => {
                tracing::debug!(
                    "Ignoring benign race during MoveOffScreen restore for {}: {}",
                    window_id,
                    e
                );
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to restore off-screen window {} during shutdown recovery: {}",
                    window_id,
                    e
                );
                failures.push(format!("window {}: {}", window_id, e));
            }
        }
    }

    (restored_count, failures)
}

/// Restore all windows currently parked at MoveOffScreen sentinel coordinates.
///
/// Returns the number of restored windows. If any window restore fails, this
/// returns an aggregated error after attempting all windows.
pub fn restore_windows_moved_offscreen(window_ids: &[WindowId]) -> Result<usize, Win32Error> {
    if window_ids.is_empty() {
        return Ok(0);
    }

    let primary = get_primary_monitor()?;
    let (restored_count, failures) = restore_windows_moved_offscreen_with_work_area(
        window_ids,
        &primary.work_area,
        restore_window_if_offscreen_to_work_area,
    );

    if !failures.is_empty() {
        return Err(combine_operation_failures(
            "Failed to restore one or more MoveOffScreen windows",
            failures,
        ));
    }

    Ok(restored_count)
}

/// Restore managed windows to their visible positions, best-effort.
///
/// Resets border colors and restores windows parked at MoveOffScreen
/// sentinel coordinates. Logs warnings for failures but never panics.
pub fn uncloak_all_managed_windows(window_ids: &[WindowId]) {
    crate::dwm_uncloak_all();

    for &wid in window_ids {
        if wid == 0 {
            continue;
        }
        let _ = reset_window_border_color(wid);
    }

    if let Err(e) = restore_windows_moved_offscreen(window_ids) {
        tracing::warn!(
            "MoveOffScreen shutdown recovery had one or more failures: {}",
            e
        );
    }

    tracing::info!(
        "Restored {} managed windows during shutdown",
        window_ids.len()
    );
}

/// Restore any top-level window parked at MoveOffScreen sentinel coordinates.
///
/// This helper is panic-safe and best-effort, making it suitable for panic
/// hooks where daemon state may be unavailable or poisoned.
pub fn restore_all_windows_moved_offscreen_best_effort() -> usize {
    let primary = match get_primary_monitor() {
        Ok(primary) => primary,
        Err(e) => {
            eprintln!(
                "[leopardwm] Emergency MoveOffScreen restore skipped: no primary monitor ({})",
                e
            );
            return 0;
        }
    };

    let window_ids = collect_all_top_level_window_ids();
    let (restored_count, failures) = restore_windows_moved_offscreen_with_work_area(
        &window_ids,
        &primary.work_area,
        restore_window_if_offscreen_to_work_area,
    );

    if !failures.is_empty() {
        eprintln!(
            "[leopardwm] Emergency MoveOffScreen restore had {} hard failure(s)",
            failures.len()
        );
    }

    if restored_count > 0 {
        eprintln!(
            "[leopardwm] Emergency MoveOffScreen restore recovered {} window(s)",
            restored_count
        );
    }

    restored_count
}

/// Restore all visible windows on the system, best-effort.
///
/// Restores any windows parked at MoveOffScreen sentinel coordinates.
/// This does not require AppState and works even if state is poisoned,
/// making it suitable for use in panic hooks.
pub fn uncloak_all_visible_windows() {
    crate::dwm_uncloak_all();
    let _ = restore_all_windows_moved_offscreen_best_effort();
    // eprintln because tracing may not work in a panic hook
    eprintln!("[leopardwm] Emergency window restore complete");
}

/// Cascade windows starting at (0, 0) on the primary monitor work area.
///
/// Each window is sized to 60% of the work area and offset by 30px from the
/// previous one. Off-screen windows are first restored, then cascaded.
pub fn cascade_windows(window_ids: &[WindowId]) {
    let work_area = match get_primary_monitor() {
        Ok(m) => m.work_area,
        Err(_) => Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        },
    };

    let _ = restore_all_windows_moved_offscreen_best_effort();

    // Use height as the base so windows look reasonable on ultrawide monitors
    let cascade_h = (work_area.height as f64 * 0.5) as i32;
    let cascade_w = (cascade_h as f64 * 1.33) as i32; // 4:3 aspect ratio
    let step = 30;

    let placements: Vec<WindowPlacement> = window_ids
        .iter()
        .enumerate()
        .map(|(i, &wid)| {
            let offset = (i as i32) * step;
            WindowPlacement {
                window_id: wid,
                rect: Rect {
                    x: work_area.x + offset,
                    y: work_area.y + offset,
                    width: cascade_w,
                    height: cascade_h,
                },
                visibility: Visibility::Visible,
                column_index: 0,
            }
        })
        .collect();

    for &wid in window_ids {
        let hwnd = HWND(wid as *mut c_void);
        unsafe {
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
        }
    }

    let _ = apply_placements(&placements, &PlatformConfig::default(), None, false);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_position_window_flags_show_without_activation() {
        let flags = position_window_flags();
        assert_ne!(flags.0 & SWP_SHOWWINDOW.0, 0);
        assert_ne!(flags.0 & SWP_NOACTIVATE.0, 0);
    }

    #[test]
    fn positioned_rect_requires_all_four_edges() {
        let requested = Rect::new(100, 200, 800, 600);
        assert!(positioned_rect_matches(requested, requested));
        assert!(positioned_rect_matches(
            Rect::new(102, 198, 800, 600),
            requested
        ));
        assert!(!positioned_rect_matches(
            Rect::new(103, 200, 800, 600),
            requested
        ));
        assert!(!positioned_rect_matches(
            Rect::new(100, 200, 796, 600),
            requested
        ));
    }

    #[test]
    fn test_is_benign_side_effect_error_only_for_nonzero_not_found() {
        assert!(is_benign_side_effect_error(&Win32Error::WindowNotFound(
            123
        )));
        assert!(!is_benign_side_effect_error(&Win32Error::WindowNotFound(0)));
        assert!(!is_benign_side_effect_error(
            &Win32Error::SetPositionFailed("hard failure".to_string())
        ));
    }

    #[test]
    fn test_restore_windows_moved_offscreen_with_work_area_ignores_benign_races() {
        let window_ids = [10, 20, 30];
        let work_area = Rect::new(0, 0, 1920, 1080);
        let mut seen: Vec<WindowId> = Vec::new();
        let (restored, failures) = restore_windows_moved_offscreen_with_work_area(
            &window_ids,
            &work_area,
            |window_id, _| {
                seen.push(window_id);
                match window_id {
                    10 => Ok(true),
                    20 => Err(Win32Error::WindowNotFound(20)),
                    30 => Ok(false),
                    _ => unreachable!(),
                }
            },
        );

        assert_eq!(seen, window_ids);
        assert_eq!(restored, 1);
        assert!(failures.is_empty());
    }

    #[test]
    fn test_restore_windows_moved_offscreen_with_work_area_reports_hard_failures() {
        let window_ids = [7, 8];
        let work_area = Rect::new(0, 0, 1920, 1080);
        let (restored, failures) = restore_windows_moved_offscreen_with_work_area(
            &window_ids,
            &work_area,
            |window_id, _| match window_id {
                7 => Ok(true),
                8 => Err(Win32Error::SetPositionFailed("boom".to_string())),
                _ => unreachable!(),
            },
        );

        assert_eq!(restored, 1);
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("window 8"));
        assert!(failures[0].contains("boom"));
    }

    #[test]
    fn test_move_offscreen_sentinel_detection() {
        assert!(is_move_offscreen_sentinel_position(
            MOVE_OFFSCREEN_SENTINEL_COORD,
            MOVE_OFFSCREEN_SENTINEL_COORD
        ));
        assert!(is_move_offscreen_sentinel_position(-32_768, -32_768));
        assert!(is_move_offscreen_sentinel_position(
            EFFECTIVE_SENTINEL_THRESHOLD,
            EFFECTIVE_SENTINEL_THRESHOLD
        ));
        assert!(!is_move_offscreen_sentinel_position(
            EFFECTIVE_SENTINEL_THRESHOLD + 1,
            EFFECTIVE_SENTINEL_THRESHOLD
        ));
        assert!(!is_move_offscreen_sentinel_position(
            EFFECTIVE_SENTINEL_THRESHOLD,
            EFFECTIVE_SENTINEL_THRESHOLD + 1
        ));
    }

    #[test]
    fn test_move_offscreen_sentinel_does_not_match_minimized_coordinates() {
        // Windows commonly reports minimized windows around (-32000, -32000).
        assert!(!is_move_offscreen_sentinel_position(-32_000, -32_000));
    }

    #[test]
    fn test_move_offscreen_restore_rect_clamps_size() {
        let offscreen = Rect::new(
            MOVE_OFFSCREEN_SENTINEL_COORD,
            MOVE_OFFSCREEN_SENTINEL_COORD,
            5000,
            0,
        );
        let work_area = Rect::new(100, 200, 1920, 1080);
        let restored = compute_restore_rect_from_offscreen(&offscreen, &work_area);

        assert_eq!(restored.x, 100);
        assert_eq!(restored.y, 200);
        assert_eq!(restored.width, 1920);
        assert_eq!(restored.height, 1);
        assert!(is_move_offscreen_sentinel_rect(&offscreen));
        assert!(!is_move_offscreen_sentinel_rect(&restored));
    }

    #[test]
    fn test_restore_windows_moved_offscreen_empty_list() {
        let result = restore_windows_moved_offscreen(&[]);
        assert_eq!(result.unwrap(), 0);
    }

    #[test]
    fn test_uncloak_all_managed_empty_list() {
        // Should not panic with an empty list
        uncloak_all_managed_windows(&[]);
    }

    #[test]
    #[ignore = "Calls real Win32 APIs against literal HWND values (999_999, 1_234_567) \
                that may collide with a live window on a running daemon and move it if \
                parked at MoveOffScreen sentinel coords. Run with: cargo test -- --ignored"]
    fn test_uncloak_all_managed_with_invalid_ids() {
        // Should not panic even with invalid window IDs (best-effort)
        uncloak_all_managed_windows(&[0, 999_999, 1_234_567]);
    }

    #[test]
    #[ignore = "Enumerates all system windows and moves any parked at MoveOffScreen sentinel \
                coords back to the primary monitor work area. Safe to run in isolation but \
                disrupts a concurrently-running daemon (mass retile + Chromium swap-chain \
                desync). Run with: cargo test -- --ignored"]
    fn test_uncloak_all_visible_windows_no_panic() {
        // EnumWindows should succeed; uncloaking random windows is best-effort
        uncloak_all_visible_windows();
    }
}
