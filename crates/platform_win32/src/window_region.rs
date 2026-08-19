//! Owned `SetWindowRgn` clipping for tiled windows that cross monitor bounds.
//!
//! The module never overwrites an application-defined region. LeopardWM marks
//! every region it owns with versioned HWND properties and stores the exact
//! local rectangle there, allowing a later process to recover a stale region
//! after an abnormal exit without clearing a region that the application has
//! subsequently replaced.

use crate::{recover_poisoned_mutex, window_id_to_hwnd};
use leopardwm_core_layout::{Rect, Visibility, WindowId};
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};
use windows::core::w;
use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::Graphics::Gdi::{
    CreateRectRgn, DeleteObject, EqualRgn, GetWindowRgn, HGDIOBJ,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetPropW, GetWindowThreadProcessId, IsWindow, RemovePropW, SetPropW,
    SetWindowRgn,
};

const ERROR_REGION_KIND: i32 = 0;
const NULL_REGION_KIND: i32 = 1;
const OWNER_MAGIC: usize = 0x4c57_4d32; // "LWM2"

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowRegionClip {
    pub window_id: WindowId,
    /// Screen-coordinate output rectangle that the HWND may paint into.
    pub clip_bounds: Rect,
    /// Safe placement used if the application already owns a custom region or
    /// the region API fails. Focused windows normally use a contained visible
    /// rectangle; non-focused windows use an off-screen parked rectangle.
    pub fallback_rect: Rect,
    pub fallback_visibility: Visibility,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegionClipResult {
    Applied,
    Unchanged,
    Unsupported,
    Failed,
}

impl RegionClipResult {
    pub(crate) fn succeeded(self) -> bool {
        matches!(self, Self::Applied | Self::Unchanged)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WindowIdentity {
    process_id: u32,
    thread_id: u32,
    class_name: String,
}

#[derive(Debug, Clone)]
struct RegionState {
    identity: WindowIdentity,
    expected_region: Rect,
}

static REGION_STATES: OnceLock<Mutex<HashMap<WindowId, RegionState>>> = OnceLock::new();
static REGION_COMMIT: Mutex<()> = Mutex::new(());

fn states() -> &'static Mutex<HashMap<WindowId, RegionState>> {
    REGION_STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_states() -> std::sync::MutexGuard<'static, HashMap<WindowId, RegionState>> {
    states().lock().unwrap_or_else(recover_poisoned_mutex)
}

fn lock_commit() -> std::sync::MutexGuard<'static, ()> {
    REGION_COMMIT
        .lock()
        .unwrap_or_else(recover_poisoned_mutex)
}

fn identity(window_id: WindowId) -> Option<WindowIdentity> {
    let hwnd = window_id_to_hwnd(window_id).ok()?;
    if !unsafe { IsWindow(Some(hwnd)).as_bool() } {
        return None;
    }

    let mut process_id = 0u32;
    let thread_id = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
    if thread_id == 0 || process_id == 0 {
        return None;
    }

    let mut class = [0u16; 256];
    let len = unsafe { GetClassNameW(hwnd, &mut class) };
    if len <= 0 {
        return None;
    }

    Some(WindowIdentity {
        process_id,
        thread_id,
        class_name: String::from_utf16_lossy(&class[..len as usize]),
    })
}

fn is_same_window(window_id: WindowId, expected: &WindowIdentity) -> bool {
    identity(window_id).as_ref() == Some(expected)
}

fn handle_from_usize(value: usize) -> HANDLE {
    HANDLE(value as *mut c_void)
}

fn usize_from_handle(value: HANDLE) -> usize {
    value.0 as usize
}

fn encode_coordinate(value: i32) -> HANDLE {
    // Bias into 1..=2^32 so zero remains reserved for a missing property.
    let biased = (i64::from(value) - i64::from(i32::MIN) + 1) as u64;
    handle_from_usize(biased as usize)
}

fn decode_coordinate(value: HANDLE) -> Option<i32> {
    let raw = usize_from_handle(value) as u64;
    if raw == 0 || raw > u64::from(u32::MAX) + 1 {
        return None;
    }
    let decoded = raw as i64 - 1 + i64::from(i32::MIN);
    i32::try_from(decoded).ok()
}

fn has_owner_marker(hwnd: HWND) -> bool {
    usize_from_handle(unsafe { GetPropW(hwnd, w!("LeopardWM.RegionClip.v2.Owner")) })
        == OWNER_MAGIC
}

fn remove_metadata(hwnd: HWND) {
    unsafe {
        let _ = RemovePropW(hwnd, w!("LeopardWM.RegionClip.v2.Owner"));
        let _ = RemovePropW(hwnd, w!("LeopardWM.RegionClip.v2.Left"));
        let _ = RemovePropW(hwnd, w!("LeopardWM.RegionClip.v2.Top"));
        let _ = RemovePropW(hwnd, w!("LeopardWM.RegionClip.v2.Right"));
        let _ = RemovePropW(hwnd, w!("LeopardWM.RegionClip.v2.Bottom"));
    }
}

fn write_metadata(hwnd: HWND, rect: Rect) -> bool {
    let values = unsafe {
        [
            SetPropW(
                hwnd,
                w!("LeopardWM.RegionClip.v2.Owner"),
                handle_from_usize(OWNER_MAGIC),
            ),
            SetPropW(
                hwnd,
                w!("LeopardWM.RegionClip.v2.Left"),
                encode_coordinate(rect.x),
            ),
            SetPropW(
                hwnd,
                w!("LeopardWM.RegionClip.v2.Top"),
                encode_coordinate(rect.y),
            ),
            SetPropW(
                hwnd,
                w!("LeopardWM.RegionClip.v2.Right"),
                encode_coordinate(rect.right()),
            ),
            SetPropW(
                hwnd,
                w!("LeopardWM.RegionClip.v2.Bottom"),
                encode_coordinate(rect.bottom()),
            ),
        ]
    };
    if values.into_iter().all(|result| result.is_ok()) {
        true
    } else {
        remove_metadata(hwnd);
        false
    }
}

fn read_metadata(hwnd: HWND) -> Option<Rect> {
    if !has_owner_marker(hwnd) {
        return None;
    }
    let left = decode_coordinate(unsafe { GetPropW(hwnd, w!("LeopardWM.RegionClip.v2.Left")) })?;
    let top = decode_coordinate(unsafe { GetPropW(hwnd, w!("LeopardWM.RegionClip.v2.Top")) })?;
    let right =
        decode_coordinate(unsafe { GetPropW(hwnd, w!("LeopardWM.RegionClip.v2.Right")) })?;
    let bottom =
        decode_coordinate(unsafe { GetPropW(hwnd, w!("LeopardWM.RegionClip.v2.Bottom")) })?;
    if right <= left || bottom <= top {
        return None;
    }
    Some(Rect::new(left, top, right - left, bottom - top))
}

fn create_region(rect: Rect) -> Option<windows::Win32::Graphics::Gdi::HRGN> {
    unsafe { CreateRectRgn(rect.x, rect.y, rect.right(), rect.bottom()) }.ok()
}

fn delete_region(region: windows::Win32::Graphics::Gdi::HRGN) {
    unsafe {
        let _ = DeleteObject(HGDIOBJ(region.0));
    }
}

fn current_region_kind(hwnd: HWND) -> i32 {
    let Some(region) = create_region(Rect::new(0, 0, 1, 1)) else {
        return ERROR_REGION_KIND;
    };
    let kind = unsafe { GetWindowRgn(hwnd, region) };
    delete_region(region);
    kind
}

fn actual_region_matches(hwnd: HWND, expected: Rect) -> bool {
    let Some(actual) = create_region(Rect::new(0, 0, 1, 1)) else {
        return false;
    };
    let kind = unsafe { GetWindowRgn(hwnd, actual) };
    if kind <= NULL_REGION_KIND {
        delete_region(actual);
        return false;
    }
    let Some(expected_region) = create_region(expected) else {
        delete_region(actual);
        return false;
    };
    let equal = unsafe { EqualRgn(actual, expected_region).as_bool() };
    delete_region(actual);
    delete_region(expected_region);
    equal
}

fn clear_region(hwnd: HWND, redraw: bool) -> bool {
    unsafe { SetWindowRgn(hwnd, None, redraw) != 0 }
}

/// Remove metadata left by another LeopardWM instance. The actual window region
/// is cleared only when it exactly matches the rectangle encoded in the HWND
/// properties; an application-owned replacement is never removed.
fn recover_stale_metadata(hwnd: HWND, redraw: bool) -> bool {
    if !has_owner_marker(hwnd) {
        return true;
    }
    let expected = read_metadata(hwnd);
    let recovered = match expected {
        Some(rect) if actual_region_matches(hwnd, rect) => clear_region(hwnd, redraw),
        _ => true,
    };
    if recovered {
        remove_metadata(hwnd);
    }
    recovered
}

fn window_has_no_region(hwnd: HWND) -> bool {
    current_region_kind(hwnd) == NULL_REGION_KIND
}

/// Compute the HWND-local region that exposes only the portion of the visible
/// DWM frame inside `clip_bounds`. Unclipped edges retain their outer frame and
/// shadow; only an edge that actually crosses the output boundary is cut.
pub(crate) fn relative_clip_region(
    outer_rect: Rect,
    visible_rect: Rect,
    clip_bounds: Rect,
) -> Option<Rect> {
    let intersection_left = visible_rect.x.max(clip_bounds.x);
    let intersection_top = visible_rect.y.max(clip_bounds.y);
    let intersection_right = visible_rect.right().min(clip_bounds.right());
    let intersection_bottom = visible_rect.bottom().min(clip_bounds.bottom());
    if intersection_right <= intersection_left || intersection_bottom <= intersection_top {
        return None;
    }

    let outer_width = outer_rect.width.max(1);
    let outer_height = outer_rect.height.max(1);
    let left = if visible_rect.x >= clip_bounds.x {
        0
    } else {
        intersection_left
            .saturating_sub(outer_rect.x)
            .clamp(0, outer_width)
    };
    let top = if visible_rect.y >= clip_bounds.y {
        0
    } else {
        intersection_top
            .saturating_sub(outer_rect.y)
            .clamp(0, outer_height)
    };
    let right = if visible_rect.right() <= clip_bounds.right() {
        outer_width
    } else {
        intersection_right
            .saturating_sub(outer_rect.x)
            .clamp(left, outer_width)
    };
    let bottom = if visible_rect.bottom() <= clip_bounds.bottom() {
        outer_height
    } else {
        intersection_bottom
            .saturating_sub(outer_rect.y)
            .clamp(top, outer_height)
    };
    if right <= left || bottom <= top {
        return None;
    }
    Some(Rect::new(left, top, right - left, bottom - top))
}

/// Whether an HWND can be safely clipped without replacing an application-owned
/// custom region. Unsupported windows are deliberately re-probed on later
/// frames so an application that removes its temporary region can recover.
pub(crate) fn can_clip_window_region(window_id: WindowId) -> bool {
    let _commit = lock_commit();
    let Some(current_identity) = identity(window_id) else {
        lock_states().remove(&window_id);
        return false;
    };
    let Ok(hwnd) = window_id_to_hwnd(window_id) else {
        return false;
    };

    if let Some(state) = lock_states().get(&window_id).cloned() {
        if state.identity == current_identity
            && has_owner_marker(hwnd)
            && actual_region_matches(hwnd, state.expected_region)
        {
            return true;
        }
        lock_states().remove(&window_id);
        // The application may have replaced our region. Remove only our
        // metadata; do not clear a region whose shape no longer matches.
        if has_owner_marker(hwnd) {
            remove_metadata(hwnd);
        }
    }

    if !recover_stale_metadata(hwnd, false) {
        return false;
    }
    window_has_no_region(hwnd)
}

/// Install or update a LeopardWM-owned region. `redraw` should be false for an
/// intermediate animation frame and true for the exact landing pass.
pub(crate) fn apply_window_region_clip(
    window_id: WindowId,
    outer_rect: Rect,
    visible_rect: Rect,
    clip_bounds: Rect,
    redraw: bool,
) -> RegionClipResult {
    let Some(expected_region) = relative_clip_region(outer_rect, visible_rect, clip_bounds) else {
        return RegionClipResult::Unsupported;
    };

    let _commit = lock_commit();
    let Some(current_identity) = identity(window_id) else {
        lock_states().remove(&window_id);
        return RegionClipResult::Failed;
    };
    let Ok(hwnd) = window_id_to_hwnd(window_id) else {
        return RegionClipResult::Failed;
    };

    if let Some(state) = lock_states().get(&window_id).cloned() {
        if state.identity == current_identity
            && state.expected_region == expected_region
            && has_owner_marker(hwnd)
            && actual_region_matches(hwnd, expected_region)
        {
            return RegionClipResult::Unchanged;
        }

        lock_states().remove(&window_id);
        if has_owner_marker(hwnd) {
            if state.identity == current_identity
                && actual_region_matches(hwnd, state.expected_region)
            {
                if !clear_region(hwnd, false) {
                    lock_states().insert(window_id, state);
                    return RegionClipResult::Failed;
                }
            }
            remove_metadata(hwnd);
        }
    }

    if !recover_stale_metadata(hwnd, false) {
        return RegionClipResult::Failed;
    }
    // Re-check immediately before ownership transfer to minimize the race with
    // an application installing its own region.
    if !window_has_no_region(hwnd) {
        return RegionClipResult::Unsupported;
    }
    if !write_metadata(hwnd, expected_region) {
        return RegionClipResult::Failed;
    }
    let Some(region) = create_region(expected_region) else {
        remove_metadata(hwnd);
        return RegionClipResult::Failed;
    };

    if unsafe { SetWindowRgn(hwnd, Some(region), redraw) } == 0 {
        delete_region(region);
        remove_metadata(hwnd);
        return RegionClipResult::Failed;
    }
    // On success Windows owns `region`; deleting it would invalidate the shape.
    lock_states().insert(
        window_id,
        RegionState {
            identity: current_identity,
            expected_region,
        },
    );
    RegionClipResult::Applied
}

/// Restore a window only when its current region is still the exact region
/// LeopardWM installed. If the application has replaced it, metadata and state
/// are relinquished without touching the application's region.
pub(crate) fn restore_window_region(window_id: WindowId, redraw: bool) -> bool {
    let _commit = lock_commit();
    let state = lock_states().get(&window_id).cloned();
    let Ok(hwnd) = window_id_to_hwnd(window_id) else {
        lock_states().remove(&window_id);
        return true;
    };
    if !unsafe { IsWindow(Some(hwnd)).as_bool() } {
        lock_states().remove(&window_id);
        return true;
    }

    let expected = state
        .as_ref()
        .filter(|state| is_same_window(window_id, &state.identity))
        .map(|state| state.expected_region)
        .or_else(|| read_metadata(hwnd));

    let Some(expected) = expected else {
        lock_states().remove(&window_id);
        if has_owner_marker(hwnd) {
            remove_metadata(hwnd);
        }
        return true;
    };

    if actual_region_matches(hwnd, expected) {
        if !clear_region(hwnd, redraw) {
            return false;
        }
    }
    remove_metadata(hwnd);
    lock_states().remove(&window_id);
    true
}

/// Restore regions for managed placements that no longer request clipping, and
/// for tracked windows that have left the active layout entirely.
pub(crate) fn reconcile_window_regions(
    managed_window_ids: &HashSet<WindowId>,
    clipped_window_ids: &HashSet<WindowId>,
    redraw: bool,
) {
    for window_id in managed_window_ids.difference(clipped_window_ids) {
        let _ = restore_window_region(*window_id, redraw);
    }
    let stale: Vec<WindowId> = lock_states()
        .keys()
        .filter(|window_id| !managed_window_ids.contains(window_id))
        .copied()
        .collect();
    for window_id in stale {
        let _ = restore_window_region(window_id, redraw);
    }
}

/// Restore every region tracked by this LeopardWM process. Called by normal
/// shutdown, panic recovery, emergency uncloak, and empty-layout cleanup.
pub fn restore_all_window_regions() {
    let window_ids: Vec<WindowId> = lock_states().keys().copied().collect();
    for window_id in window_ids {
        let _ = restore_window_region(window_id, true);
    }
}

/// Forget a destroyed HWND without issuing Win32 calls that could affect a new
/// window reusing the same numeric handle.
pub fn forget_window_region(window_id: WindowId) {
    lock_states().remove(&window_id);
}

#[cfg(test)]
mod tests {
    use super::{decode_coordinate, encode_coordinate, relative_clip_region};
    use leopardwm_core_layout::Rect;

    #[test]
    fn coordinate_property_encoding_round_trips_extremes() {
        for value in [i32::MIN, -100_000, -1, 0, 1, 100_000, i32::MAX] {
            assert_eq!(decode_coordinate(encode_coordinate(value)), Some(value));
        }
    }

    #[test]
    fn right_edge_clip_preserves_the_unclipped_outer_frame() {
        let region = relative_clip_region(
            Rect::new(1792, 90, 616, 916),
            Rect::new(1800, 100, 600, 900),
            Rect::new(0, 0, 1920, 1080),
        )
        .unwrap();
        assert_eq!(region, Rect::new(0, 0, 128, 916));
    }

    #[test]
    fn left_edge_clip_preserves_the_unclipped_outer_frame() {
        let region = relative_clip_region(
            Rect::new(-208, 90, 616, 916),
            Rect::new(-200, 100, 600, 900),
            Rect::new(0, 0, 1920, 1080),
        )
        .unwrap();
        assert_eq!(region, Rect::new(208, 0, 408, 916));
    }

    #[test]
    fn clips_vertical_neighbors_and_negative_virtual_coordinates() {
        let region = relative_clip_region(
            Rect::new(-1930, -110, 820, 1200),
            Rect::new(-1920, -100, 800, 1180),
            Rect::new(-1920, 0, 1920, 1080),
        )
        .unwrap();
        assert_eq!(region, Rect::new(10, 110, 810, 1080));
    }

    #[test]
    fn returns_full_outer_frame_when_visible_frame_is_inside_bounds() {
        let region = relative_clip_region(
            Rect::new(92, 90, 816, 916),
            Rect::new(100, 100, 800, 900),
            Rect::new(0, 0, 1920, 1080),
        )
        .unwrap();
        assert_eq!(region, Rect::new(0, 0, 816, 916));
    }

    #[test]
    fn rejects_a_window_with_no_visible_intersection() {
        assert!(relative_clip_region(
            Rect::new(2100, 0, 400, 800),
            Rect::new(2100, 0, 400, 800),
            Rect::new(0, 0, 1920, 1080),
        )
        .is_none());
    }
}
