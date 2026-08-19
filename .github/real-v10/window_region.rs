//! Conservative ownership of temporary rectangular window regions.
//!
//! A region is installed only on an unregioned top-level HWND. LeopardWM marks
//! every owned region with HWND properties containing the exact rectangle, so a
//! later process can recover a region left by an abnormal exit without clearing
//! an application-owned replacement. Unsupported/custom-region windows fall
//! back to the daemon's whole-window isolation policy.

use crate::{get_window_info, recover_poisoned_mutex, window_id_to_hwnd};
use leopardwm_core_layout::{Rect, WindowId};
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::core::w;
use windows::Win32::Foundation::{HANDLE, HWND, RECT};
use windows::Win32::Graphics::Gdi::{CreateRectRgn, DeleteObject, EqualRgn, HGDIOBJ};
use windows::Win32::UI::WindowsAndMessaging::{
    GetPropW, GetWindowRgn, GetWindowRgnBox, IsWindow, RemovePropW, SetPropW, SetWindowRgn,
};

const ERROR_REGION_KIND: i32 = 0;
const NULL_REGION_KIND: i32 = 1;
const REGION_VERIFY_INTERVAL: Duration = Duration::from_millis(250);
const OWNER_MARKER: HANDLE = HANDLE(0x4c57_4d52usize as *mut c_void);

const OWNER_PROP: windows::core::PCWSTR = w!("LeopardWM.RegionClip.Owner.v2");
const LEFT_PROP: windows::core::PCWSTR = w!("LeopardWM.RegionClip.Left.v2");
const TOP_PROP: windows::core::PCWSTR = w!("LeopardWM.RegionClip.Top.v2");
const RIGHT_PROP: windows::core::PCWSTR = w!("LeopardWM.RegionClip.Right.v2");
const BOTTOM_PROP: windows::core::PCWSTR = w!("LeopardWM.RegionClip.Bottom.v2");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WindowIdentity {
    process_id: u32,
}

#[derive(Debug, Clone)]
struct RegionState {
    identity: WindowIdentity,
    region: Rect,
    last_verified: Instant,
}

static REGION_COMMIT: Mutex<()> = Mutex::new(());
static REGION_STATES: OnceLock<Mutex<HashMap<WindowId, RegionState>>> = OnceLock::new();

fn states() -> &'static Mutex<HashMap<WindowId, RegionState>> {
    REGION_STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_commit() -> std::sync::MutexGuard<'static, ()> {
    REGION_COMMIT
        .lock()
        .unwrap_or_else(recover_poisoned_mutex)
}

fn lock_states() -> std::sync::MutexGuard<'static, HashMap<WindowId, RegionState>> {
    states().lock().unwrap_or_else(recover_poisoned_mutex)
}

fn identity(window_id: WindowId) -> Option<WindowIdentity> {
    let info = get_window_info(window_id).ok()?;
    Some(WindowIdentity {
        process_id: info.process_id,
    })
}

fn live_hwnd(window_id: WindowId) -> Option<HWND> {
    let hwnd = window_id_to_hwnd(window_id).ok()?;
    unsafe { IsWindow(Some(hwnd)).as_bool() }.then_some(hwnd)
}

fn encode_i32(value: i32) -> HANDLE {
    let raw = u32::from_ne_bytes(value.to_ne_bytes()) as usize;
    HANDLE(raw.wrapping_add(1) as *mut c_void)
}

fn decode_i32(value: HANDLE) -> Option<i32> {
    let raw = value.0 as usize;
    (raw != 0).then(|| {
        let bits = raw.wrapping_sub(1) as u32;
        i32::from_ne_bytes(bits.to_ne_bytes())
    })
}

fn prop(hwnd: HWND, name: windows::core::PCWSTR) -> HANDLE {
    unsafe { GetPropW(hwnd, name) }
}

fn remove_prop(hwnd: HWND, name: windows::core::PCWSTR) {
    unsafe {
        let _ = RemovePropW(hwnd, name);
    }
}

fn clear_marker(hwnd: HWND) {
    for name in [OWNER_PROP, LEFT_PROP, TOP_PROP, RIGHT_PROP, BOTTOM_PROP] {
        remove_prop(hwnd, name);
    }
}

fn marker_rect(hwnd: HWND) -> Option<Rect> {
    if prop(hwnd, OWNER_PROP) != OWNER_MARKER {
        return None;
    }
    let left = decode_i32(prop(hwnd, LEFT_PROP))?;
    let top = decode_i32(prop(hwnd, TOP_PROP))?;
    let right = decode_i32(prop(hwnd, RIGHT_PROP))?;
    let bottom = decode_i32(prop(hwnd, BOTTOM_PROP))?;
    (right > left && bottom > top).then(|| Rect::new(left, top, right - left, bottom - top))
}

fn write_marker(hwnd: HWND, rect: Rect) -> bool {
    let values = [
        (LEFT_PROP, encode_i32(rect.x)),
        (TOP_PROP, encode_i32(rect.y)),
        (RIGHT_PROP, encode_i32(rect.right())),
        (BOTTOM_PROP, encode_i32(rect.bottom())),
        (OWNER_PROP, OWNER_MARKER),
    ];
    for (name, value) in values {
        if unsafe { SetPropW(hwnd, name, value) }.is_err() {
            clear_marker(hwnd);
            return false;
        }
    }
    true
}

fn clear_region(hwnd: HWND, redraw: bool) -> bool {
    unsafe { SetWindowRgn(hwnd, None, redraw) != 0 }
}

fn window_has_no_region(hwnd: HWND) -> bool {
    let mut bounds = RECT::default();
    unsafe { GetWindowRgnBox(hwnd, &mut bounds) } == NULL_REGION_KIND
}

fn current_region_matches(hwnd: HWND, expected: Rect) -> bool {
    let Ok(actual) = (unsafe { CreateRectRgn(0, 0, 0, 0) }) else {
        return false;
    };
    let Ok(wanted) = (unsafe {
        CreateRectRgn(
            expected.x,
            expected.y,
            expected.right(),
            expected.bottom(),
        )
    }) else {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(actual.0));
        }
        return false;
    };

    let kind = unsafe { GetWindowRgn(hwnd, actual) };
    let equal = kind != ERROR_REGION_KIND && unsafe { EqualRgn(actual, wanted).as_bool() };
    unsafe {
        let _ = DeleteObject(HGDIOBJ(actual.0));
        let _ = DeleteObject(HGDIOBJ(wanted.0));
    }
    equal
}

/// Compute the rectangular region, in window-local coordinates, that exposes
/// only the horizontal part of `outer_rect` inside `clip_bounds`.
pub(crate) fn relative_clip_region(outer_rect: Rect, clip_bounds: Rect) -> Option<Rect> {
    let outer_width = outer_rect.width.max(1);
    let outer_height = outer_rect.height.max(1);
    let left = outer_rect.x.max(clip_bounds.x);
    let right = outer_rect.right().min(clip_bounds.right());
    if right <= left {
        return None;
    }

    let relative_left = left.saturating_sub(outer_rect.x).clamp(0, outer_width);
    let relative_right = right
        .saturating_sub(outer_rect.x)
        .clamp(relative_left, outer_width);
    (relative_right > relative_left).then(|| {
        Rect::new(
            relative_left,
            0,
            relative_right - relative_left,
            outer_height,
        )
    })
}

/// Recover a marker left by an earlier LeopardWM process. The current region is
/// cleared only when it is exactly the rectangle encoded by our properties.
fn recover_stale_marker(hwnd: HWND) -> bool {
    if prop(hwnd, OWNER_PROP) != OWNER_MARKER {
        return true;
    }
    let expected = marker_rect(hwnd);
    let owned = expected.is_some_and(|rect| current_region_matches(hwnd, rect));
    if owned && !clear_region(hwnd, true) {
        return false;
    }
    clear_marker(hwnd);
    true
}

/// Install or update a LeopardWM-owned rectangular region. On any ownership or
/// API ambiguity this fails closed, allowing the caller to use its safe fallback.
pub(crate) fn apply_window_region_clip(
    window_id: WindowId,
    outer_rect: Rect,
    clip_bounds: Rect,
    redraw: bool,
) -> bool {
    let Some(region_rect) = relative_clip_region(outer_rect, clip_bounds) else {
        return false;
    };
    let Some(hwnd) = live_hwnd(window_id) else {
        return false;
    };
    let Some(current_identity) = identity(window_id) else {
        return false;
    };

    let _commit = lock_commit();
    let previous = lock_states().get(&window_id).cloned();

    let previous = match previous {
        Some(state) if state.identity == current_identity => Some(state),
        Some(_) => {
            lock_states().remove(&window_id);
            None
        }
        None => None,
    };

    if let Some(state) = previous.as_ref() {
        if marker_rect(hwnd) != Some(state.region) {
            clear_marker(hwnd);
            lock_states().remove(&window_id);
            return false;
        }
        if state.region == region_rect
            && state.last_verified.elapsed() < REGION_VERIFY_INTERVAL
        {
            return true;
        }
        if !current_region_matches(hwnd, state.region) {
            // The application replaced our region. Relinquish ownership without
            // clearing the replacement.
            clear_marker(hwnd);
            lock_states().remove(&window_id);
            return false;
        }
        if state.region == region_rect {
            if let Some(state) = lock_states().get_mut(&window_id) {
                state.last_verified = Instant::now();
            }
            return true;
        }
    } else {
        if !recover_stale_marker(hwnd) || !window_has_no_region(hwnd) {
            return false;
        }
    }

    let old_region = previous.as_ref().map(|state| state.region);
    if !write_marker(hwnd, region_rect) {
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
        if let Some(old) = old_region {
            let _ = write_marker(hwnd, old);
        } else {
            clear_marker(hwnd);
        }
        return false;
    };

    if unsafe { SetWindowRgn(hwnd, Some(region), redraw) } == 0 {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(region.0));
        }
        if let Some(old) = old_region {
            let _ = write_marker(hwnd, old);
        } else {
            clear_marker(hwnd);
        }
        return false;
    }

    // Windows owns `region` after a successful SetWindowRgn call.
    lock_states().insert(
        window_id,
        RegionState {
            identity: current_identity,
            region: region_rect,
            last_verified: Instant::now(),
        },
    );
    true
}

/// Restore the original unregioned shape. If the application replaced our
/// region, its replacement is preserved and only our stale marker/state is removed.
pub(crate) fn restore_window_region(window_id: WindowId, redraw: bool) -> bool {
    let _commit = lock_commit();
    let Some(state) = lock_states().get(&window_id).cloned() else {
        let Some(hwnd) = live_hwnd(window_id) else {
            return true;
        };
        return recover_stale_marker(hwnd);
    };

    let Some(hwnd) = live_hwnd(window_id) else {
        lock_states().remove(&window_id);
        return true;
    };
    if identity(window_id) != Some(state.identity) {
        lock_states().remove(&window_id);
        return true;
    }
    if marker_rect(hwnd) != Some(state.region) || !current_region_matches(hwnd, state.region) {
        clear_marker(hwnd);
        lock_states().remove(&window_id);
        return true;
    }
    if !clear_region(hwnd, redraw) {
        return false;
    }
    clear_marker(hwnd);
    lock_states().remove(&window_id);
    true
}

pub(crate) fn restore_window_regions_not_in(current_ids: &HashSet<WindowId>) {
    let stale: Vec<WindowId> = lock_states()
        .keys()
        .filter(|window_id| !current_ids.contains(window_id))
        .copied()
        .collect();
    for window_id in stale {
        let _ = restore_window_region(window_id, true);
    }
}

/// Best-effort recovery for shutdown, panic-revert, and emergency-uncloak paths.
pub fn restore_all_window_regions() {
    // A second pass handles a transient first-call failure while preserving any
    // state that still cannot be safely restored for a future recovery attempt.
    for _ in 0..2 {
        let window_ids: Vec<WindowId> = lock_states().keys().copied().collect();
        if window_ids.is_empty() {
            break;
        }
        for window_id in window_ids {
            let _ = restore_window_region(window_id, true);
        }
    }
}

/// Drop bookkeeping for a destroyed HWND without issuing an API call that could
/// touch a new window reusing the same numeric handle.
pub fn forget_window_region(window_id: WindowId) {
    let _commit = lock_commit();
    lock_states().remove(&window_id);
}

#[cfg(test)]
mod tests {
    use super::{decode_i32, encode_i32, relative_clip_region};
    use leopardwm_core_layout::Rect;

    #[test]
    fn signed_property_encoding_round_trips_virtual_desktop_coordinates() {
        for value in [i32::MIN, -100_000, -1920, -1, 0, 1, 1920, i32::MAX] {
            assert_eq!(decode_i32(encode_i32(value)), Some(value));
        }
    }

    #[test]
    fn computes_right_edge_region_relative_to_outer_frame() {
        assert_eq!(
            relative_clip_region(
                Rect::new(1792, 90, 616, 916),
                Rect::new(0, 0, 1920, 1080),
            ),
            Some(Rect::new(0, 0, 128, 916)),
        );
    }

    #[test]
    fn computes_left_edge_region_relative_to_outer_frame() {
        assert_eq!(
            relative_clip_region(
                Rect::new(-208, 90, 616, 916),
                Rect::new(0, 0, 1920, 1080),
            ),
            Some(Rect::new(208, 0, 408, 916)),
        );
    }

    #[test]
    fn handles_negative_monitor_origins() {
        assert_eq!(
            relative_clip_region(
                Rect::new(-1930, -10, 820, 1100),
                Rect::new(-1920, 0, 1920, 1080),
            ),
            Some(Rect::new(10, 0, 810, 1100)),
        );
    }

    #[test]
    fn rejects_a_window_without_horizontal_intersection() {
        assert!(relative_clip_region(
            Rect::new(2100, 0, 400, 800),
            Rect::new(0, 0, 1920, 1080),
        )
        .is_none());
    }
}
