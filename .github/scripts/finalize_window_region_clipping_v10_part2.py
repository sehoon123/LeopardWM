from pathlib import Path


def replace_once(path: Path, old: str, new: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: expected one target, found {count}: {old[:100]!r}")
    path.write_text(text.replace(old, new), encoding="utf-8", newline="\n")


region = Path("crates/platform_win32/src/window_region.rs")
replace_once(
    region,
    "const NULL_REGION_KIND: i32 = 1;",
    "const NULL_REGION_KIND: i32 = 1;\nconst SIMPLE_REGION_KIND: i32 = 2;",
)
replace_once(
    region,
    """fn has_no_application_region(hwnd: HWND) -> bool {
    let mut bounds = RECT::default();
    unsafe { GetWindowRgnBox(hwnd, &mut bounds) } == NULL_REGION_KIND
}""",
    """fn current_region(hwnd: HWND) -> (i32, Rect) {
    let mut bounds = RECT::default();
    let kind = unsafe { GetWindowRgnBox(hwnd, &mut bounds) };
    (
        kind,
        Rect::new(
            bounds.left,
            bounds.top,
            bounds.right.saturating_sub(bounds.left),
            bounds.bottom.saturating_sub(bounds.top),
        ),
    )
}

fn has_no_application_region(hwnd: HWND) -> bool {
    current_region(hwnd).0 == NULL_REGION_KIND
}

fn owned_region_matches(hwnd: HWND, state: &RegionState) -> bool {
    let Some(expected) = state.last_region else {
        return false;
    };
    let (kind, actual) = current_region(hwnd);
    kind == SIMPLE_REGION_KIND && actual == expected && has_owner_marker(hwnd)
}""",
)
replace_once(
    region,
    """    if state.identity != current_identity || !state.supported {
        return false;
    }
    if state.active && state.last_region == Some(region_rect) {
        return true;
    }""",
    """    if state.identity != current_identity || !state.supported {
        return false;
    }
    if state.active && !owned_region_matches(hwnd, state) {
        // The application replaced our region while it was clipped. Relinquish
        // ownership without clearing the application's new shape, and force the
        // daemon onto the whole-window fallback.
        remove_owner_marker(hwnd);
        state.supported = false;
        state.active = false;
        state.last_region = None;
        return false;
    }
    if state.active && state.last_region == Some(region_rect) {
        return true;
    }""",
)
replace_once(
    region,
    """    let marker = has_owner_marker(hwnd);
    let region_present = !has_no_application_region(hwnd);
    let guard = lock_states();
    let active = guard
        .get(&window_id)
        .is_some_and(|state| state.active && is_same_window(window_id, &state.identity));
    if expect_clip {
        marker && region_present && active
    } else {
        !marker && !active
    }""",
    """    let marker = has_owner_marker(hwnd);
    let mut guard = lock_states();
    let active_matches = guard.get(&window_id).is_some_and(|state| {
        state.active
            && is_same_window(window_id, &state.identity)
            && owned_region_matches(hwnd, state)
    });
    if expect_clip {
        if marker && active_matches {
            true
        } else {
            if let Some(state) = guard.get_mut(&window_id) {
                if state.active && marker {
                    // A different region now belongs to the application. Do not
                    // clear it merely because our placement cache was stale.
                    remove_owner_marker(hwnd);
                    state.supported = false;
                    state.active = false;
                    state.last_region = None;
                }
            }
            false
        }
    } else {
        !marker && !guard.get(&window_id).is_some_and(|state| state.active)
    }""",
)
replace_once(
    region,
    """    if !is_same_window(window_id, &state.identity) {
        guard.remove(&window_id);
        return true;
    }
    let Ok(hwnd) = window_id_to_hwnd(window_id) else {
        guard.remove(&window_id);
        return true;
    };
    if !clear_region(hwnd, redraw) {""",
    """    if !is_same_window(window_id, &state.identity) {
        guard.remove(&window_id);
        return true;
    }
    let Ok(hwnd) = window_id_to_hwnd(window_id) else {
        guard.remove(&window_id);
        return true;
    };
    if !owned_region_matches(hwnd, state) {
        // The target application changed its own region. Drop only our marker
        // and state; clearing here would destroy an application-owned shape.
        remove_owner_marker(hwnd);
        state.supported = false;
        state.active = false;
        state.last_region = None;
        return true;
    }
    if !clear_region(hwnd, redraw) {""",
)

layout = Path("crates/daemon/src/layout_apply.rs")
text = layout.read_text(encoding="utf-8")
old = '''        if let Some(ref drag) = self.drag_state {
            if drag.is_tiled {
                all_placements.retain(|p| {
                    p.window_id != drag.hwnd && p.window_id != crate::state::DRAG_PLACEHOLDER_HWND
                });
            }
        }'''
new = '''        if let Some(ref drag) = self.drag_state {
            if drag.is_tiled {
                all_placements.retain(|p| {
                    p.window_id != drag.hwnd && p.window_id != crate::state::DRAG_PLACEHOLDER_HWND
                });
                window_clip_bounds.remove(&drag.hwnd);
                window_clip_bounds.remove(&crate::state::DRAG_PLACEHOLDER_HWND);
            }
        }'''
count = text.count(old)
if count != 2:
    raise RuntimeError(f"layout_apply.rs: expected two drag filters, found {count}")
text = text.replace(old, new)
layout.write_text(text, encoding="utf-8", newline="\n")

transitions = Path("crates/daemon/src/transitions.rs")
replace_once(
    transitions,
    """            let size_changed =
                start_rect.width != target_rect.width || start_rect.height != target_rect.height;
            if size_changes_only && !size_changed {
                continue;
            }""",
    """            let size_changed =
                start_rect.width != target_rect.width || start_rect.height != target_rect.height;
            if size_changes_only && !size_changed {
                continue;
            }
            // A DWM thumbnail is a separate top-level composition surface and
            // is not constrained by the source HWND's SetWindowRgn. Avoid the
            // ghost path at monitor edges; adaptive safe mode will collapse an
            // unprotected size-changing transition to an exact landing instead.
            let Some(work_area) = self.monitors.get(&monitor_id).map(|monitor| monitor.work_area)
            else {
                continue;
            };
            let crosses_horizontal_edge = |rect: leopardwm_core_layout::Rect| {
                rect.x < work_area.x || rect.right() > work_area.right()
            };
            if crosses_horizontal_edge(start_rect) || crosses_horizontal_edge(target_rect) {
                continue;
            }""",
)

print("second window-region clipping hardening pass applied")
