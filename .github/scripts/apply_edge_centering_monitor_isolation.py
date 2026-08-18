from pathlib import Path


def replace_once(path: Path, old: str, new: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: expected one replacement target, found {count}")
    path.write_text(text.replace(old, new), encoding="utf-8", newline="\n")


def replace_section(path: Path, start_marker: str, end_marker: str, replacement: str) -> None:
    text = path.read_text(encoding="utf-8")
    start = text.find(start_marker)
    if start < 0:
        raise RuntimeError(f"{path}: start marker not found: {start_marker!r}")
    end = text.find(end_marker, start)
    if end < 0:
        raise RuntimeError(f"{path}: end marker not found: {end_marker!r}")
    path.write_text(text[:start] + replacement + text[end:], encoding="utf-8", newline="\n")


operations = Path("crates/core_layout/src/workspace/operations.rs")
replace_once(
    operations,
    """        // Clamp target to valid range (visible area = viewport minus outer padding)\n        let vis_w = self.visible_width(viewport_width);\n        let max_scroll = (self.total_width() - vis_w).max(0);\n        let clamped_target = target.clamp(0.0, max_scroll as f64);\n""",
    """        // Center mode may intentionally scroll past the strip edges so the\n        // first and last active columns can reach the viewport center. Manual\n        // scrolling remains clamped by `scroll_by`.\n        let vis_w = self.visible_width(viewport_width);\n        let (min_scroll, max_scroll) = self.focused_scroll_bounds(vis_w);\n        let clamped_target = target.clamp(min_scroll, max_scroll);\n""",
)

operations_tests = r'''

#[cfg(test)]
mod edge_centering_tests {
    use super::*;

    fn five_columns() -> Workspace {
        let mut workspace = Workspace::with_gaps(10, 10);
        for window_id in 1..=5 {
            workspace.insert_window(window_id, Some(600)).unwrap();
        }
        workspace.set_centering_mode(CenteringMode::Center);
        workspace.set_center_past_edges(true);
        workspace
    }

    fn finish_scroll(workspace: &mut Workspace) {
        assert!(!workspace.tick_animation(10_000));
    }

    #[test]
    fn animated_centering_places_first_column_at_viewport_center() {
        let mut workspace = five_columns();
        workspace.set_focus(0, 0).unwrap();

        workspace.ensure_focused_visible_animated(1920);
        finish_scroll(&mut workspace);

        // visible width = 1920 - 10 - 10 = 1900
        // target = 0 + 600/2 - 1900/2 = -650
        assert_eq!(workspace.scroll_offset(), -650.0);
    }

    #[test]
    fn animated_centering_places_last_column_at_viewport_center() {
        let mut workspace = five_columns();
        workspace.set_focus(4, 0).unwrap();

        workspace.ensure_focused_visible_animated(1920);
        finish_scroll(&mut workspace);

        // last x = 4 * (600 + 10) = 2440
        // target = 2440 + 600/2 - 1900/2 = 1790
        assert_eq!(workspace.scroll_offset(), 1790.0);
    }

    #[test]
    fn disabling_edge_centering_keeps_normal_scroll_bounds() {
        let mut workspace = five_columns();
        workspace.set_center_past_edges(false);
        workspace.set_focus(0, 0).unwrap();

        workspace.ensure_focused_visible_animated(1920);
        finish_scroll(&mut workspace);

        assert_eq!(workspace.scroll_offset(), 0.0);
    }

    #[test]
    fn manual_scroll_never_creates_edge_blank_space() {
        let mut workspace = five_columns();
        workspace.set_focus(0, 0).unwrap();

        workspace.scroll_by(-10_000.0, 1920);
        assert_eq!(workspace.scroll_offset(), 0.0);
    }
}
'''
text = operations.read_text(encoding="utf-8")
if "mod edge_centering_tests" not in text:
    operations.write_text(text.rstrip() + operations_tests + "\n", encoding="utf-8", newline="\n")

layout_apply = Path("crates/daemon/src/layout_apply.rs")
new_park_function = r'''fn park_offscreen_avoiding_neighbors(
    placements: &mut [leopardwm_core_layout::WindowPlacement],
    owner_id: leopardwm_platform_win32::MonitorId,
    monitors: &std::collections::HashMap<
        leopardwm_platform_win32::MonitorId,
        leopardwm_platform_win32::MonitorInfo,
    >,
    monitor_rects: &[leopardwm_core_layout::Rect],
) {
    use leopardwm_core_layout::Visibility;

    let Some(owner) = monitors.get(&owner_id).map(|monitor| monitor.rect) else {
        return;
    };

    for placement in placements {
        let intersects_neighbor = monitors
            .iter()
            .filter(|(id, _)| **id != owner_id)
            .any(|(_, monitor)| placement.rect.intersects(&monitor.rect));
        if !intersects_neighbor {
            continue;
        }

        if placement.visibility == Visibility::Visible {
            let crosses_owner_edge = placement.rect.x < owner.x
                || placement.rect.right() > owner.right()
                || placement.rect.y < owner.y
                || placement.rect.bottom() > owner.bottom();
            // Floating windows may intentionally span monitors. Tiled windows
            // are hidden as a unit rather than leaking into a neighbor output.
            if placement.column_index == usize::MAX || !crosses_owner_edge {
                continue;
            }
            placement.visibility = if placement.rect.x < owner.x {
                Visibility::OffScreenLeft
            } else {
                Visibility::OffScreenRight
            };
        }

        placement.rect = offscreen_park_rect(placement.rect, owner, monitor_rects);
    }
}

'''
replace_section(
    layout_apply,
    "fn park_offscreen_avoiding_neighbors(",
    "/// Pick an off-screen rect",
    new_park_function,
)

text = layout_apply.read_text(encoding="utf-8")
method_start = text.find("    pub(crate) fn send_animation_frame(")
if method_start < 0:
    raise RuntimeError("send_animation_frame not found")
block_start = text.find("        let mut all_placements = Vec::new();", method_start)
block_end = text.find("        // Filter out the dragged window", block_start)
if block_start < 0 or block_end < 0:
    raise RuntimeError("animation placement collection block not found")
new_animation_block = r'''        let mut all_placements = Vec::new();
        let monitor_rects: Vec<_> = self.monitors.values().map(|monitor| monitor.rect).collect();
        let mut owner_ranges = Vec::with_capacity(self.monitors.len());
        for (monitor_id, ws_vec) in &self.workspaces {
            let idx = self.active_workspace_idx(*monitor_id);
            if let Some(workspace) = ws_vec.get(idx) {
                if self.monitors.contains_key(monitor_id) {
                    let viewport = self.layout_viewport(*monitor_id);
                    let start = all_placements.len();
                    all_placements.extend(workspace.compute_placements_animated(viewport));
                    owner_ranges.push((*monitor_id, start, all_placements.len()));
                }
            }
        }
        let base_placement_len = all_placements.len();
        if all_placements.is_empty()
            && self
                .layout_transition
                .as_ref()
                .is_none_or(|transition| transition.exit_rects.is_empty())
        {
            return Ok(false);
        }

        // Interpolate first, then enforce monitor isolation against the actual
        // frame rectangles. Doing this earlier lets a transition move a visible
        // tiled window into a neighboring monitor after the safety check.
        if let Some(ref transition) = self.layout_transition {
            Self::apply_transition_interpolation(transition, &mut all_placements);
        }
        for (owner_id, start, end) in owner_ranges {
            park_offscreen_avoiding_neighbors(
                &mut all_placements[start..end],
                owner_id,
                &self.monitors,
                &monitor_rects,
            );
        }
        // `apply_transition_interpolation` appends exiting windows. They are
        // few, so resolve only those owners rather than allocating a per-frame
        // HWND-to-monitor map for every placement.
        for placement in &mut all_placements[base_placement_len..] {
            if let Some((owner_id, _)) = self.find_window_workspace(placement.window_id) {
                park_offscreen_avoiding_neighbors(
                    std::slice::from_mut(placement),
                    owner_id,
                    &self.monitors,
                    &monitor_rects,
                );
            }
        }

'''
layout_apply.write_text(
    text[:block_start] + new_animation_block + text[block_end:],
    encoding="utf-8",
    newline="\n",
)

layout_tests = r'''

#[cfg(test)]
mod monitor_isolation_tests {
    use super::*;
    use leopardwm_core_layout::{Rect, Visibility, WindowPlacement};
    use leopardwm_platform_win32::{MonitorId, MonitorInfo};
    use std::collections::HashMap;

    fn monitor(id: MonitorId, x: i32) -> MonitorInfo {
        let rect = Rect::new(x, 0, 1920, 1080);
        MonitorInfo {
            id,
            rect,
            work_area: rect,
            is_primary: id == 1,
            device_name: format!("DISPLAY{id}"),
            scale_factor: 1.0,
        }
    }

    fn side_by_side_monitors() -> HashMap<MonitorId, MonitorInfo> {
        HashMap::from([(1, monitor(1, 0)), (2, monitor(2, 1920))])
    }

    fn isolate(placements: &mut [WindowPlacement], owner_id: MonitorId) {
        let monitors = side_by_side_monitors();
        let rects: Vec<_> = monitors.values().map(|monitor| monitor.rect).collect();
        park_offscreen_avoiding_neighbors(placements, owner_id, &monitors, &rects);
    }

    #[test]
    fn partially_visible_tiled_window_is_hidden_from_right_neighbor() {
        let mut placements = vec![WindowPlacement {
            window_id: 1,
            rect: Rect::new(1800, 40, 400, 800),
            visibility: Visibility::Visible,
            column_index: 0,
        }];

        isolate(&mut placements, 1);

        assert_eq!(placements[0].visibility, Visibility::OffScreenRight);
        assert!(side_by_side_monitors()
            .values()
            .all(|monitor| !placements[0].rect.intersects(&monitor.rect)));
    }

    #[test]
    fn partially_visible_tiled_window_is_hidden_from_left_neighbor() {
        let mut placements = vec![WindowPlacement {
            window_id: 2,
            rect: Rect::new(1800, 40, 400, 800),
            visibility: Visibility::Visible,
            column_index: 0,
        }];

        isolate(&mut placements, 2);

        assert_eq!(placements[0].visibility, Visibility::OffScreenLeft);
        assert!(side_by_side_monitors()
            .values()
            .all(|monitor| !placements[0].rect.intersects(&monitor.rect)));
    }

    #[test]
    fn fully_contained_tiled_window_remains_visible() {
        let original = Rect::new(100, 40, 800, 800);
        let mut placements = vec![WindowPlacement {
            window_id: 3,
            rect: original,
            visibility: Visibility::Visible,
            column_index: 0,
        }];

        isolate(&mut placements, 1);

        assert_eq!(placements[0].visibility, Visibility::Visible);
        assert_eq!(placements[0].rect, original);
    }

    #[test]
    fn floating_window_may_intentionally_span_monitors() {
        let original = Rect::new(1800, 40, 400, 800);
        let mut placements = vec![WindowPlacement {
            window_id: 4,
            rect: original,
            visibility: Visibility::Visible,
            column_index: usize::MAX,
        }];

        isolate(&mut placements, 1);

        assert_eq!(placements[0].visibility, Visibility::Visible);
        assert_eq!(placements[0].rect, original);
    }

    #[test]
    fn existing_offscreen_placement_is_reparked_clear_of_neighbors() {
        let mut placements = vec![WindowPlacement {
            window_id: 5,
            rect: Rect::new(1920, 40, 600, 800),
            visibility: Visibility::OffScreenRight,
            column_index: 1,
        }];

        isolate(&mut placements, 1);

        assert_eq!(placements[0].visibility, Visibility::OffScreenRight);
        assert!(side_by_side_monitors()
            .values()
            .all(|monitor| !placements[0].rect.intersects(&monitor.rect)));
    }
}
'''
text = layout_apply.read_text(encoding="utf-8")
if "mod monitor_isolation_tests" not in text:
    layout_apply.write_text(text.rstrip() + layout_tests + "\n", encoding="utf-8", newline="\n")

print("edge centering and monitor isolation patch applied")
