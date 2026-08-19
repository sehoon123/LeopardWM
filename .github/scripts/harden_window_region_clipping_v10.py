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
    """    if state.active && state.last_region == Some(region_rect) {
        return true;
    }

    let marker_was_new = !state.active;""",
    """    if state.active && state.last_region == Some(region_rect) {
        return true;
    }

    // Capability may have been cached while the application had no region.
    // Re-check before every inactive -> active transition so a later custom
    // region is never overwritten.
    if !state.active && !has_no_application_region(hwnd) {
        state.supported = false;
        return false;
    }

    let marker_was_new = !state.active;""",
)
replace_once(
    region,
    """    let mut guard = lock_states();
    let Some(state) = guard.get_mut(&window_id) else {
        return true;
    };""",
    """    let mut guard = lock_states();
    let Some(state) = guard.get_mut(&window_id) else {
        // A SetWindowRgn region can outlive a crashed LeopardWM process. The
        // HWND property is our durable ownership marker, so the first ordinary
        // placement after restart restores the normal rectangular shape even
        // when this window no longer needs clipping.
        drop(guard);
        let Ok(hwnd) = window_id_to_hwnd(window_id) else {
            return true;
        };
        if has_owner_marker(hwnd) {
            if !clear_region(hwnd, redraw) {
                return false;
            }
            remove_owner_marker(hwnd);
        }
        return true;
    };""",
)

# Include clip-plan identity in the exact-apply fast path and update path.
layout = Path("crates/daemon/src/layout_apply.rs")
text = layout.read_text(encoding="utf-8")
if "last_placed_clip_bounds" not in text:
    raise RuntimeError("layout_apply.rs: clip cache integration missing")
# The broad first-pass replacement may have inserted clip-cache clears with
# conservative indentation. Rustfmt handles whitespace, but ensure no duplicate
# clear was introduced by a rerun.
text = text.replace(
    "self.last_placed_clip_bounds.clear();\n                self.last_placed_clip_bounds.clear();",
    "self.last_placed_clip_bounds.clear();",
)
layout.write_text(text, encoding="utf-8", newline="\n")

print("window-region clipping hardening applied")
