from pathlib import Path

ROOT = Path.cwd()


def patch(path: str, old: str, new: str, expected: int = 1) -> None:
    target = ROOT / path
    text = target.read_text(encoding="utf-8")
    count = text.count(old)
    if count != expected:
        raise RuntimeError(f"{path}: expected {expected}, found {count}: {old[:100]!r}")
    target.write_text(text.replace(old, new), encoding="utf-8", newline="\n")


region = "crates/platform_win32/src/window_region.rs"
patch(
    region,
    "const OWNER_MARKER: HANDLE = HANDLE(0x4c57_4d52usize as *mut c_void);\n",
    "fn owner_marker() -> HANDLE {\n    HANDLE(0x4c57_4d52usize as *mut c_void)\n}\n",
)
patch(region, "prop(hwnd, OWNER_PROP) != OWNER_MARKER", "prop(hwnd, OWNER_PROP) != owner_marker()", 2)
patch(region, "(OWNER_PROP, OWNER_MARKER),", "(OWNER_PROP, owner_marker()),")
patch(
    region,
    """    if !write_marker(hwnd, region_rect) {
        return false;
    }
""",
    """    if !write_marker(hwnd, region_rect) {
        if let Some(old) = old_region {
            let _ = write_marker(hwnd, old);
        }
        return false;
    }
""",
)

layout = "crates/daemon/src/layout_apply.rs"
patch(
    layout,
    "fn upsert_region_clip(\n",
    """fn clamp_horizontally_inside(
    rect: leopardwm_core_layout::Rect,
    bounds: leopardwm_core_layout::Rect,
) -> leopardwm_core_layout::Rect {
    let width = rect.width.max(1).min(bounds.width.max(1));
    let max_x = bounds
        .x
        .saturating_add(bounds.width.max(1).saturating_sub(width));
    leopardwm_core_layout::Rect::new(rect.x.clamp(bounds.x, max_x), rect.y, width, rect.height)
}

fn upsert_region_clip(
""",
)

placement = "crates/platform_win32/src/placement.rs"
patch(
    placement,
    """        let Some(spec) = clip_spec_for(specs, placement.window_id) else {
            effective.push(placement.clone());
            continue;
        };
        if placement.visibility != Visibility::Visible || placement.column_index == usize::MAX {
            effective.push(placement.clone());
            continue;
        }
""",
    """        let Some(spec) = clip_spec_for(specs, placement.window_id) else {
            let _ = crate::window_region::restore_window_region(placement.window_id, false);
            effective.push(placement.clone());
            continue;
        };
        if placement.visibility != Visibility::Visible || placement.column_index == usize::MAX {
            let _ = crate::window_region::restore_window_region(placement.window_id, false);
            effective.push(placement.clone());
            continue;
        }
""",
)
patch(
    placement,
    "    let (applied, mut failed_window_ids) = position_entries(&entries);\n",
    "    let (applied, failed_window_ids) = position_entries(&entries);\n",
)

print("real v10 hardening fixups applied")
