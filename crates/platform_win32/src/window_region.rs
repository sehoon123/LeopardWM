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
use windows::Win32::Foundation::{HANDLE, HWND, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::{
    CreateRectRgn, DeleteObject, EqualRgn, GetWindowRgn, SetWindowRgn, HGDIOBJ,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetPropW, GetWindowRect, GetWindowThreadProcessId, IsWindow, RemovePropW,
    SetPropW,
};

const ERROR_REGION_KIND: i32 = 0;
const NULL_REGION_KIND: i32 = 1;
const SIMPLE_REGION_KIND: i32 = 2;
const COMPLEX_REGION_KIND: i32 = 3;
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
    REGION_COMMIT.lock().unwrap_or_else(recover_poisoned_mutex)
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
    usize_from_handle(unsafe { GetPropW(hwnd, w!("LeopardWM.RegionClip.v2.Owner")) }) == OWNER_MAGIC
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
    // Publish coordinates first and the owner marker last. Another process can
    // observe either a complete record or no record, never a partial rectangle.
    let payload = unsafe {
        [
            SetPropW(
                hwnd,
                w!("LeopardWM.RegionClip.v2.Left"),
                Some(encode_coordinate(rect.x)),
            ),
            SetPropW(
                hwnd,
                w!("LeopardWM.RegionClip.v2.Top"),
                Some(encode_coordinate(rect.y)),
            ),
            SetPropW(
                hwnd,
                w!("LeopardWM.RegionClip.v2.Right"),
                Some(encode_coordinate(rect.right())),
            ),
            SetPropW(
                hwnd,
                w!("LeopardWM.RegionClip.v2.Bottom"),
                Some(encode_coordinate(rect.bottom())),
            ),
        ]
    };
    if payload.into_iter().any(|result| result.is_err()) {
        remove_metadata(hwnd);
        return false;
    }
    if unsafe {
        SetPropW(
            hwnd,
            w!("LeopardWM.RegionClip.v2.Owner"),
            Some(handle_from_usize(OWNER_MAGIC)),
        )
    }
    .is_err()
    {
        remove_metadata(hwnd);
        return false;
    }
    true
}

fn read_metadata(hwnd: HWND) -> Option<Rect> {
    if !has_owner_marker(hwnd) {
        return None;
    }
    let left = decode_coordinate(unsafe { GetPropW(hwnd, w!("LeopardWM.RegionClip.v2.Left")) })?;
    let top = decode_coordinate(unsafe { GetPropW(hwnd, w!("LeopardWM.RegionClip.v2.Top")) })?;
    let right = decode_coordinate(unsafe { GetPropW(hwnd, w!("LeopardWM.RegionClip.v2.Right")) })?;
    let bottom =
        decode_coordinate(unsafe { GetPropW(hwnd, w!("LeopardWM.RegionClip.v2.Bottom")) })?;
    if right < left || bottom < top {
        return None;
    }
    Some(Rect::new(left, top, right - left, bottom - top))
}

fn create_region(rect: Rect) -> Option<windows::Win32::Graphics::Gdi::HRGN> {
    let region = unsafe { CreateRectRgn(rect.x, rect.y, rect.right(), rect.bottom()) };
    if region.0.is_null() {
        None
    } else {
        Some(region)
    }
}

fn delete_region(region: windows::Win32::Graphics::Gdi::HRGN) {
    unsafe {
        let _ = DeleteObject(HGDIOBJ(region.0));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowRegionKind {
    /// ERROR means no region for this normal case. NULLREGION means an
    /// explicitly empty region, not the absence of a region.
    NoRegion,
    Empty,
    Simple,
    Complex,
}

fn classify_window_region_kind(raw: i32) -> Option<WindowRegionKind> {
    match raw {
        ERROR_REGION_KIND => Some(WindowRegionKind::NoRegion),
        NULL_REGION_KIND => Some(WindowRegionKind::Empty),
        SIMPLE_REGION_KIND => Some(WindowRegionKind::Simple),
        COMPLEX_REGION_KIND => Some(WindowRegionKind::Complex),
        _ => None,
    }
}

fn current_region_kind(hwnd: HWND) -> Option<WindowRegionKind> {
    let region = create_region(Rect::new(0, 0, 1, 1))?;
    let raw = unsafe { GetWindowRgn(hwnd, region) }.0;
    delete_region(region);
    classify_window_region_kind(raw)
}

fn actual_region_matches(hwnd: HWND, expected: Rect) -> bool {
    let Some(actual) = create_region(Rect::new(0, 0, 1, 1)) else {
        return false;
    };
    let raw = unsafe { GetWindowRgn(hwnd, actual) }.0;
    let Some(kind) = classify_window_region_kind(raw) else {
        delete_region(actual);
        return false;
    };
    if kind == WindowRegionKind::NoRegion {
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

#[cfg(test)]
fn window_has_no_region(hwnd: HWND) -> bool {
    matches!(current_region_kind(hwnd), Some(WindowRegionKind::NoRegion))
}

fn rect_from_win32(rect: RECT) -> Option<Rect> {
    let width = rect.right.saturating_sub(rect.left);
    let height = rect.bottom.saturating_sub(rect.top);
    (width > 0 && height > 0).then(|| Rect::new(rect.left, rect.top, width, height))
}

fn current_window_geometry(hwnd: HWND) -> Option<(Rect, Rect)> {
    let mut outer = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut outer) }.ok()?;
    let outer = rect_from_win32(outer)?;

    let mut visible = RECT::default();
    let visible = if unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut visible as *mut _ as _,
            std::mem::size_of::<RECT>() as u32,
        )
    }
    .is_ok()
    {
        rect_from_win32(visible).unwrap_or(outer)
    } else {
        outer
    };
    Some((outer, visible))
}

fn intersect_regions(left: Rect, right: Rect) -> Rect {
    let x = left.x.max(right.x);
    let y = left.y.max(right.y);
    let right_edge = left.right().min(right.right());
    let bottom_edge = left.bottom().min(right.bottom());
    Rect::new(
        x,
        y,
        right_edge.saturating_sub(x).max(0),
        bottom_edge.saturating_sub(y).max(0),
    )
}

fn allowed_region(outer_rect: Rect, visible_rect: Rect, clip_bounds: Rect) -> Rect {
    relative_clip_region(outer_rect, visible_rect, clip_bounds)
        .unwrap_or_else(|| Rect::new(0, 0, 0, 0))
}

/// Local shape that is safe at both the old and target HWND positions. Since a
/// monitor rectangle is convex, the same bridge is also safe during any DWM
/// interpolation between those endpoints.
pub(crate) fn bridge_clip_region(
    current_outer: Rect,
    current_visible: Rect,
    target_outer: Rect,
    target_visible: Rect,
    clip_bounds: Rect,
) -> Rect {
    intersect_regions(
        allowed_region(current_outer, current_visible, clip_bounds),
        allowed_region(target_outer, target_visible, clip_bounds),
    )
}

fn install_owned_region_locked(
    window_id: WindowId,
    hwnd: HWND,
    identity: WindowIdentity,
    expected_region: Rect,
    redraw: bool,
) -> RegionClipResult {
    let Some(region) = create_region(expected_region) else {
        return RegionClipResult::Failed;
    };
    if unsafe { SetWindowRgn(hwnd, Some(region), redraw) } == 0 {
        delete_region(region);
        return RegionClipResult::Failed;
    }
    // Windows owns HRGN after a successful SetWindowRgn call.
    if !write_metadata(hwnd, expected_region) {
        // In-process state remains authoritative, allowing normal cleanup even
        // when HWND property storage was temporarily unavailable.
        remove_metadata(hwnd);
    }
    lock_states().insert(
        window_id,
        RegionState {
            identity,
            expected_region,
        },
    );
    RegionClipResult::Applied
}

fn owned_region_for_identity(
    window_id: WindowId,
    hwnd: HWND,
    identity: &WindowIdentity,
) -> Result<Option<Rect>, RegionClipResult> {
    if let Some(state) = lock_states().get(&window_id).cloned() {
        if state.identity == *identity && actual_region_matches(hwnd, state.expected_region) {
            return Ok(Some(state.expected_region));
        }
        lock_states().remove(&window_id);
        if state.identity == *identity && has_owner_marker(hwnd) {
            // Application takeover: discard only our marker, never its shape.
            remove_metadata(hwnd);
            return Err(RegionClipResult::Unsupported);
        }
    }

    if has_owner_marker(hwnd) {
        if let Some(expected) = read_metadata(hwnd) {
            if actual_region_matches(hwnd, expected) {
                return Ok(Some(expected));
            }
        }
        remove_metadata(hwnd);
        return Err(RegionClipResult::Unsupported);
    }

    match current_region_kind(hwnd) {
        Some(WindowRegionKind::NoRegion) => Ok(None),
        Some(_) => Err(RegionClipResult::Unsupported),
        None => Err(RegionClipResult::Failed),
    }
}

/// Install a restrictive bridge before the HWND is uncloaked or moved.
pub(crate) fn prepare_window_region_clip(
    window_id: WindowId,
    target_outer: Rect,
    target_visible: Rect,
    clip_bounds: Rect,
) -> RegionClipResult {
    let target_region = allowed_region(target_outer, target_visible, clip_bounds);
    let _commit = lock_commit();
    let Some(current_identity) = identity(window_id) else {
        lock_states().remove(&window_id);
        return RegionClipResult::Failed;
    };
    let Ok(hwnd) = window_id_to_hwnd(window_id) else {
        return RegionClipResult::Failed;
    };

    let current_owned = match owned_region_for_identity(window_id, hwnd, &current_identity) {
        Ok(region) => region,
        Err(result) => return result,
    };
    let bridge = if let Some(region) = current_owned {
        intersect_regions(region, target_region)
    } else {
        let Some((current_outer, current_visible)) = current_window_geometry(hwnd) else {
            return RegionClipResult::Failed;
        };
        bridge_clip_region(
            current_outer,
            current_visible,
            target_outer,
            target_visible,
            clip_bounds,
        )
    };
    if current_owned == Some(bridge) && actual_region_matches(hwnd, bridge) {
        return RegionClipResult::Unchanged;
    }
    install_owned_region_locked(window_id, hwnd, current_identity, bridge, false)
}

pub(crate) fn has_owned_window_region(window_id: WindowId) -> bool {
    if lock_states().contains_key(&window_id) {
        return true;
    }
    window_id_to_hwnd(window_id)
        .ok()
        .is_some_and(has_owner_marker)
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
    if right < left || bottom < top {
        return None;
    }
    Some(Rect::new(left, top, right - left, bottom - top))
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
    let target_region = allowed_region(outer_rect, visible_rect, clip_bounds);
    let _commit = lock_commit();
    let Some(current_identity) = identity(window_id) else {
        lock_states().remove(&window_id);
        return RegionClipResult::Failed;
    };
    let Ok(hwnd) = window_id_to_hwnd(window_id) else {
        return RegionClipResult::Failed;
    };

    let current_owned = match owned_region_for_identity(window_id, hwnd, &current_identity) {
        Ok(region) => region,
        Err(result) => return result,
    };
    if current_owned == Some(target_region) && actual_region_matches(hwnd, target_region) {
        return RegionClipResult::Unchanged;
    }

    // Replace the bridge directly. Clearing first creates an unbounded
    // rectangular DWM frame between the two SetWindowRgn calls.
    install_owned_region_locked(window_id, hwnd, current_identity, target_region, redraw)
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

    if actual_region_matches(hwnd, expected) && !clear_region(hwnd, redraw) {
        return false;
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
    use super::{
        actual_region_matches, apply_window_region_clip, bridge_clip_region,
        classify_window_region_kind, decode_coordinate, encode_coordinate,
        prepare_window_region_clip, relative_clip_region, restore_window_region,
        window_has_no_region, WindowRegionKind, ERROR_REGION_KIND,
    };
    use leopardwm_core_layout::Rect;
    use std::sync::OnceLock;
    use windows::core::w;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassExW, SetWindowPos,
        SWP_NOACTIVATE, SWP_NOZORDER, WINDOW_EX_STYLE, WNDCLASSEXW, WS_OVERLAPPED,
    };

    unsafe extern "system" fn test_wndproc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    }

    fn test_window() -> HWND {
        static REGISTERED: OnceLock<()> = OnceLock::new();
        let instance = unsafe { GetModuleHandleW(None).unwrap() };
        REGISTERED.get_or_init(|| {
            let class = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(test_wndproc),
                hInstance: instance.into(),
                lpszClassName: w!("LeopardWMPreviewRegionTest"),
                ..Default::default()
            };
            assert_ne!(unsafe { RegisterClassExW(&class) }, 0);
        });
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("LeopardWMPreviewRegionTest"),
                w!(""),
                WS_OVERLAPPED,
                0,
                0,
                1000,
                800,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .unwrap()
        }
    }

    fn window_id(hwnd: HWND) -> u64 {
        hwnd.0 as usize as u64
    }

    struct TestWindow(HWND);

    impl TestWindow {
        fn new() -> Self {
            Self(test_window())
        }
    }

    impl Drop for TestWindow {
        fn drop(&mut self) {
            let _ = restore_window_region(window_id(self.0), false);
            unsafe {
                let _ = DestroyWindow(self.0);
            }
        }
    }

    #[test]
    fn get_window_rgn_error_is_the_normal_unowned_state() {
        assert_eq!(
            classify_window_region_kind(ERROR_REGION_KIND),
            Some(WindowRegionKind::NoRegion)
        );

        let window = TestWindow::new();
        let id = window_id(window.0);
        assert!(window_has_no_region(window.0));

        let result = apply_window_region_clip(
            id,
            Rect::new(0, 0, 1000, 800),
            Rect::new(0, 0, 1000, 800),
            Rect::new(0, 0, 250, 800),
            false,
        );
        assert!(result.succeeded());
        assert!(restore_window_region(id, false));
        assert!(window_has_no_region(window.0));
    }

    #[test]
    fn centered_preview_regions_are_symmetric() {
        let viewport = Rect::new(0, 0, 1000, 800);
        for (column_width, preview_width) in [(500, 250), (750, 125)] {
            let left = Rect::new(preview_width - column_width, 0, column_width, 800);
            let right = Rect::new(1000 - preview_width, 0, column_width, 800);

            assert_eq!(
                relative_clip_region(left, left, viewport),
                Some(Rect::new(
                    column_width - preview_width,
                    0,
                    preview_width,
                    800,
                ))
            );
            assert_eq!(
                relative_clip_region(right, right, viewport),
                Some(Rect::new(0, 0, preview_width, 800))
            );
        }
    }

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
        assert_eq!(region, Rect::new(0, 110, 820, 1090));
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

    fn screen_region(outer: Rect, local: Rect) -> Rect {
        Rect::new(
            outer.x.saturating_add(local.x),
            outer.y.saturating_add(local.y),
            local.width,
            local.height,
        )
    }

    fn position_test_window(hwnd: HWND, rect: Rect) {
        unsafe {
            SetWindowPos(
                hwnd,
                None,
                rect.x,
                rect.y,
                rect.width,
                rect.height,
                SWP_NOZORDER | SWP_NOACTIVATE,
            )
            .unwrap();
        }
    }

    #[test]
    fn bridge_is_safe_at_endpoints_and_intermediate_positions() {
        let owner = Rect::new(1000, 0, 1000, 800);
        for width in [250, 500, 750, 1000, 1250] {
            for old_x in (250..=2250).step_by(125) {
                for new_x in (250..=2250).step_by(125) {
                    let old = Rect::new(old_x, 0, width, 800);
                    let new = Rect::new(new_x, 0, width, 800);
                    let bridge = bridge_clip_region(old, old, new, new, owner);
                    for step in 0..=8 {
                        let x = old_x + (new_x - old_x) * step / 8;
                        let translated = screen_region(Rect::new(x, 0, width, 800), bridge);
                        if bridge.width > 0 && bridge.height > 0 {
                            assert!(translated.x >= owner.x);
                            assert!(translated.right() <= owner.right());
                            assert!(translated.y >= owner.y);
                            assert!(translated.bottom() <= owner.bottom());
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn seventy_five_percent_preview_masks_the_negative_monitor_half() {
        let owner = Rect::new(1000, 0, 1000, 800);
        let old = Rect::new(1000, 0, 750, 800);
        let target = Rect::new(500, 0, 750, 800);
        let bridge = bridge_clip_region(old, old, target, target, owner);
        assert_eq!(bridge, Rect::new(500, 0, 250, 800));
        let target_screen = screen_region(target, bridge);
        assert_eq!(target_screen, Rect::new(1000, 0, 250, 800));
        assert!(!target_screen.intersects(&Rect::new(0, 0, 1000, 800)));
    }

    #[test]
    fn opposite_edge_jump_uses_an_empty_safe_bridge() {
        let owner = Rect::new(1000, 0, 1000, 800);
        let left = Rect::new(500, 0, 750, 800);
        let right = Rect::new(1750, 0, 750, 800);
        assert_eq!(bridge_clip_region(left, left, right, right, owner).width, 0);
    }

    #[test]
    fn outward_move_restricts_before_positioning() {
        let window = TestWindow::new();
        let id = window_id(window.0);
        let owner = Rect::new(0, 0, 1000, 800);
        let current = Rect::new(-250, 0, 750, 800);
        let target = Rect::new(-500, 0, 750, 800);
        position_test_window(window.0, current);
        assert!(apply_window_region_clip(id, current, current, owner, false).succeeded());
        assert!(actual_region_matches(window.0, Rect::new(250, 0, 500, 800)));

        assert!(prepare_window_region_clip(id, target, target, owner).succeeded());
        assert!(actual_region_matches(window.0, Rect::new(500, 0, 250, 800)));
    }

    #[test]
    fn inward_move_expands_only_after_positioning() {
        let window = TestWindow::new();
        let id = window_id(window.0);
        let owner = Rect::new(0, 0, 1000, 800);
        let current = Rect::new(-500, 0, 750, 800);
        let target = Rect::new(-250, 0, 750, 800);
        position_test_window(window.0, current);
        assert!(apply_window_region_clip(id, current, current, owner, false).succeeded());
        assert!(actual_region_matches(window.0, Rect::new(500, 0, 250, 800)));

        assert!(prepare_window_region_clip(id, target, target, owner).succeeded());
        assert!(actual_region_matches(window.0, Rect::new(500, 0, 250, 800)));

        position_test_window(window.0, target);
        assert!(apply_window_region_clip(id, target, target, owner, false).succeeded());
        assert!(actual_region_matches(window.0, Rect::new(250, 0, 500, 800)));
    }
}
