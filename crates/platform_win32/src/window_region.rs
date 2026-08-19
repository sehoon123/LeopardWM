//! Safe top-level window region clipping for multi-monitor tiling.
//!
//! LeopardWM only clips windows that had no application-defined window region.
//! A named HWND property marks regions owned by LeopardWM so a later process can
//! recover a region left behind by an abnormal termination. Unsupported or
//! protected windows are left untouched and the daemon uses its whole-window
//! fallback policy instead.

use crate::{get_window_info, recover_poisoned_mutex, window_id_to_hwnd};
use leopardwm_core_layout::{Rect, WindowId};
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};
use windows::core::w;
use windows::Win32::Foundation::{HANDLE, HWND, RECT};
use windows::Win32::Graphics::Gdi::{CreateRectRgn, DeleteObject, HGDIOBJ};
use windows::Win32::UI::WindowsAndMessaging::{
    GetPropW, GetWindowRgnBox, IsWindow, RemovePropW, SetPropW, SetWindowRgn,
};

const NULL_REGION_KIND: i32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
struct WindowIdentity {
    process_id: u32,
    class_name: String,
}

#[derive(Debug, Clone)]
struct RegionState {
    identity: WindowIdentity,
    supported: bool,
    active: bool,
    last_region: Option<Rect>,
}

static REGION_STATES: OnceLock<Mutex<HashMap<WindowId, RegionState>>> = OnceLock::new();

fn states() -> &'static Mutex<HashMap<WindowId, RegionState>> {
    REGION_STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_states() -> std::sync::MutexGuard<'static, HashMap<WindowId, RegionState>> {
    states().lock().unwrap_or_else(recover_poisoned_mutex)
}

fn identity(window_id: WindowId) -> Option<WindowIdentity> {
    let info = get_window_info(window_id).ok()?;
    Some(WindowIdentity {
        process_id: info.process_id,
        class_name: info.class_name,
    })
}

fn is_same_window(window_id: WindowId, expected: &WindowIdentity) -> bool {
    let Ok(hwnd) = window_id_to_hwnd(window_id) else {
        return false;
    };
    if !unsafe { IsWindow(Some(hwnd)).as_bool() } {
        return false;
    }
    identity(window_id).as_ref() == Some(expected)
}

fn marker_value() -> HANDLE {
    HANDLE(1usize as *mut c_void)
}

fn has_owner_marker(hwnd: HWND) -> bool {
    !unsafe { GetPropW(hwnd, w!("LeopardWM.RegionClip.v1")) }
        .0
        .is_null()
}

fn set_owner_marker(hwnd: HWND) -> bool {
    unsafe { SetPropW(hwnd, w!("LeopardWM.RegionClip.v1"), marker_value()).is_ok() }
}

fn remove_owner_marker(hwnd: HWND) {
    unsafe {
        let _ = RemovePropW(hwnd, w!("LeopardWM.RegionClip.v1"));
    }
}

fn clear_region(hwnd: HWND, redraw: bool) -> bool {
    unsafe { SetWindowRgn(hwnd, None, redraw) != 0 }
}

fn has_no_application_region(hwnd: HWND) -> bool {
    let mut bounds = RECT::default();
    unsafe { GetWindowRgnBox(hwnd, &mut bounds) } == NULL_REGION_KIND
}

/// Compute a horizontal window-relative region that exposes only the portion
/// of `visible_rect` inside `clip_bounds`. Vertical geometry is deliberately
/// preserved: this feature isolates side-by-side monitors without changing
/// taskbar or top/bottom behavior.
pub(crate) fn relative_clip_region(
    outer_rect: Rect,
    visible_rect: Rect,
    clip_bounds: Rect,
) -> Option<Rect> {
    let outer_width = outer_rect.width.max(1);
    let outer_height = outer_rect.height.max(1);
    let left = visible_rect.x.max(clip_bounds.x);
    let right = visible_rect.right().min(clip_bounds.right());
    if right <= left {
        return None;
    }

    let relative_left = left.saturating_sub(outer_rect.x).clamp(0, outer_width);
    let relative_right = right
        .saturating_sub(outer_rect.x)
        .clamp(relative_left, outer_width);
    if relative_right <= relative_left {
        return None;
    }

    Some(Rect::new(
        relative_left,
        0,
        relative_right - relative_left,
        outer_height,
    ))
}

/// Return whether LeopardWM may safely own a temporary region for this HWND.
///
/// Application-defined simple/complex regions are never overwritten. A stale
/// ownership marker from a previous LeopardWM crash is recovered first.
pub fn can_clip_window_region(window_id: WindowId) -> bool {
    let Some(current_identity) = identity(window_id) else {
        return false;
    };
    let Ok(hwnd) = window_id_to_hwnd(window_id) else {
        return false;
    };
    if !unsafe { IsWindow(Some(hwnd)).as_bool() } {
        return false;
    }

    let mut guard = lock_states();
    if let Some(state) = guard.get(&window_id) {
        if state.identity == current_identity {
            return state.supported;
        }
        guard.remove(&window_id);
    }

    // SetWindowRgn survives the process that called it. A property placed
    // before every owned region lets a later LeopardWM instance restore a
    // region left by an abnormal termination before re-evaluating capability.
    if has_owner_marker(hwnd) {
        if !clear_region(hwnd, true) {
            guard.insert(
                window_id,
                RegionState {
                    identity: current_identity,
                    supported: false,
                    active: true,
                    last_region: None,
                },
            );
            return false;
        }
        remove_owner_marker(hwnd);
    }

    let no_custom_region = has_no_application_region(hwnd);
    let marker_supported = if no_custom_region && set_owner_marker(hwnd) {
        remove_owner_marker(hwnd);
        true
    } else {
        false
    };
    let supported = no_custom_region && marker_supported;
    guard.insert(
        window_id,
        RegionState {
            identity: current_identity,
            supported,
            active: false,
            last_region: None,
        },
    );
    supported
}

/// Apply a LeopardWM-owned clipping region after the HWND has reached its
/// current outer rectangle. Returns false when the daemon must use its safe
/// whole-window fallback on a guarded re-apply.
pub(crate) fn apply_window_region_clip(
    window_id: WindowId,
    outer_rect: Rect,
    visible_rect: Rect,
    clip_bounds: Rect,
    redraw: bool,
) -> bool {
    let Some(region_rect) = relative_clip_region(outer_rect, visible_rect, clip_bounds) else {
        return false;
    };
    if !can_clip_window_region(window_id) {
        return false;
    }
    let Some(current_identity) = identity(window_id) else {
        return false;
    };
    let Ok(hwnd) = window_id_to_hwnd(window_id) else {
        return false;
    };

    let mut guard = lock_states();
    let Some(state) = guard.get_mut(&window_id) else {
        return false;
    };
    if state.identity != current_identity || !state.supported {
        return false;
    }
    if state.active && state.last_region == Some(region_rect) {
        return true;
    }

    let marker_was_new = !state.active;
    if marker_was_new && !set_owner_marker(hwnd) {
        state.supported = false;
        return false;
    }

    let Ok(region) = (unsafe {
        CreateRectRgn(
            region_rect.x,
            region_rect.y,
            region_rect.right(),
            region_rect.bottom(),
        )
    }) else {
        if marker_was_new {
            remove_owner_marker(hwnd);
        }
        state.supported = false;
        return false;
    };

    if unsafe { SetWindowRgn(hwnd, Some(region), redraw) } == 0 {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(region.0));
        }
        if state.active {
            let _ = clear_region(hwnd, redraw);
        }
        remove_owner_marker(hwnd);
        state.supported = false;
        state.active = false;
        state.last_region = None;
        return false;
    }

    // On success Windows owns `region`; deleting it here would invalidate the
    // window shape.
    state.active = true;
    state.last_region = Some(region_rect);
    true
}

/// Restore an HWND to its original unregioned shape. Unsupported custom-region
/// windows are no-ops because LeopardWM never modified them.
pub(crate) fn restore_window_region(window_id: WindowId, redraw: bool) -> bool {
    let mut guard = lock_states();
    let Some(state) = guard.get_mut(&window_id) else {
        return true;
    };
    if !state.active {
        return true;
    }
    if !is_same_window(window_id, &state.identity) {
        guard.remove(&window_id);
        return true;
    }
    let Ok(hwnd) = window_id_to_hwnd(window_id) else {
        guard.remove(&window_id);
        return true;
    };
    if !clear_region(hwnd, redraw) {
        return false;
    }
    remove_owner_marker(hwnd);
    state.active = false;
    state.last_region = None;
    true
}

pub(crate) fn restore_window_regions_not_in(current_ids: &HashSet<WindowId>) {
    let stale: Vec<WindowId> = {
        let guard = lock_states();
        guard
            .keys()
            .filter(|window_id| !current_ids.contains(window_id))
            .copied()
            .collect()
    };
    for window_id in stale {
        let _ = restore_window_region(window_id, true);
        let mut guard = lock_states();
        if guard
            .get(&window_id)
            .is_some_and(|state| !state.active)
        {
            guard.remove(&window_id);
        }
    }
}

/// Restore every region owned by LeopardWM. Called by normal shutdown, panic
/// recovery, emergency uncloak, and empty-layout cleanup.
pub fn restore_all_window_regions() {
    let window_ids: Vec<WindowId> = {
        let guard = lock_states();
        guard.keys().copied().collect()
    };
    for window_id in window_ids {
        let _ = restore_window_region(window_id, true);
    }
    lock_states().retain(|_, state| state.active);
}

/// Remove state for a destroyed/recycled HWND without touching a possible new
/// window that reused the numeric handle.
pub fn forget_window_region(window_id: WindowId) {
    lock_states().remove(&window_id);
}

#[cfg(test)]
mod tests {
    use super::relative_clip_region;
    use leopardwm_core_layout::Rect;

    #[test]
    fn computes_right_edge_region_relative_to_outer_frame() {
        let region = relative_clip_region(
            Rect::new(1792, 90, 616, 916),
            Rect::new(1800, 100, 600, 900),
            Rect::new(0, 0, 1920, 1040),
        )
        .unwrap();
        assert_eq!(region, Rect::new(8, 0, 120, 916));
    }

    #[test]
    fn computes_left_edge_region_relative_to_outer_frame() {
        let region = relative_clip_region(
            Rect::new(-208, 90, 616, 916),
            Rect::new(-200, 100, 600, 900),
            Rect::new(0, 0, 1920, 1040),
        )
        .unwrap();
        assert_eq!(region, Rect::new(208, 0, 400, 916));
    }

    #[test]
    fn preserves_full_outer_height_and_handles_negative_monitor_origins() {
        let region = relative_clip_region(
            Rect::new(-1930, -10, 820, 1100),
            Rect::new(-1920, 0, 800, 1080),
            Rect::new(-1920, 0, 1920, 1040),
        )
        .unwrap();
        assert_eq!(region, Rect::new(10, 0, 800, 1100));
    }

    #[test]
    fn rejects_a_window_with_no_visible_horizontal_intersection() {
        assert!(relative_clip_region(
            Rect::new(2100, 0, 400, 800),
            Rect::new(2100, 0, 400, 800),
            Rect::new(0, 0, 1920, 1080),
        )
        .is_none());
    }
}
