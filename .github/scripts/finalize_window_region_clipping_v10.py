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
    """    if unsafe { SetWindowRgn(hwnd, Some(region), redraw) } == 0 {
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
    }""",
    """    if unsafe { SetWindowRgn(hwnd, Some(region), redraw) } == 0 {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(region.0));
        }
        state.supported = false;
        if state.active {
            // Keep the durable ownership marker until restoration actually
            // succeeds. Removing it here would make a failed clear impossible
            // to recover after a process crash.
            if clear_region(hwnd, redraw) {
                remove_owner_marker(hwnd);
                state.active = false;
                state.last_region = None;
            }
        } else {
            remove_owner_marker(hwnd);
            state.last_region = None;
        }
        return false;
    }""",
)
insert = '''
/// Verify that the current HWND region still matches the cached ownership
/// state before an unchanged placement is skipped. Applications may clear a
/// region themselves, and a prior LeopardWM process may have left only the
/// durable HWND marker behind.
pub(crate) fn window_region_state_matches(window_id: WindowId, expect_clip: bool) -> bool {
    let Ok(hwnd) = window_id_to_hwnd(window_id) else {
        return false;
    };
    if !unsafe { IsWindow(Some(hwnd)).as_bool() } {
        return false;
    }
    let marker = has_owner_marker(hwnd);
    let region_present = !has_no_application_region(hwnd);
    let guard = lock_states();
    let active = guard
        .get(&window_id)
        .is_some_and(|state| state.active && is_same_window(window_id, &state.identity));
    if expect_clip {
        marker && region_present && active
    } else {
        !marker && !active
    }
}

'''
marker = "/// Restore an HWND to its original unregioned shape."
text = region.read_text(encoding="utf-8")
if marker not in text:
    raise RuntimeError("window_region.rs: state-match insertion marker missing")
text = text.replace(marker, insert + marker, 1)
text = text.replace(
    """pub fn forget_window_region(window_id: WindowId) {
    lock_states().remove(&window_id);
}""",
    """pub fn forget_window_region(window_id: WindowId) {
    // If this is an ordinary unmanage rather than a destroyed/recycled HWND,
    // restore first. Identity validation prevents touching a replacement HWND.
    let _ = restore_window_region(window_id, true);
    lock_states().remove(&window_id);
}""",
    1,
)
region.write_text(text, encoding="utf-8", newline="\n")

placement = Path("crates/platform_win32/src/placement.rs")
replace_once(
    placement,
    """        if previous == Some((placement.rect, placement.visibility))
            && previous_clip == current_clip
        {
            skipped += 1;
            continue;
        }""",
    """        if previous == Some((placement.rect, placement.visibility))
            && previous_clip == current_clip
            && crate::window_region::window_region_state_matches(
                placement.window_id,
                current_clip.is_some(),
            )
        {
            skipped += 1;
            continue;
        }""",
)
replace_once(
    placement,
    """pub fn clear_suspected_oversize(window_id: WindowId) {
    let mut guard = lock_suspected_oversize();
    if let Some(map) = guard.as_mut() {
        map.remove(&window_id);
    }
}""",
    """pub fn clear_suspected_oversize(window_id: WindowId) {
    let mut guard = lock_suspected_oversize();
    if let Some(map) = guard.as_mut() {
        map.remove(&window_id);
    }
    drop(guard);
    crate::window_region::forget_window_region(window_id);
}""",
)

print("final window-region clipping hardening applied")
