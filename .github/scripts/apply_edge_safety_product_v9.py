from pathlib import Path


def replace_once(path: Path, old: str, new: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: expected one replacement target, found {count}")
    path.write_text(text.replace(old, new), encoding="utf-8", newline="\n")


def replace_section(path: Path, start: str, end: str, replacement: str) -> None:
    text = path.read_text(encoding="utf-8")
    start_at = text.find(start)
    end_at = text.find(end, start_at)
    if start_at < 0 or end_at < 0:
        raise RuntimeError(f"{path}: section markers not found")
    path.write_text(text[:start_at] + replacement + text[end_at:], encoding="utf-8", newline="\n")


layout = Path("crates/daemon/src/layout_apply.rs")
replace_section(
    layout,
    "/// Re-park any off-screen placement that would land on a NEIGHBOR monitor",
    "/// Pick an off-screen rect for `window`",
    """/// Keep tiled placements isolated to their owning monitor.
///
/// Non-focused tiled windows that intersect a neighboring output are parked
/// clear of every monitor. The active focused column is horizontally contained
/// by its work area instead, so focus navigation remains usable without leaking
/// pixels into another monitor. Floating windows may span monitors intentionally.
fn clamp_horizontally_inside(
    rect: leopardwm_core_layout::Rect,
    bounds: leopardwm_core_layout::Rect,
) -> leopardwm_core_layout::Rect {
    let width = rect.width.max(1).min(bounds.width.max(1));
    let max_x = bounds.x.saturating_add(bounds.width.max(1).saturating_sub(width));
    leopardwm_core_layout::Rect::new(rect.x.clamp(bounds.x, max_x), rect.y, width, rect.height)
}

pub(crate) fn park_offscreen_avoiding_neighbors(
    placements: &mut [leopardwm_core_layout::WindowPlacement],
    owner_id: leopardwm_platform_win32::MonitorId,
    focused_column: Option<usize>,
    monitors: &std::collections::HashMap<
        leopardwm_platform_win32::MonitorId,
        leopardwm_platform_win32::MonitorInfo,
    >,
    monitor_rects: &[leopardwm_core_layout::Rect],
) {
    use leopardwm_core_layout::Visibility;

    let Some(owner) = monitors.get(&owner_id) else {
        return;
    };
    let owner_rect = owner.rect;
    let intersects_neighbor = |rect: leopardwm_core_layout::Rect| {
        monitors
            .iter()
            .filter(|(id, _)| **id != owner_id)
            .any(|(_, monitor)| rect.intersects(&monitor.rect))
    };

    for placement in placements {
        if !intersects_neighbor(placement.rect) {
            continue;
        }

        if placement.visibility == Visibility::Visible {
            let crosses_horizontal_edge = placement.rect.x < owner_rect.x
                || placement.rect.right() > owner_rect.right();
            let crosses_vertical_edge = placement.rect.y < owner_rect.y
                || placement.rect.bottom() > owner_rect.bottom();

            // Mirrored displays can report overlapping coordinates. A window
            // wholly inside its owner is valid even if another monitor overlaps.
            if placement.column_index == usize::MAX
                || (!crosses_horizontal_edge && !crosses_vertical_edge)
            {
                continue;
            }

            if focused_column == Some(placement.column_index) && crosses_horizontal_edge {
                placement.rect = clamp_horizontally_inside(placement.rect, owner.work_area);
                if !crosses_vertical_edge && !intersects_neighbor(placement.rect) {
                    continue;
                }
            }

            placement.visibility = if placement.rect.x < owner_rect.x {
                Visibility::OffScreenLeft
            } else {
                Visibility::OffScreenRight
            };
        }

        placement.rect = offscreen_park_rect(placement.rect, owner_rect, monitor_rects);
    }
}

""",
)

replace_once(
    layout,
    """                    owner_ranges.push((*monitor_id, start, all_placements.len()));""",
    """                    let focused_column = (*monitor_id == self.focused_monitor)
                        .then(|| workspace.focused_column_index());
                    owner_ranges.push((*monitor_id, focused_column, start, all_placements.len()));""",
)
replace_once(
    layout,
    """        for (owner_id, start, end) in owner_ranges {
            park_offscreen_avoiding_neighbors(
                &mut all_placements[start..end],
                owner_id,
                &self.monitors,
                &monitor_rects,
            );
        }""",
    """        for (owner_id, focused_column, start, end) in owner_ranges {
            park_offscreen_avoiding_neighbors(
                &mut all_placements[start..end],
                owner_id,
                focused_column,
                &self.monitors,
                &monitor_rects,
            );
        }""",
)
replace_once(
    layout,
    """                park_offscreen_avoiding_neighbors(
                    std::slice::from_mut(placement),
                    owner_id,
                    &self.monitors,
                    &monitor_rects,
                );""",
    """                // Exiting windows are not the active focused column for
                // this frame; never pin an old workspace back on-screen.
                park_offscreen_avoiding_neighbors(
                    std::slice::from_mut(placement),
                    owner_id,
                    None,
                    &self.monitors,
                    &monitor_rects,
                );""",
)
replace_once(
    layout,
    """                    park_offscreen_avoiding_neighbors(
                        &mut placements,
                        *monitor_id,
                        &self.monitors,
                        &monitor_rects,
                    );""",
    """                    let focused_column = (*monitor_id == self.focused_monitor)
                        .then(|| workspace.focused_column_index());
                    park_offscreen_avoiding_neighbors(
                        &mut placements,
                        *monitor_id,
                        focused_column,
                        &self.monitors,
                        &monitor_rects,
                    );""",
)

# Existing in-file tests exercise the old three-argument contract.
text = layout.read_text(encoding="utf-8")
text = text.replace(
    "park_offscreen_avoiding_neighbors(&mut placements, 1, &monitors, &monitor_rects);",
    "park_offscreen_avoiding_neighbors(&mut placements, 1, None, &monitors, &monitor_rects);",
)
text = text.replace(
    "park_offscreen_avoiding_neighbors(placements, owner_id, &monitors, &rects);",
    "park_offscreen_avoiding_neighbors(placements, owner_id, None, &monitors, &rects);",
)
if text.count("park_offscreen_avoiding_neighbors(") != 7:
    raise RuntimeError("layout_apply.rs: unexpected isolation call count")
if "mod edge_safety_audit_tests;" in text:
    raise RuntimeError("layout_apply.rs: edge test module already declared")
text += "\n#[cfg(test)]\n#[path = \"layout_apply_edge_tests.rs\"]\nmod edge_safety_audit_tests;\n"
layout.write_text(text, encoding="utf-8", newline="\n")

print("edge safety product patch applied")
