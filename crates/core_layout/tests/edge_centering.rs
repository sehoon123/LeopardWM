use leopardwm_core_layout::{CenteringMode, Rect, WindowPlacement, Workspace};

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

fn placement_center(workspace: &Workspace, window_id: u64) -> i32 {
    let placement = workspace
        .compute_placements(Rect::new(0, 0, 1920, 1080))
        .into_iter()
        .find(|placement| placement.window_id == window_id)
        .unwrap();
    placement.rect.x + placement.rect.width / 2
}

fn visible_intersection_width(rect: Rect, viewport: Rect) -> i32 {
    rect.right()
        .min(viewport.right())
        .saturating_sub(rect.x.max(viewport.x))
        .max(0)
}

fn centered_three_columns(column_width: i32) -> Vec<WindowPlacement> {
    let mut workspace = Workspace::with_gaps(0, 0);
    for window_id in 1..=3 {
        workspace.insert_window(window_id, Some(column_width)).unwrap();
    }
    workspace.set_centering_mode(CenteringMode::Center);
    workspace.set_center_past_edges(true);
    workspace.set_focus(1, 0).unwrap();
    workspace.ensure_focused_visible(2000);
    workspace.compute_placements(Rect::new(0, 0, 2000, 1000))
}

#[test]
fn first_and_last_columns_reach_the_true_viewport_center() {
    for (column, window_id, offset) in [(0, 1, -650.0), (4, 5, 1790.0)] {
        let mut workspace = five_columns();
        workspace.set_focus(column, 0).unwrap();

        workspace.ensure_focused_visible_animated(1920);
        finish_scroll(&mut workspace);

        assert_eq!(workspace.scroll_offset(), offset);
        assert_eq!(placement_center(&workspace, window_id), 960);
    }
}

#[test]
fn first_and_last_active_columns_center_across_minimized_edges() {
    let mut first = five_columns();
    assert!(first.mark_minimized(1));
    first.set_focus(1, 0).unwrap();
    first.ensure_focused_visible_animated(1920);
    finish_scroll(&mut first);
    assert_eq!(placement_center(&first, 2), 960);

    let mut last = five_columns();
    assert!(last.mark_minimized(5));
    last.set_focus(3, 0).unwrap();
    last.ensure_focused_visible_animated(1920);
    finish_scroll(&mut last);
    assert_eq!(placement_center(&last, 4), 960);
}

#[test]
fn reduce_motion_and_interrupted_animation_keep_exact_edge_targets() {
    let mut reduced = five_columns();
    reduced.set_reduce_motion(true);
    reduced.set_focus(4, 0).unwrap();
    reduced.ensure_focused_visible_animated(1920);
    assert!(!reduced.is_animating());
    assert_eq!(reduced.scroll_offset(), 1790.0);

    let mut interrupted = five_columns();
    interrupted.set_focus(4, 0).unwrap();
    interrupted.ensure_focused_visible_animated(1920);
    assert!(interrupted.tick_animation(50));
    interrupted.set_focus(0, 0).unwrap();
    interrupted.ensure_focused_visible_animated(1920);
    finish_scroll(&mut interrupted);
    assert_eq!(interrupted.scroll_offset(), -650.0);
}

#[test]
fn edge_blank_space_is_opt_in_and_manual_scroll_remains_bounded() {
    let mut workspace = five_columns();
    workspace.set_focus(0, 0).unwrap();
    workspace.set_center_past_edges(false);
    workspace.center_focused_column_animated(1920);
    finish_scroll(&mut workspace);
    assert_eq!(workspace.scroll_offset(), 0.0);

    workspace.set_center_past_edges(true);
    workspace.center_focused_column_animated(1920);
    finish_scroll(&mut workspace);
    assert_eq!(workspace.scroll_offset(), -650.0);

    workspace.scroll_by(-10_000.0, 1920);
    assert_eq!(workspace.scroll_offset(), 0.0);
}

#[test]
fn centered_half_width_column_keeps_symmetric_quarter_previews() {
    let viewport = Rect::new(0, 0, 2000, 1000);
    let placements = centered_three_columns(1000);
    let visible_widths: Vec<_> = placements
        .iter()
        .map(|placement| visible_intersection_width(placement.rect, viewport))
        .collect();
    assert_eq!(visible_widths, [500, 1000, 500]);
}

#[test]
fn centered_three_quarter_width_column_keeps_symmetric_eighth_previews() {
    let viewport = Rect::new(0, 0, 2000, 1000);
    let placements = centered_three_columns(1500);
    let visible_widths: Vec<_> = placements
        .iter()
        .map(|placement| visible_intersection_width(placement.rect, viewport))
        .collect();
    assert_eq!(visible_widths, [250, 1500, 250]);
}
