//! Conservative ownership of temporary rectangular top-level window regions.
//!
//! LeopardWM installs a region only on a live, responsive, unregioned HWND.
//! Every owned region is described by recoverable HWND properties. A later
//! LeopardWM process clears a crash-leftover region only when the actual region
//! exactly matches an active or pending rectangle written by LeopardWM. Any
//! application-owned or replaced region is preserved and the caller falls back
//! to whole-window monitor isolation.

use crate::{get_window_info, recover_poisoned_mutex, window_id_to_hwnd};
use leopardwm_core_layout::{Rect, WindowId};
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HANDLE, HWND, RECT};
use windows::Win32::Graphics::Gdi::{CreateRectRgn, DeleteObject, EqualRgn, HGDIOBJ};
use windows::Win32::UI::WindowsAndMessaging::{
    GetPropW, GetWindowRgn, GetWindowRgnBox, IsHungAppWindow, IsWindow, RemovePropW, SetPropW,
    SetWindowRgn,
};

const ERROR_REGION_KIND: i32 = 0;
const NULL_REGION_KIND: i32 = 1;
const REGION_VERIFY_INTERVAL: Duration = Duration::from_millis(250);
const UNSUPPORTED_RETRY_INTERVAL: Duration = Duration::from_secs(3);

const OWNER_PROP: PCWSTR = w!("LeopardWM.RegionClip.Owner.v3");
const ACTIVE_LEFT_PROP: PCWSTR = w!("LeopardWM.RegionClip.Active.Left.v3");
const ACTIVE_TOP_PROP: PCWSTR = w!("LeopardWM.RegionClip.Active.Top.v3");
const ACTIVE_RIGHT_PROP: PCWSTR = w!("LeopardWM.RegionClip.Active.Right.v3");
const ACTIVE_BOTTOM_PROP: PCWSTR = w!("LeopardWM.RegionClip.Active.Bottom.v3");
const PENDING_LEFT_PROP: PCWSTR = w!("LeopardWM.RegionClip.Pending.Left.v3");
const PENDING_TOP_PROP: PCWSTR = w!("LeopardWM.RegionClip.Pending.Top.v3");
const PENDING_RIGHT_PROP: PCWSTR = w!("LeopardWM.RegionClip.Pending.Right.v3");
const PENDING_BOTTOM_PROP: PCWSTR = w!("LeopardWM.RegionClip.Pending.Bottom.v3");

fn owner_marker() -> HANDLE {
    HANDLE(0x4c57_4d52usize as *mut c_void)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WindowIdentity {
    process_id: u32,
    class_name: String,
}

#[derive(Debug, Clone)]
struct ActiveRegion {
    identity: WindowIdentity,
    region: Rect,
    last_verified: Instant,
}

#[derive(Debug, Clone)]
struct UnsupportedRegion {
    identity: WindowIdentity,
    retry_after: Instant,
}

#[derive(Default)]
struct RegionBook {
    active: HashMap<WindowId, ActiveRegion>,
    unsupported: HashMap<WindowId, UnsupportedRegion>,
}

static REGION_COMMIT: Mutex<()> = Mutex::new(());
static REGION_BOOK: OnceLock<Mutex<RegionBook>> = OnceLock::new();

fn book() -> &'static Mutex<RegionBook> {
    REGION_BOOK.get_or_init(|| Mutex::new(RegionBook::default()))
}

fn lock_commit() -> std::sync::MutexGuard<'static, ()> {
    REGION_COMMIT
        .lock()
        .unwrap_or_else(recover_poisoned_mutex)
}

fn lock_book() -> std::sync::MutexGuard<'static, RegionBook> {
    book().lock().unwrap_or_else(recover_poisoned_mutex)
}

fn identity(window_id: WindowId) -> Option<WindowIdentity> {
    let info = get_window_info(window_id).ok()?;
    Some(WindowIdentity {
        process_id: info.process_id,
        class_name: info.class_name,
    })
}

fn live_responsive_hwnd(window_id: WindowId) -> Option<HWND> {
    let hwnd = window_id_to_hwnd(window_id).ok()?;
    unsafe {
        (IsWindow(Some(hwnd)).as_bool() && !IsHungAppWindow(hwnd).as_bool()).then_some(hwnd)
    }
}

/// Store every i32 in a non-null pointer-sized property value. LeopardWM ships
/// only x86-64 Windows binaries, so 2^32 payloads fit after adding one in u64.
fn encode_i32(value: i32) -> HANDLE {
    let bits = u32::from_ne_bytes(value.to_ne_bytes());
    HANDLE((u64::from(bits) + 1) as usize as *mut c_void)
}

fn decode_i32(value: HANDLE) -> Option<i32> {
    let raw = value.0 as usize as u64;
    if raw == 0 || raw > u64::from(u32::MAX) + 1 {
        return None;
    }
    let bits = (raw - 1) as u32;
    Some(i32::from_ne_bytes(bits.to_ne_bytes()))
}

fn get_prop(hwnd: HWND, name: PCWSTR) -> HANDLE {
    unsafe { GetPropW(hwnd, name) }
}

fn remove_prop(hwnd: HWND, name: PCWSTR) {
    unsafe {
        let _ = RemovePropW(hwnd, name);
    }
}

fn set_prop(hwnd: HWND, name: PCWSTR, value: HANDLE) -> bool {
    unsafe { SetPropW(hwnd, name, value).is_ok() }
}

const ACTIVE_PROPS: [PCWSTR; 4] = [
    ACTIVE_LEFT_PROP,
    ACTIVE_TOP_PROP,
    ACTIVE_RIGHT_PROP,
    ACTIVE_BOTTOM_PROP,
];
const PENDING_PROPS: [PCWSTR; 4] = [
    PENDING_LEFT_PROP,
    PENDING_TOP_PROP,
    PENDING_RIGHT_PROP,
    PENDING_BOTTOM_PROP,
];

fn clear_props(hwnd: HWND, props: [PCWSTR; 4]) {
    for prop in props {
        remove_prop(hwnd, prop);
    }
}

fn clear_all_markers(hwnd: HWND) {
    clear_props(hwnd, ACTIVE_PROPS);
    clear_props(hwnd, PENDING_PROPS);
    remove_prop(hwnd, OWNER_PROP);
}

fn write_rect_props(hwnd: HWND, props: [PCWSTR; 4], rect: Rect) -> bool {
    let values = [rect.x, rect.y, rect.right(), rect.bottom()];
    for (prop, value) in props.into_iter().zip(values) {
        if !set_prop(hwnd, prop, encode_i32(value)) {
            return false;
        }
    }
    true
}

fn read_rect_props(hwnd: HWND, props: [PCWSTR; 4]) -> Option<Rect> {
    let left = decode_i32(get_prop(hwnd, props[0]))?;
    let top = decode_i32(get_prop(hwnd, props[1]))?;
    let right = decode_i32(get_prop(hwnd, props[2]))?;
    let bottom = decode_i32(get_prop(hwnd, props[3]))?;
    (right > left && bottom > top).then(|| Rect::new(left, top, right - left, bottom - top))
}

fn marker_candidates(hwnd: HWND) -> Vec<Rect> {
    if get_prop(hwnd, OWNER_PROP) != owner_marker() {
        return Vec::new();
    }
    let mut candidates = Vec::with_capacity(2);
    if let Some(active) = read_rect_props(hwnd, ACTIVE_PROPS) {
        candidates.push(active);
    }
    if let Some(pending) = read_rect_props(hwnd, PENDING_PROPS) {
        if !candidates.contains(&pending) {
            candidates.push(pending);
        }
    }
    candidates
}

fn write_pending_marker(hwnd: HWND, pending: Rect, has_active: bool) -> bool {
    clear_props(hwnd, PENDING_PROPS);
    if !write_rect_props(hwnd, PENDING_PROPS, pending) {
        clear_props(hwnd, PENDING_PROPS);
        return false;
    }
    if get_prop(hwnd, OWNER_PROP) == owner_marker() || set_prop(hwnd, OWNER_PROP, owner_marker()) {
        true
    } else {
        clear_props(hwnd, PENDING_PROPS);
        if !has_active {
            remove_prop(hwnd, OWNER_PROP);
        }
        false
    }
}

fn rollback_pending_marker(hwnd: HWND, has_active: bool) {
    clear_props(hwnd, PENDING_PROPS);
    if !has_active {
        clear_props(hwnd, ACTIVE_PROPS);
        remove_prop(hwnd, OWNER_PROP);
    }
}

fn commit_pending_marker(hwnd: HWND, region: Rect) {
    if write_rect_props(hwnd, ACTIVE_PROPS, region) {
        clear_props(hwnd, PENDING_PROPS);
    }
    // If an individual property update failed, the pending rectangle remains.
    // Recovery accepts either candidate, so ownership is still provable.
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

fn mark_unsupported(window_id: WindowId, identity: WindowIdentity) {
    lock_book().unsupported.insert(
        window_id,
        UnsupportedRegion {
            identity,
            retry_after: Instant::now() + UNSUPPORTED_RETRY_INTERVAL,
        },
    );
}

/// Compute the horizontal part of `outer_rect` inside `clip_bounds`, expressed
/// in window-local coordinates. The full outer height is retained intentionally.
pub(crate) fn relative_clip_region(outer_rect: Rect, clip_bounds: Rect) -> Option<Rect> {
    let outer_width = outer_rect.width.max(1);
    let outer_height = outer_rect.height.max(1);
    let left = outer_rect.x.max(clip_bounds.x);
    let right = outer_rect.right().min(clip_bounds.right());
    if right <= left {
        return None;
    }

    let local_left = left.saturating_sub(outer_rect.x).clamp(0, outer_width);
    let local_right = right
        .saturating_sub(outer_rect.x)
        .clamp(local_left, outer_width);
    (local_right > local_left)
        .then(|| Rect::new(local_left, 0, local_right - local_left, outer_height))
}

/// Recover a marker left by a previous process. A region is cleared only when
/// it exactly matches one of our active/pending transaction rectangles.
fn recover_stale_marker(hwnd: HWND) -> bool {
    if get_prop(hwnd, OWNER_PROP) != owner_marker() {
        // Clean harmless coordinate properties left before the owner property
        // was committed. They do not prove ownership of the current region.
        clear_props(hwnd, ACTIVE_PROPS);
        clear_props(hwnd, PENDING_PROPS);
        return window_has_no_region(hwnd);
    }

    if window_has_no_region(hwnd) {
        clear_all_markers(hwnd);
        return true;
    }

    let owned = marker_candidates(hwnd)
        .into_iter()
        .any(|candidate| current_region_matches(hwnd, candidate));
    if !owned {
        // The application replaced our region or the marker is incomplete.
        // Preserve the region and relinquish ownership.
        clear_all_markers(hwnd);
        return false;
    }
    if !clear_region(hwnd, true) {
        return false;
    }
    clear_all_markers(hwnd);
    true
}

/// A cheap, fail-closed capability probe used before a placement batch chooses
/// between clipping and its whole-window fallback.
pub(crate) fn can_clip_window_region(window_id: WindowId) -> bool {
    let Some(hwnd) = live_responsive_hwnd(window_id) else {
        return false;
    };
    let Some(current_identity) = identity(window_id) else {
        return false;
    };
    let _commit = lock_commit();

    {
        let mut book = lock_book();
        if let Some(active) = book.active.get(&window_id) {
            if active.identity == current_identity {
                return true;
            }
            book.active.remove(&window_id);
        }
        if let Some(unsupported) = book.unsupported.get(&window_id) {
            if unsupported.identity == current_identity && Instant::now() < unsupported.retry_after {
                return false;
            }
            book.unsupported.remove(&window_id);
        }
    }

    let supported = recover_stale_marker(hwnd) && window_has_no_region(hwnd);
    if !supported {
        mark_unsupported(window_id, current_identity);
    }
    supported
}

/// Install or update a LeopardWM-owned rectangular region after the target HWND
/// has reached `outer_rect`. Returns false so the caller can apply its fallback
/// when ownership, responsiveness, or a Win32 call is unsafe.
pub(crate) fn apply_window_region_clip(
    window_id: WindowId,
    outer_rect: Rect,
    clip_bounds: Rect,
    redraw: bool,
) -> bool {
    let Some(region_rect) = relative_clip_region(outer_rect, clip_bounds) else {
        return false;
    };
    let Some(hwnd) = live_responsive_hwnd(window_id) else {
        return false;
    };
    let Some(current_identity) = identity(window_id) else {
        return false;
    };
    let _commit = lock_commit();

    let previous = {
        let mut book = lock_book();
        match book.active.get(&window_id).cloned() {
            Some(state) if state.identity == current_identity => Some(state),
            Some(_) => {
                book.active.remove(&window_id);
                None
            }
            None => None,
        }
    };

    if let Some(state) = previous.as_ref() {
        if !marker_candidates(hwnd).contains(&state.region) {
            clear_all_markers(hwnd);
            let mut book = lock_book();
            book.active.remove(&window_id);
            mark_unsupported(window_id, current_identity);
            return false;
        }
        if state.region == region_rect
            && state.last_verified.elapsed() < REGION_VERIFY_INTERVAL
        {
            return true;
        }
        if !current_region_matches(hwnd, state.region) {
            // The target replaced our region. Never clear the replacement.
            clear_all_markers(hwnd);
            let mut book = lock_book();
            book.active.remove(&window_id);
            drop(book);
            mark_unsupported(window_id, current_identity);
            return false;
        }
        if state.region == region_rect {
            if let Some(active) = lock_book().active.get_mut(&window_id) {
                active.last_verified = Instant::now();
            }
            return true;
        }
    } else if !recover_stale_marker(hwnd) || !window_has_no_region(hwnd) {
        mark_unsupported(window_id, current_identity);
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
        mark_unsupported(window_id, current_identity);
        return false;
    };

    let has_active = previous.is_some();
    if !write_pending_marker(hwnd, region_rect, has_active) {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(region.0));
        }
        mark_unsupported(window_id, current_identity);
        return false;
    }

    if unsafe { SetWindowRgn(hwnd, Some(region), redraw) } == 0 {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(region.0));
        }
        rollback_pending_marker(hwnd, has_active);
        mark_unsupported(window_id, current_identity);
        return false;
    }

    // Windows owns `region` after a successful SetWindowRgn call.
    commit_pending_marker(hwnd, region_rect);
    let mut book = lock_book();
    book.active.insert(
        window_id,
        ActiveRegion {
            identity: current_identity,
            region: region_rect,
            last_verified: Instant::now(),
        },
    );
    book.unsupported.remove(&window_id);
    true
}

/// Restore the original unregioned shape. If the target replaced our region,
/// preserve its replacement and only discard LeopardWM bookkeeping/markers.
pub(crate) fn restore_window_region(window_id: WindowId, redraw: bool) -> bool {
    let _commit = lock_commit();
    let Some(hwnd) = live_responsive_hwnd(window_id) else {
        if live_responsive_hwnd(window_id).is_none() {
            lock_book().active.remove(&window_id);
        }
        return false;
    };

    let state = lock_book().active.get(&window_id).cloned();
    let Some(state) = state else {
        return recover_stale_marker(hwnd);
    };
    if identity(window_id) != Some(state.identity.clone()) {
        lock_book().active.remove(&window_id);
        return true;
    }
    if !marker_candidates(hwnd).contains(&state.region)
        || !current_region_matches(hwnd, state.region)
    {
        clear_all_markers(hwnd);
        lock_book().active.remove(&window_id);
        return true;
    }
    if !clear_region(hwnd, redraw) {
        return false;
    }
    clear_all_markers(hwnd);
    lock_book().active.remove(&window_id);
    true
}

pub(crate) fn restore_window_regions_not_in(current_ids: &HashSet<WindowId>, redraw: bool) {
    let stale: Vec<WindowId> = lock_book()
        .active
        .keys()
        .filter(|window_id| !current_ids.contains(window_id))
        .copied()
        .collect();
    for window_id in stale {
        let _ = restore_window_region(window_id, redraw);
    }
}

/// Best-effort recovery used by shutdown, panic-revert, pause, and emergency
/// uncloak paths. A second pass covers transient first-call failures.
pub fn restore_all_window_regions() {
    for _ in 0..2 {
        let window_ids: Vec<WindowId> = lock_book().active.keys().copied().collect();
        if window_ids.is_empty() {
            break;
        }
        for window_id in window_ids {
            let _ = restore_window_region(window_id, true);
        }
    }
}

/// Drop state for a destroyed HWND without issuing an API call that could touch
/// a new window reusing the same numeric handle.
pub fn forget_window_region(window_id: WindowId) {
    let _commit = lock_commit();
    let mut book = lock_book();
    book.active.remove(&window_id);
    book.unsupported.remove(&window_id);
}

#[cfg(test)]
mod tests {
    use super::{decode_i32, encode_i32, relative_clip_region};
    use leopardwm_core_layout::Rect;

    #[test]
    fn signed_property_encoding_round_trips_every_edge_case() {
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
