//! Crash-safe `SetWindowRgn` clipping for tiled windows at monitor boundaries.
//!
//! LeopardWM never overwrites an application-defined window region. Every
//! region it owns is described by versioned HWND properties. The properties
//! include the owner process creation time, so a second live LeopardWM process
//! cannot clear the first process's regions. Region replacement uses active and
//! pending slots, making either side of a crash recoverable.

use crate::{recover_poisoned_mutex, window_id_to_hwnd};
use leopardwm_core_layout::{Rect, Visibility, WindowId};
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, BOOL, FILETIME, HANDLE, HWND, LPARAM};
use windows::Win32::Graphics::Gdi::{CreateRectRgn, DeleteObject, EqualRgn, HGDIOBJ, HRGN};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetProcessTimes, OpenProcess,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetPropW, GetWindowRgn, GetWindowThreadProcessId, IsWindow,
    RemovePropW, SetPropW, SetWindowRgn,
};

const ERROR_REGION_KIND: i32 = 0;
const NULL_REGION_KIND: i32 = 1;
const OWNER_MAGIC: usize = 0x4c57_4d34; // "LWM4"
const SLOT_MAGIC: usize = 0x5247_4e34; // "RGN4"
const GDI_COORD_MAX: i32 = 67_108_863;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowRegionClip {
    pub window_id: WindowId,
    /// Screen-coordinate output rectangle that this HWND may paint into.
    pub clip_bounds: Rect,
    /// Safe placement used if clipping is unsupported or fails.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OwnerToken {
    process_id: u32,
    creation_time: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OwnerStatus {
    None,
    Current,
    OtherAlive,
    Stale,
}

#[derive(Debug, Clone, Copy)]
enum MetadataSlot {
    Active,
    Pending,
}

static REGION_STATES: OnceLock<Mutex<HashMap<WindowId, RegionState>>> = OnceLock::new();
static REGION_COMMIT: Mutex<()> = Mutex::new(());
static CURRENT_OWNER: OnceLock<OwnerToken> = OnceLock::new();

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

fn filetime_value(value: FILETIME) -> u64 {
    (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime)
}

fn process_creation_time(handle: HANDLE) -> Option<u64> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) }
        .ok()
        .map(|_| filetime_value(creation))
}

fn current_owner() -> OwnerToken {
    *CURRENT_OWNER.get_or_init(|| OwnerToken {
        process_id: unsafe { GetCurrentProcessId() },
        creation_time: process_creation_time(unsafe { GetCurrentProcess() }).unwrap_or_default(),
    })
}

fn owner_process_is_alive(owner: OwnerToken) -> bool {
    let current = current_owner();
    if owner == current {
        return true;
    }
    let Ok(process) = (unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION,
            false,
            owner.process_id,
        )
    }) else {
        // Access denial is not proof that the owner is dead. Fail closed.
        return true;
    };
    let creation_matches = process_creation_time(process) == Some(owner.creation_time);
    unsafe {
        let _ = CloseHandle(process);
    }
    creation_matches
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

fn handle_from_usize(value: usize) -> HANDLE {
    HANDLE(value as *mut c_void)
}

fn usize_from_handle(value: HANDLE) -> usize {
    value.0 as usize
}

fn get_prop(hwnd: HWND, name: PCWSTR) -> HANDLE {
    unsafe { GetPropW(hwnd, name) }
}

fn set_prop(hwnd: HWND, name: PCWSTR, value: HANDLE) -> bool {
    unsafe { SetPropW(hwnd, name, value).is_ok() }
}

fn remove_prop(hwnd: HWND, name: PCWSTR) {
    unsafe {
        let _ = RemovePropW(hwnd, name);
    }
}

fn encode_u32(value: u32) -> HANDLE {
    handle_from_usize(value as usize + 1)
}

fn decode_u32(value: HANDLE) -> Option<u32> {
    usize_from_handle(value)
        .checked_sub(1)
        .and_then(|value| u32::try_from(value).ok())
}

fn encode_coordinate(value: i32) -> HANDLE {
    // Bias into 1..=2^32; zero remains the "property missing" sentinel.
    let biased = (i64::from(value) - i64::from(i32::MIN) + 1) as u64;
    handle_from_usize(biased as usize)
}

fn decode_coordinate(value: HANDLE) -> Option<i32> {
    let raw = usize_from_handle(value) as u64;
    if raw == 0 || raw > u64::from(u32::MAX) + 1 {
        return None;
    }
    i32::try_from(raw as i64 - 1 + i64::from(i32::MIN)).ok()
}

fn remove_owner(hwnd: HWND) {
    remove_prop(hwnd, w!("LeopardWM.RegionClip.v4.Owner.Valid"));
    remove_prop(hwnd, w!("LeopardWM.RegionClip.v4.Owner.Pid"));
    remove_prop(hwnd, w!("LeopardWM.RegionClip.v4.Owner.TimeLow"));
    remove_prop(hwnd, w!("LeopardWM.RegionClip.v4.Owner.TimeHigh"));
}

fn write_owner(hwnd: HWND, owner: OwnerToken) -> bool {
    remove_owner(hwnd);
    let low = owner.creation_time as u32;
    let high = (owner.creation_time >> 32) as u32;
    let written = set_prop(
        hwnd,
        w!("LeopardWM.RegionClip.v4.Owner.Pid"),
        encode_u32(owner.process_id),
    ) && set_prop(
        hwnd,
        w!("LeopardWM.RegionClip.v4.Owner.TimeLow"),
        encode_u32(low),
    ) && set_prop(
        hwnd,
        w!("LeopardWM.RegionClip.v4.Owner.TimeHigh"),
        encode_u32(high),
    );
    if !written
        || !set_prop(
            hwnd,
            w!("LeopardWM.RegionClip.v4.Owner.Valid"),
            handle_from_usize(OWNER_MAGIC),
        )
    {
        remove_owner(hwnd);
        return false;
    }
    true
}

fn read_owner(hwnd: HWND) -> Option<OwnerToken> {
    if usize_from_handle(get_prop(hwnd, w!("LeopardWM.RegionClip.v4.Owner.Valid")))
        != OWNER_MAGIC
    {
        return None;
    }
    let process_id = decode_u32(get_prop(hwnd, w!("LeopardWM.RegionClip.v4.Owner.Pid")))?;
    let low = decode_u32(get_prop(
        hwnd,
        w!("LeopardWM.RegionClip.v4.Owner.TimeLow"),
    ))?;
    let high = decode_u32(get_prop(
        hwnd,
        w!("LeopardWM.RegionClip.v4.Owner.TimeHigh"),
    ))?;
    Some(OwnerToken {
        process_id,
        creation_time: (u64::from(high) << 32) | u64::from(low),
    })
}

fn owner_status(hwnd: HWND) -> OwnerStatus {
    let Some(owner) = read_owner(hwnd) else {
        return OwnerStatus::None;
    };
    if owner == current_owner() {
        OwnerStatus::Current
    } else if owner_process_is_alive(owner) {
        OwnerStatus::OtherAlive
    } else {
        OwnerStatus::Stale
    }
}

fn slot_valid_name(slot: MetadataSlot) -> PCWSTR {
    match slot {
        MetadataSlot::Active => w!("LeopardWM.RegionClip.v4.Active.Valid"),
        MetadataSlot::Pending => w!("LeopardWM.RegionClip.v4.Pending.Valid"),
    }
}

fn slot_names(slot: MetadataSlot) -> [PCWSTR; 4] {
    match slot {
        MetadataSlot::Active => [
            w!("LeopardWM.RegionClip.v4.Active.Left"),
            w!("LeopardWM.RegionClip.v4.Active.Top"),
            w!("LeopardWM.RegionClip.v4.Active.Right"),
            w!("LeopardWM.RegionClip.v4.Active.Bottom"),
        ],
        MetadataSlot::Pending => [
            w!("LeopardWM.RegionClip.v4.Pending.Left"),
            w!("LeopardWM.RegionClip.v4.Pending.Top"),
            w!("LeopardWM.RegionClip.v4.Pending.Right"),
            w!("LeopardWM.RegionClip.v4.Pending.Bottom"),
        ],
    }
}

fn remove_slot(hwnd: HWND, slot: MetadataSlot) {
    remove_prop(hwnd, slot_valid_name(slot));
    for name in slot_names(slot) {
        remove_prop(hwnd, name);
    }
}

fn write_slot(hwnd: HWND, slot: MetadataSlot, rect: Rect) -> bool {
    remove_slot(hwnd, slot);
    let [left, top, right, bottom] = slot_names(slot);
    let written = set_prop(hwnd, left, encode_coordinate(rect.x))
        && set_prop(hwnd, top, encode_coordinate(rect.y))
        && set_prop(hwnd, right, encode_coordinate(rect.right()))
        && set_prop(hwnd, bottom, encode_coordinate(rect.bottom()));
    if !written
        || !set_prop(
            hwnd,
            slot_valid_name(slot),
            handle_from_usize(SLOT_MAGIC),
        )
    {
        remove_slot(hwnd, slot);
        return false;
    }
    true
}

fn read_slot(hwnd: HWND, slot: MetadataSlot) -> Option<Rect> {
    if usize_from_handle(get_prop(hwnd, slot_valid_name(slot))) != SLOT_MAGIC {
        return None;
    }
    let [left, top, right, bottom] = slot_names(slot);
    let left = decode_coordinate(get_prop(hwnd, left))?;
    let top = decode_coordinate(get_prop(hwnd, top))?;
    let right = decode_coordinate(get_prop(hwnd, right))?;
    let bottom = decode_coordinate(get_prop(hwnd, bottom))?;
    if right <= left || bottom <= top {
        return None;
    }
    Some(Rect::new(left, top, right - left, bottom - top))
}

fn metadata_candidates(hwnd: HWND) -> Vec<Rect> {
    let mut candidates = Vec::with_capacity(2);
    if let Some(rect) = read_slot(hwnd, MetadataSlot::Active) {
        candidates.push(rect);
    }
    if let Some(rect) = read_slot(hwnd, MetadataSlot::Pending) {
        if !candidates.contains(&rect) {
            candidates.push(rect);
        }
    }
    candidates
}

fn remove_all_metadata(hwnd: HWND) {
    remove_slot(hwnd, MetadataSlot::Active);
    remove_slot(hwnd, MetadataSlot::Pending);
    remove_owner(hwnd);
}

fn create_region(rect: Rect) -> Option<HRGN> {
    if rect.width <= 0
        || rect.height <= 0
        || rect.x.abs() > GDI_COORD_MAX
        || rect.y.abs() > GDI_COORD_MAX
        || rect.right().abs() > GDI_COORD_MAX
        || rect.bottom().abs() > GDI_COORD_MAX
    {
        return None;
    }
    unsafe { CreateRectRgn(rect.x, rect.y, rect.right(), rect.bottom()) }.ok()
}

fn delete_region(region: HRGN) {
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

/// Recover an abandoned owner. A live owner process is never disturbed. A
/// stale/current region is cleared only if its live shape exactly matches one
/// of the committed or pending rectangles; application takeover is untouched.
fn recover_metadata(hwnd: HWND, redraw: bool) -> bool {
    match owner_status(hwnd) {
        OwnerStatus::None => return true,
        OwnerStatus::OtherAlive => return false,
        OwnerStatus::Current | OwnerStatus::Stale => {}
    }

    let candidates = metadata_candidates(hwnd);
    let kind = current_region_kind(hwnd);
    if kind == ERROR_REGION_KIND {
        return false;
    }
    if kind == NULL_REGION_KIND {
        remove_all_metadata(hwnd);
        return true;
    }
    if candidates
        .iter()
        .copied()
        .any(|candidate| actual_region_matches(hwnd, candidate))
        && !clear_region(hwnd, redraw)
    {
        return false;
    }
    remove_all_metadata(hwnd);
    true
}

fn window_has_no_region(hwnd: HWND) -> bool {
    current_region_kind(hwnd) == NULL_REGION_KIND
}

/// Compute an HWND-local region that exposes only the part of the visible DWM
/// frame inside `clip_bounds`. Outer chrome remains on non-crossing edges.
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
            && owner_status(hwnd) == OwnerStatus::Current
            && metadata_candidates(hwnd).contains(&state.expected_region)
            && actual_region_matches(hwnd, state.expected_region)
        {
            return true;
        }
        lock_states().remove(&window_id);
    }

    if !recover_metadata(hwnd, false) {
        return false;
    }
    window_has_no_region(hwnd)
}

/// Install or update a temporary clipping region. Active + pending slots make
/// both the old and new shape recoverable across every update crash point.
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
    let Some(new_region) = create_region(expected_region) else {
        return RegionClipResult::Unsupported;
    };

    let _commit = lock_commit();
    let Some(current_identity) = identity(window_id) else {
        delete_region(new_region);
        lock_states().remove(&window_id);
        return RegionClipResult::Failed;
    };
    let Ok(hwnd) = window_id_to_hwnd(window_id) else {
        delete_region(new_region);
        return RegionClipResult::Failed;
    };

    let old_state = lock_states().get(&window_id).cloned();
    let old_owned = old_state.as_ref().is_some_and(|state| {
        state.identity == current_identity
            && owner_status(hwnd) == OwnerStatus::Current
            && metadata_candidates(hwnd).contains(&state.expected_region)
            && actual_region_matches(hwnd, state.expected_region)
    });
    if old_owned
        && old_state
            .as_ref()
            .is_some_and(|state| state.expected_region == expected_region)
    {
        delete_region(new_region);
        return RegionClipResult::Unchanged;
    }
    if !old_owned {
        lock_states().remove(&window_id);
        if !recover_metadata(hwnd, false) {
            delete_region(new_region);
            return RegionClipResult::Failed;
        }
        if !window_has_no_region(hwnd) {
            delete_region(new_region);
            return RegionClipResult::Unsupported;
        }
    }

    if !write_owner(hwnd, current_owner())
        || !write_slot(hwnd, MetadataSlot::Pending, expected_region)
    {
        delete_region(new_region);
        return RegionClipResult::Failed;
    }
    if unsafe { SetWindowRgn(hwnd, Some(new_region), redraw) } == 0 {
        delete_region(new_region);
        remove_slot(hwnd, MetadataSlot::Pending);
        if read_slot(hwnd, MetadataSlot::Active).is_none() {
            remove_all_metadata(hwnd);
        }
        return RegionClipResult::Failed;
    }
    // On success Windows owns `new_region`.
    if !actual_region_matches(hwnd, expected_region) {
        // The application replaced the region concurrently. Relinquish only
        // LeopardWM metadata; never clear the application's replacement.
        remove_all_metadata(hwnd);
        lock_states().remove(&window_id);
        return RegionClipResult::Unsupported;
    }

    if write_slot(hwnd, MetadataSlot::Active, expected_region) {
        remove_slot(hwnd, MetadataSlot::Pending);
    }
    // If promotion fails, the valid pending slot still describes the live
    // region and remains sufficient for crash recovery.
    lock_states().insert(
        window_id,
        RegionState {
            identity: current_identity,
            expected_region,
        },
    );
    RegionClipResult::Applied
}

/// Restore only a region still owned by this process or a dead predecessor.
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
    if let Some(state) = state.as_ref() {
        if identity(window_id).as_ref() != Some(&state.identity) {
            lock_states().remove(&window_id);
            return true;
        }
    }
    if !recover_metadata(hwnd, redraw) {
        return false;
    }
    lock_states().remove(&window_id);
    true
}

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

unsafe extern "system" fn collect_marked_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    if read_owner(hwnd).is_some() {
        let windows = &mut *(lparam.0 as *mut Vec<WindowId>);
        windows.push(hwnd.0 as usize as u64);
    }
    BOOL(1)
}

/// Restore all regions owned by this process or a dead predecessor. Regions
/// belonging to another live LeopardWM process are deliberately skipped.
pub fn restore_all_window_regions() {
    let mut window_ids: HashSet<WindowId> = lock_states().keys().copied().collect();
    let mut marked = Vec::new();
    unsafe {
        let _ = EnumWindows(
            Some(collect_marked_window),
            LPARAM((&mut marked as *mut Vec<WindowId>) as isize),
        );
    }
    window_ids.extend(marked);
    for window_id in window_ids {
        let _ = restore_window_region(window_id, true);
    }
}

/// Forget a destroyed HWND without issuing calls against a recycled handle.
pub fn forget_window_region(window_id: WindowId) {
    lock_states().remove(&window_id);
}

#[cfg(test)]
mod tests {
    use super::{decode_coordinate, encode_coordinate, relative_clip_region};
    use leopardwm_core_layout::Rect;

    #[test]
    fn coordinate_properties_round_trip_extremes() {
        for value in [i32::MIN, -100_000, -1, 0, 1, 100_000, i32::MAX] {
            assert_eq!(decode_coordinate(encode_coordinate(value)), Some(value));
        }
    }

    #[test]
    fn clips_only_the_crossing_right_edge() {
        let region = relative_clip_region(
            Rect::new(1792, 90, 616, 916),
            Rect::new(1800, 100, 600, 900),
            Rect::new(0, 0, 1920, 1080),
        )
        .unwrap();
        assert_eq!(region, Rect::new(0, 0, 128, 916));
    }

    #[test]
    fn clips_only_the_crossing_left_edge() {
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
    fn keeps_outer_chrome_when_visible_frame_is_inside_bounds() {
        let region = relative_clip_region(
            Rect::new(92, 90, 816, 916),
            Rect::new(100, 100, 800, 900),
            Rect::new(0, 0, 1920, 1080),
        )
        .unwrap();
        assert_eq!(region, Rect::new(0, 0, 816, 916));
    }

    #[test]
    fn rejects_a_window_without_visible_intersection() {
        assert!(relative_clip_region(
            Rect::new(2100, 0, 400, 800),
            Rect::new(2100, 0, 400, 800),
            Rect::new(0, 0, 1920, 1080),
        )
        .is_none());
    }
}
