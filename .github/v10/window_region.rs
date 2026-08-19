//! Managed `SetWindowRgn` clipping for tiled windows at monitor boundaries.
//!
//! LeopardWM only takes ownership when a window has no application-defined
//! region. Every update verifies that the current region is still the one we
//! installed; if an application replaces it, LeopardWM relinquishes ownership
//! and the caller falls back to whole-window hiding.

use crate::{recover_poisoned_mutex, window_id_to_hwnd};
use leopardwm_core_layout::{Rect, WindowId};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{
    CreateRectRgn, DeleteObject, EqualRgn, HGDIOBJ, HRGN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowRgn, GetWindowThreadProcessId, IsWindow, SetWindowRgn,
};

const REGION_ERROR: i32 = 0;
const NULL_REGION: i32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LocalClipRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl LocalClipRect {
    fn is_full_window(self, outer: Rect) -> bool {
        self.left == 0
            && self.top == 0
            && self.right == outer.width
            && self.bottom == outer.height
    }
}

/// Convert a screen-space clip boundary into a non-empty window-local region.
pub(crate) fn local_clip_rect(outer: Rect, bounds: Rect) -> Option<LocalClipRect> {
    let outer_left = i64::from(outer.x);
    let outer_top = i64::from(outer.y);
    let outer_right = outer_left + i64::from(outer.width);
    let outer_bottom = outer_top + i64::from(outer.height);
    let bounds_left = i64::from(bounds.x);
    let bounds_top = i64::from(bounds.y);
    let bounds_right = bounds_left + i64::from(bounds.width);
    let bounds_bottom = bounds_top + i64::from(bounds.height);

    let left = outer_left.max(bounds_left);
    let top = outer_top.max(bounds_top);
    let right = outer_right.min(bounds_right);
    let bottom = outer_bottom.min(bounds_bottom);
    if left >= right || top >= bottom {
        return None;
    }

    Some(LocalClipRect {
        left: i32::try_from(left - outer_left).ok()?,
        top: i32::try_from(top - outer_top).ok()?,
        right: i32::try_from(right - outer_left).ok()?,
        bottom: i32::try_from(bottom - outer_top).ok()?,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegionApplyOutcome {
    Applied,
    Unsupported,
    Retry,
}

#[derive(Debug, Clone, Copy)]
enum RegionState {
    Owned {
        expected: LocalClipRect,
        spec_bounds: Rect,
        process_id: u32,
    },
    Unsupported {
        spec_bounds: Rect,
        process_id: u32,
    },
}

static MANAGED_REGIONS: Mutex<Option<HashMap<WindowId, RegionState>>> = Mutex::new(None);

fn lock_regions() -> std::sync::MutexGuard<'static, Option<HashMap<WindowId, RegionState>>> {
    MANAGED_REGIONS
        .lock()
        .unwrap_or_else(recover_poisoned_mutex)
}

struct OwnedRegion(Option<HRGN>);

impl OwnedRegion {
    fn empty() -> Option<Self> {
        unsafe { CreateRectRgn(0, 0, 0, 0).ok().map(|region| Self(Some(region))) }
    }

    fn rectangle(rect: LocalClipRect) -> Option<Self> {
        unsafe {
            CreateRectRgn(rect.left, rect.top, rect.right, rect.bottom)
                .ok()
                .map(|region| Self(Some(region)))
        }
    }

    fn handle(&self) -> HRGN {
        self.0.expect("owned region handle")
    }

    fn transfer(mut self) -> HRGN {
        self.0.take().expect("owned region handle")
    }
}

impl Drop for OwnedRegion {
    fn drop(&mut self) {
        if let Some(region) = self.0.take() {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(region.0));
            }
        }
    }
}

fn process_id(hwnd: HWND) -> Option<u32> {
    let mut process_id = 0u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut process_id));
    }
    (process_id != 0).then_some(process_id)
}

fn region_kind(hwnd: HWND, destination: HRGN) -> i32 {
    unsafe { GetWindowRgn(hwnd, destination) }
}

fn current_region_matches(hwnd: HWND, expected: LocalClipRect) -> bool {
    let Some(current) = OwnedRegion::empty() else {
        return false;
    };
    if region_kind(hwnd, current.handle()) <= NULL_REGION {
        return false;
    }
    let Some(expected_region) = OwnedRegion::rectangle(expected) else {
        return false;
    };
    unsafe { EqualRgn(current.handle(), expected_region.handle()).as_bool() }
}

fn install_region(hwnd: HWND, region: OwnedRegion, redraw: bool) -> bool {
    let raw = region.transfer();
    let result = unsafe { SetWindowRgn(hwnd, Some(raw), redraw) };
    if result != 0 {
        true
    } else {
        drop(OwnedRegion(Some(raw)));
        false
    }
}

fn clear_region(hwnd: HWND, redraw: bool) -> bool {
    unsafe { SetWindowRgn(hwnd, None, redraw) != 0 }
}

fn valid_identity(hwnd: HWND, expected_process_id: u32) -> bool {
    unsafe { IsWindow(Some(hwnd)).as_bool() }
        && process_id(hwnd).is_some_and(|current| current == expected_process_id)
}

/// Apply or update a rectangular monitor clip without overwriting an
/// application-owned region.
pub(crate) fn apply_managed_region(
    window_id: WindowId,
    outer: Rect,
    bounds: Rect,
    redraw: bool,
) -> RegionApplyOutcome {
    let Ok(hwnd) = window_id_to_hwnd(window_id) else {
        return RegionApplyOutcome::Retry;
    };
    if !unsafe { IsWindow(Some(hwnd)).as_bool() } {
        forget_managed_window_region(window_id);
        return RegionApplyOutcome::Retry;
    }
    let Some(pid) = process_id(hwnd) else {
        return RegionApplyOutcome::Retry;
    };
    let Some(desired) = local_clip_rect(outer, bounds) else {
        return RegionApplyOutcome::Retry;
    };
    if desired.is_full_window(outer) {
        return clear_managed_region(window_id, redraw)
            .then_some(RegionApplyOutcome::Applied)
            .unwrap_or(RegionApplyOutcome::Retry);
    }

    let mut guard = lock_regions();
    let states = guard.get_or_insert_with(HashMap::new);
    if states.get(&window_id).is_some_and(|state| match *state {
        RegionState::Owned { process_id, .. } | RegionState::Unsupported { process_id, .. } => {
            process_id != pid
        }
    }) {
        states.remove(&window_id);
    }

    match states.get(&window_id).copied() {
        Some(RegionState::Unsupported {
            spec_bounds,
            process_id,
        }) if process_id == pid && spec_bounds == bounds => RegionApplyOutcome::Unsupported,
        Some(RegionState::Unsupported { .. }) => {
            states.remove(&window_id);
            RegionApplyOutcome::Unsupported
        }
        Some(RegionState::Owned {
            expected,
            process_id,
            ..
        }) => {
            if !valid_identity(hwnd, process_id) || !current_region_matches(hwnd, expected) {
                states.insert(
                    window_id,
                    RegionState::Unsupported {
                        spec_bounds: bounds,
                        process_id: pid,
                    },
                );
                return RegionApplyOutcome::Unsupported;
            }
            if expected == desired {
                states.insert(
                    window_id,
                    RegionState::Owned {
                        expected,
                        spec_bounds: bounds,
                        process_id: pid,
                    },
                );
                return RegionApplyOutcome::Applied;
            }
            let Some(region) = OwnedRegion::rectangle(desired) else {
                return RegionApplyOutcome::Retry;
            };
            if install_region(hwnd, region, redraw) {
                states.insert(
                    window_id,
                    RegionState::Owned {
                        expected: desired,
                        spec_bounds: bounds,
                        process_id: pid,
                    },
                );
                RegionApplyOutcome::Applied
            } else {
                RegionApplyOutcome::Retry
            }
        }
        None => {
            let Some(probe) = OwnedRegion::empty() else {
                return RegionApplyOutcome::Retry;
            };
            let kind = region_kind(hwnd, probe.handle());
            if kind == REGION_ERROR {
                return RegionApplyOutcome::Retry;
            }
            if kind != NULL_REGION {
                states.insert(
                    window_id,
                    RegionState::Unsupported {
                        spec_bounds: bounds,
                        process_id: pid,
                    },
                );
                return RegionApplyOutcome::Unsupported;
            }
            let Some(region) = OwnedRegion::rectangle(desired) else {
                return RegionApplyOutcome::Retry;
            };
            if install_region(hwnd, region, redraw) {
                states.insert(
                    window_id,
                    RegionState::Owned {
                        expected: desired,
                        spec_bounds: bounds,
                        process_id: pid,
                    },
                );
                RegionApplyOutcome::Applied
            } else {
                RegionApplyOutcome::Retry
            }
        }
    }
}

/// Clear LeopardWM's region only when the window still carries the exact
/// region we installed. Application replacements are never cleared.
pub(crate) fn clear_managed_region(window_id: WindowId, redraw: bool) -> bool {
    let Ok(hwnd) = window_id_to_hwnd(window_id) else {
        forget_managed_window_region(window_id);
        return false;
    };
    let state = {
        let mut guard = lock_regions();
        guard.as_mut().and_then(|states| states.remove(&window_id))
    };
    match state {
        None | Some(RegionState::Unsupported { .. }) => true,
        Some(RegionState::Owned {
            expected,
            process_id,
            ..
        }) => {
            if !valid_identity(hwnd, process_id) || !current_region_matches(hwnd, expected) {
                return true;
            }
            clear_region(hwnd, redraw)
        }
    }
}

/// Remove tracking for a destroyed/recycled HWND without touching the numeric
/// handle, which may already belong to another window.
pub fn forget_managed_window_region(window_id: WindowId) {
    let mut guard = lock_regions();
    if let Some(states) = guard.as_mut() {
        states.remove(&window_id);
    }
}

/// Clear regions for windows that left the current placement batch.
pub(crate) fn prune_managed_regions(active_ids: &HashSet<WindowId>) {
    let stale: Vec<WindowId> = {
        let guard = lock_regions();
        guard
            .as_ref()
            .map(|states| {
                states
                    .keys()
                    .filter(|window_id| !active_ids.contains(window_id))
                    .copied()
                    .collect()
            })
            .unwrap_or_default()
    };
    for window_id in stale {
        let _ = clear_managed_region(window_id, true);
    }
}

/// Best-effort panic/shutdown restoration.
pub fn restore_all_managed_window_regions() {
    let ids: Vec<WindowId> = {
        let guard = lock_regions();
        guard
            .as_ref()
            .map(|states| states.keys().copied().collect())
            .unwrap_or_default()
    };
    for window_id in ids {
        let _ = clear_managed_region(window_id, true);
    }
}

/// Whether the platform's region state already represents this exact desired
/// placement batch. Used by the daemon's unchanged-layout fast path.
pub fn managed_regions_match(
    active_ids: impl IntoIterator<Item = WindowId>,
    desired: &[crate::WindowRegionClip],
) -> bool {
    let active: HashSet<_> = active_ids.into_iter().collect();
    let guard = lock_regions();
    let Some(states) = guard.as_ref() else {
        return desired.is_empty();
    };
    if states.keys().any(|window_id| !active.contains(window_id)) {
        return false;
    }
    for clip in desired {
        let matches = states.get(&clip.window_id).is_some_and(|state| match *state {
            RegionState::Owned { spec_bounds, .. }
            | RegionState::Unsupported { spec_bounds, .. } => spec_bounds == clip.bounds,
        });
        if !matches {
            return false;
        }
    }
    states.iter().all(|(window_id, _)| {
        desired.iter().any(|clip| clip.window_id == *window_id)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_clip_handles_every_monitor_edge() {
        let outer = Rect::new(100, 100, 800, 600);
        assert_eq!(
            local_clip_rect(outer, Rect::new(200, 200, 500, 300)),
            Some(LocalClipRect {
                left: 100,
                top: 100,
                right: 600,
                bottom: 400,
            })
        );
        assert_eq!(local_clip_rect(outer, Rect::new(900, 100, 100, 100)), None);
    }

    #[test]
    fn local_clip_supports_negative_virtual_desktop_coordinates() {
        let outer = Rect::new(-2100, 20, 800, 600);
        let bounds = Rect::new(-1920, 0, 1920, 1080);
        assert_eq!(
            local_clip_rect(outer, bounds),
            Some(LocalClipRect {
                left: 180,
                top: 0,
                right: 800,
                bottom: 600,
            })
        );
    }

    #[test]
    fn local_clip_uses_exclusive_right_and_bottom_edges() {
        let outer = Rect::new(1800, 900, 400, 300);
        let bounds = Rect::new(0, 0, 1920, 1080);
        assert_eq!(
            local_clip_rect(outer, bounds),
            Some(LocalClipRect {
                left: 0,
                top: 0,
                right: 120,
                bottom: 180,
            })
        );
    }
}
