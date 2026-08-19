//! Recoverable `SetWindowRgn` clipping for tiled windows at monitor edges.
//!
//! LeopardWM never overwrites an application-defined window region. Every
//! region it owns is described by versioned HWND properties and is removed only
//! when the live region still exactly matches that description. This preserves
//! application ownership across HWND reuse, custom-shape changes, crashes, and
//! daemon restarts.

use crate::{recover_poisoned_mutex, window_id_to_hwnd};
use leopardwm_core_layout::{Rect, Visibility, WindowId};
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};
use windows::core::w;
use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::Graphics::Gdi::{
    CreateRectRgn, DeleteObject, EqualRgn, HGDIOBJ, HRGN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetPropW, GetWindowRgn, GetWindowThreadProcessId, IsWindow, RemovePropW,
    SetPropW, SetWindowRgn,
};

const ERROR_REGION_KIND: i32 = 0;
const NULL_REGION_KIND: i32 = 1;
const OWNER_MAGIC: usize = 0x4c57_4d33; // "LWM3"

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowRegionClip {
    pub window_id: WindowId,
    /// Screen-coordinate output rectangle that this HWND may paint into.
    pub clip_bounds: Rect,
    /// Same-frame fallback used if clipping is unsupported or fails.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegionRelation {
    NoRegion,
    Matches,
    Differs,
    QueryFailed,
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
    usize_from_handle(unsafe { GetPropW(hwnd, w!("LeopardWM.RegionClip.v3.Owner")) })
        == OWNER_MAGIC
}

fn remove_metadata(hwnd: HWND) {
    // Remove the commit marker first, then its payload.
    unsafe {
        let _ = RemovePropW(hwnd, w!("LeopardWM.RegionClip.v3.Owner"));
        let _ = RemovePropW(hwnd, w!("LeopardWM.RegionClip.v3.Left"));
        let _ = RemovePropW(hwnd, w!("LeopardWM.RegionClip.v3.Top"));
        let _ = RemovePropW(hwnd, w!("LeopardWM.RegionClip.v3.Right"));
        let _ = RemovePropW(hwnd, w!("LeopardWM.RegionClip.v3.Bottom"));
    }
}

fn write_metadata(hwnd: HWND, rect: Rect) -> bool {
    // Write the payload first and the owner marker last. Another process either
    // observes one complete committed record or no record at all.
    let payload_ok = unsafe {
        [
            SetPropW(
                hwnd,
                w!("LeopardWM.RegionClip.v3.Left"),
                encode_coordinate(rect.x),
            ),
            SetPropW(
                hwnd,
                w!("LeopardWM.RegionClip.v3.Top"),
                encode_coordinate(rect.y),
            ),
            SetPropW(
                hwnd,
                w!("LeopardWM.RegionClip.v3.Right"),
                encode_coordinate(rect.right()),
            ),
            SetPropW(
                hwnd,
                w!("LeopardWM.RegionClip.v3.Bottom"),
                encode_coordinate(rect.bottom()),
            ),
        ]
        .into_iter()
        .all(|result| result.is_ok())
    };
    let owner_ok = payload_ok
        && unsafe {
            SetPropW(
                hwnd,
                w!("LeopardWM.RegionClip.v3.Owner"),
                handle_from_usize(OWNER_MAGIC),
            )
            .is_ok()
        };
    if !owner_ok {
        remove_metadata(hwnd);
    }
    owner_ok
}

fn read_metadata(hwnd: HWND) -> Option<Rect> {
    if !has_owner_marker(hwnd) {
        return None;
    }
    let left = decode_coordinate(unsafe { GetPropW(hwnd, w!("LeopardWM.RegionClip.v3.Left")) })?;
    let top = decode_coordinate(unsafe { GetPropW(hwnd, w!("LeopardWM.RegionClip.v3.Top")) })?;
    let right =
        decode_coordinate(unsafe { GetPropW(hwnd, w!("LeopardWM.RegionClip.v3.Right")) })?;
    let bottom =
        decode_coordinate(unsafe { GetPropW(hwnd, w!("LeopardWM.RegionClip.v3.Bottom")) })?;
    if right <= left || bottom <= top {
        return None;
    }
    Some(Rect::new(left, top, right - left, bottom - top))
}

fn create_region(rect: Rect) -> Option<HRGN> {
    unsafe { CreateRectRgn(rect.x, rect.y, rect.right(), rect.bottom()) }.ok()
}

fn delete_region(region: HRGN) {
    unsafe {
        let _ = DeleteObject(HGDIOBJ(region.0));
    }
}

fn region_relation(hwnd: HWND, expected: Rect) -> RegionRelation {
    let Some(actual) = create_region(Rect::new(0, 0, 1, 1)) else {
        return RegionRelation::QueryFailed;
    };
    let kind = unsafe { GetWindowRgn(hwnd, actual) };
    match kind {
        ERROR_REGION_KIND => {
            delete_region(actual);
            RegionRelation::QueryFailed
        }
        NULL_REGION_KIND => {
            delete_region(actual);
            RegionRelation::NoRegion
        }
        _ => {
            let Some(wanted) = create_region(expected) else {
                delete_region(actual);
                return RegionRelation::QueryFailed;
            };
            let equal = unsafe { EqualRgn(actual, wanted).as_bool() };
            delete_region(actual);
            delete_region(wanted);
            if equal {
                RegionRelation::Matches
            } else {
                RegionRelation::Differs
            }
        }
    }
}

fn current_region_kind(hwnd: HWND) -> Option<i32> {
    let region = create_region(Rect::new(0, 0, 1, 1))?;
    let kind = unsafe { GetWindowRgn(hwnd, region) };
    delete_region(region);
    (kind != ERROR_REGION_KIND).then_some(kind)
}

fn clear_region(hwnd: HWND, redraw: bool) -> bool {
    unsafe { SetWindowRgn(hwnd, None, redraw) != 0 }
}

/// Recover a property-marked region left by an earlier LeopardWM process.
/// A region is cleared only when its live shape exactly equals the committed
/// metadata. Query failures retain metadata for a later retry.
fn recover_stale_metadata_locked(hwnd: HWND, redraw: bool) -> bool {
    if !has_owner_marker(hwnd) {
        return true;
    }
    let Some(expected) = read_metadata(hwnd) else {
        // Malformed ownership metadata cannot safely authorize a clear.
        remove_metadata(hwnd);
        return false;
    };
    match region_relation(hwnd, expected) {
        RegionRelation::Matches => {
            if !clear_region(hwnd, redraw) {
                return false;
            }
            remove_metadata(hwnd);
            true
        }
        RegionRelation::NoRegion => {
            remove_metadata(hwnd);
            true
        }
        RegionRelation::Differs => {
            // The application took ownership after the recorded clip.
            remove_metadata(hwnd);
            false
        }
        RegionRelation::QueryFailed => false,
    }
}

/// Compute the HWND-local region that exposes only the portion of the visible
/// DWM frame inside `clip_bounds`. Unclipped edges retain their outer frame and
/// shadow; only edges crossing the output boundary are cut.
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

/// Whether this HWND currently has no application-defined region and may be
/// clipped. Unsupported windows are re-probed on later frames so a temporary
/// application region does not permanently disable clipping.
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
        if state.identity != current_identity {
            lock_states().remove(&window_id);
            if has_owner_marker(hwnd) {
                remove_metadata(hwnd);
            }
        } else {
            match region_relation(hwnd, state.expected_region) {
                RegionRelation::Matches if has_owner_marker(hwnd) => return true,
                RegionRelation::QueryFailed => return false,
                RegionRelation::Matches | RegionRelation::NoRegion | RegionRelation::Differs => {
                    lock_states().remove(&window_id);
                    if has_owner_marker(hwnd) {
                        remove_metadata(hwnd);
                    }
                }
            }
        }
    }

    if !recover_stale_metadata_locked(hwnd, false) {
        return false;
    }
    current_region_kind(hwnd) == Some(NULL_REGION_KIND)
}

/// Install or update a LeopardWM-owned clipping region. `redraw` is normally
/// false for animation frames and true for the exact landing pass.
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
        if state.identity != current_identity {
            lock_states().remove(&window_id);
            if has_owner_marker(hwnd) {
                remove_metadata(hwnd);
            }
        } else {
            match region_relation(hwnd, state.expected_region) {
                RegionRelation::Matches
                    if state.expected_region == expected_region && has_owner_marker(hwnd) =>
                {
                    return RegionClipResult::Unchanged;
                }
                RegionRelation::Matches => {
                    if !clear_region(hwnd, false) {
                        return RegionClipResult::Failed;
                    }
                    remove_metadata(hwnd);
                    lock_states().remove(&window_id);
                }
                RegionRelation::QueryFailed => return RegionClipResult::Failed,
                RegionRelation::NoRegion | RegionRelation::Differs => {
                    remove_metadata(hwnd);
                    lock_states().remove(&window_id);
                }
            }
        }
    }

    if !recover_stale_metadata_locked(hwnd, false) {
        return RegionClipResult::Failed;
    }
    match current_region_kind(hwnd) {
        Some(NULL_REGION_KIND) => {}
        Some(_) => return RegionClipResult::Unsupported,
        None => return RegionClipResult::Failed,
    }

    // Create the GDI region before publishing ownership metadata.
    let Some(region) = create_region(expected_region) else {
        return RegionClipResult::Failed;
    };
    if !write_metadata(hwnd, expected_region) {
        delete_region(region);
        return RegionClipResult::Failed;
    }
    if unsafe { SetWindowRgn(hwnd, Some(region), redraw) } == 0 {
        delete_region(region);
        remove_metadata(hwnd);
        return RegionClipResult::Failed;
    }
    // On success Windows owns `region`; deleting it would invalidate the shape.
    match region_relation(hwnd, expected_region) {
        RegionRelation::Matches | RegionRelation::QueryFailed => {
            // SetWindowRgn's nonzero result is authoritative. If verification is
            // temporarily unavailable, retain metadata/state for the next pass.
            lock_states().insert(
                window_id,
                RegionState {
                    identity: current_identity,
                    expected_region,
                },
            );
            RegionClipResult::Applied
        }
        RegionRelation::NoRegion | RegionRelation::Differs => {
            // The target replaced or removed the region immediately. Relinquish
            // ownership and let the caller use its same-frame safe fallback.
            remove_metadata(hwnd);
            RegionClipResult::Failed
        }
    }
}

/// Restore a window only when its current shape is still the exact region
/// LeopardWM installed. Application replacements are never cleared.
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

    match region_relation(hwnd, expected) {
        RegionRelation::Matches => {
            if !clear_region(hwnd, redraw) {
                return false;
            }
            remove_metadata(hwnd);
            lock_states().remove(&window_id);
            true
        }
        RegionRelation::NoRegion | RegionRelation::Differs => {
            remove_metadata(hwnd);
            lock_states().remove(&window_id);
            true
        }
        RegionRelation::QueryFailed => false,
    }
}

/// Restore regions for placements that no longer request clipping and for
/// tracked HWNDs that left the active layout.
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

/// Restore every region tracked by this process. Called by shutdown, panic
/// recovery, emergency uncloak, and empty-layout cleanup.
pub fn restore_all_window_regions() {
    let window_ids: Vec<WindowId> = lock_states().keys().copied().collect();
    for window_id in window_ids {
        let _ = restore_window_region(window_id, true);
    }
}

/// Forget a destroyed HWND without touching a possible new window that later
/// reuses the same numeric handle.
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

#[cfg(test)]
#[path = "window_region_win32_tests.rs"]
mod win32_tests;
