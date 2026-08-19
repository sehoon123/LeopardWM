from pathlib import Path

path = Path("crates/platform_win32/src/placement.rs")
text = path.read_text(encoding="utf-8")


def replace(old: str, new: str, count: int = 1) -> None:
    global text
    actual = text.count(old)
    if actual != count:
        raise RuntimeError(f"expected {count}, found {actual}: {old[:120]!r}")
    text = text.replace(old, new)


replace(
    """            let outer = entries
                .iter()
                .find(|entry| entry.window_id == spec.window_id)
                .map(|entry| {
                    actual_outer_rect(
                        entry.hwnd,
                        Rect::new(entry.x, entry.y, entry.w.max(1), entry.h.max(1)),
                    )
                });
""",
    """            let outer = entries
                .iter()
                .find(|entry| entry.window_id == spec.window_id)
                .map(|entry| {
                    actual_outer_rect(
                        entry.hwnd,
                        Rect::new(entry.x, entry.y, entry.w.max(1), entry.h.max(1)),
                    )
                })
                .or_else(|| {
                    window_id_to_hwnd(spec.window_id)
                        .ok()
                        .map(|hwnd| actual_outer_rect(hwnd, placement.rect))
                });
""",
)
replace(
    """        if install_fallback(placement, entries, spec, high_contrast) {
            failed_window_ids.remove(&spec.window_id);
        } else {
            failed_window_ids.insert(spec.window_id);
            unrecoverable.push(spec.window_id);
        }
""",
    """        let needs_fallback = clip_candidates.contains(&spec.window_id)
            || failed_window_ids.contains(&spec.window_id)
            || entries
                .iter()
                .find(|entry| entry.window_id == spec.window_id)
                .is_some_and(|entry| !preferred_fallback_is_contained(entry, spec.clip_bounds));
        if !needs_fallback {
            continue;
        }

        if install_fallback(placement, entries, spec, high_contrast) {
            failed_window_ids.remove(&spec.window_id);
        } else {
            failed_window_ids.insert(spec.window_id);
            unrecoverable.push(spec.window_id);
        }
""",
)
replace(
    """    if crate::move_window_offscreen(placement.window_id).is_ok() {
        placement.rect = spec.safe_fallback_rect;
        placement.visibility = spec.safe_fallback_visibility;
        return true;
    }
    false
""",
    """    if crate::move_window_offscreen(placement.window_id).is_ok() {
        placement.rect = Rect::new(
            crate::MOVE_OFFSCREEN_SENTINEL_COORD,
            crate::MOVE_OFFSCREEN_SENTINEL_COORD,
            placement.rect.width,
            placement.rect.height,
        );
        placement.visibility = spec.safe_fallback_visibility;
    }
    // Do not cache this emergency path: returning false makes the daemon run
    // its existing error recovery while the HWND is already out of sight.
    false
""",
)
replace(
    """    let (mut applied, mut failed_window_ids) = position_entries(&entries);
    let active_region_ids = match reconcile_window_regions(
""",
    """    let (applied, mut failed_window_ids) = position_entries(&entries);
    let _active_region_ids = match reconcile_window_regions(
""",
)
start = """    applied = applied.saturating_add(
        entries
            .iter()
            .filter(|entry| !failed_window_ids.contains(&entry.window_id))
            .count() as u32
            .saturating_sub(applied),
    );
"""
replace(start, "")

path.write_text(text, encoding="utf-8", newline="\n")
print("placement reconciliation fixups applied")
