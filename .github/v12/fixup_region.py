from pathlib import Path

path = Path("crates/platform_win32/src/window_region.rs")
text = path.read_text(encoding="utf-8")


def replace(old: str, new: str, count: int = 1) -> None:
    global text
    actual = text.count(old)
    if actual != count:
        raise RuntimeError(f"expected {count}, found {actual}: {old[:100]!r}")
    text = text.replace(old, new)


replace(
    """fn live_responsive_hwnd(window_id: WindowId) -> Option<HWND> {
    let hwnd = window_id_to_hwnd(window_id).ok()?;
    unsafe {
        (IsWindow(Some(hwnd)).as_bool() && !IsHungAppWindow(hwnd).as_bool()).then_some(hwnd)
    }
}
""",
    """fn live_hwnd(window_id: WindowId) -> Option<HWND> {
    let hwnd = window_id_to_hwnd(window_id).ok()?;
    unsafe { IsWindow(Some(hwnd)).as_bool().then_some(hwnd) }
}

fn live_responsive_hwnd(window_id: WindowId) -> Option<HWND> {
    let hwnd = live_hwnd(window_id)?;
    unsafe { (!IsHungAppWindow(hwnd).as_bool()).then_some(hwnd) }
}
""",
)
replace(
    """            let mut book = lock_book();
            book.active.remove(&window_id);
            mark_unsupported(window_id, current_identity);
            return false;
""",
    """            let mut book = lock_book();
            book.active.remove(&window_id);
            drop(book);
            mark_unsupported(window_id, current_identity);
            return false;
""",
)
replace(
    """        if state.region == region_rect {
            if let Some(active) = lock_book().active.get_mut(&window_id) {
                active.last_verified = Instant::now();
            }
            return true;
        }
""",
    """        if state.region == region_rect {
            let mut book = lock_book();
            if let Some(active) = book.active.get_mut(&window_id) {
                active.last_verified = Instant::now();
            }
            return true;
        }
""",
)
replace(
    """pub(crate) fn restore_window_region(window_id: WindowId, redraw: bool) -> bool {
    let _commit = lock_commit();
    let Some(hwnd) = live_responsive_hwnd(window_id) else {
        if live_responsive_hwnd(window_id).is_none() {
            lock_book().active.remove(&window_id);
        }
        return false;
    };
""",
    """pub fn restore_window_region(window_id: WindowId, redraw: bool) -> bool {
    let _commit = lock_commit();
    let Some(hwnd) = live_hwnd(window_id) else {
        let mut book = lock_book();
        book.active.remove(&window_id);
        book.unsupported.remove(&window_id);
        return true;
    };
    if unsafe { IsHungAppWindow(hwnd).as_bool() } {
        // Do not synchronously call into a known-hung target. Keep ownership
        // state so a later layout or process can retry the restoration.
        return false;
    }
""",
)

path.write_text(text, encoding="utf-8", newline="\n")
print("window-region implementation fixups applied")
