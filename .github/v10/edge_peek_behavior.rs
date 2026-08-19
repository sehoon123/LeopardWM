use leopardwm_core_layout::{CenteringMode, Rect, Visibility, Workspace};

fn half_width_workspace() -> Workspace {
    let mut workspace = Workspace::with_gaps(10, 10);
    for window_id in 1..=5 {
        workspace.insert_window(window_id, Some(940)).unwrap();
    }
    workspace.set_centering_mode(CenteringMode::Center);
    workspace.set_center_past_edges(true);
    workspace
}

#[test]
fn centered_middle_column_keeps_both_neighbors_partially_visible() {
    let mut workspace = half_width_workspace();
    workspace.set_focus(2, 0).unwrap();
    workspace.ensure_focused_visible(1920);

    let placements = workspace.compute_placements(Rect::new(0, 0, 1920, 1080));
    let visible: Vec<_> = placements
        .iter()
        .filter(|placement| placement.visibility == Visibility::Visible)
        .collect();

    assert_eq!(visible.len(), 3);
    assert_eq!(visible[1].window_id, 3);
    assert_eq!(visible[1].rect.x + visible[1].rect.width / 2, 960);
    assert!(visible[0].rect.x < 0 && visible[0].rect.right() > 0);
    assert!(visible[2].rect.x < 1920 && visible[2].rect.right() > 1920);
}

#[test]
fn centered_first_column_keeps_the_next_column_peeking() {
    let mut workspace = half_width_workspace();
    workspace.set_focus(0, 0).unwrap();
    workspace.ensure_focused_visible(1920);

    let placements = workspace.compute_placements(Rect::new(0, 0, 1920, 1080));
    let first = placements
        .iter()
        .find(|placement| placement.window_id == 1)
        .unwrap();
    let second = placements
        .iter()
        .find(|placement| placement.window_id == 2)
        .unwrap();

    assert_eq!(first.rect.x + first.rect.width / 2, 960);
    assert_eq!(first.visibility, Visibility::Visible);
    assert_eq!(second.visibility, Visibility::Visible);
    assert!(second.rect.x < 1920 && second.rect.right() > 1920);
}
