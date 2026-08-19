use leopardwm_core_layout::{CenteringMode, Rect, Visibility, Workspace};

const VIEWPORT: Rect = Rect {
    x: 0,
    y: 0,
    width: 1000,
    height: 800,
};

fn workspace(widths: &[i32], mode: CenteringMode) -> Workspace {
    let mut workspace = Workspace::with_gaps(0, 0);
    for (index, width) in widths.iter().copied().enumerate() {
        workspace
            .insert_window(index as u64 + 1, Some(width))
            .unwrap();
    }
    workspace.set_centering_mode(mode);
    workspace.set_center_past_edges(true);
    workspace
}

fn visible_widths(workspace: &Workspace) -> Vec<i32> {
    let mut placements = workspace.compute_placements(VIEWPORT);
    placements.sort_by_key(|placement| placement.window_id);
    placements
        .into_iter()
        .map(|placement| {
            let left = placement.rect.x.max(VIEWPORT.x);
            let right = placement.rect.right().min(VIEWPORT.right());
            right.saturating_sub(left).max(0)
        })
        .collect()
}

fn focused_placement(
    workspace: &Workspace,
    focus: usize,
) -> leopardwm_core_layout::WindowPlacement {
    workspace
        .compute_placements(VIEWPORT)
        .into_iter()
        .find(|placement| placement.window_id == focus as u64 + 1)
        .unwrap()
}

fn finish_animation(workspace: &mut Workspace) {
    while workspace.tick_animation(10_000) {}
}

#[test]
fn just_in_view_50_100_tracks_focus_without_losing_preview() {
    let mut workspace = workspace(&[500, 1000], CenteringMode::JustInView);

    workspace.set_focus(0, 0).unwrap();
    workspace.ensure_focused_visible(VIEWPORT.width);
    assert_eq!(workspace.scroll_offset(), 0.0);
    assert_eq!(visible_widths(&workspace), vec![500, 500]);

    workspace.set_focus(1, 0).unwrap();
    workspace.ensure_focused_visible(VIEWPORT.width);
    assert_eq!(workspace.scroll_offset(), 500.0);
    assert_eq!(visible_widths(&workspace), vec![0, 1000]);

    workspace.set_focus(0, 0).unwrap();
    workspace.ensure_focused_visible(VIEWPORT.width);
    assert_eq!(workspace.scroll_offset(), 0.0);
    assert_eq!(visible_widths(&workspace), vec![500, 500]);
}

#[test]
fn on_overflow_50_100_matches_just_in_view_for_fitting_columns() {
    let mut workspace = workspace(&[500, 1000], CenteringMode::OnOverflow);

    workspace.set_focus(0, 0).unwrap();
    workspace.ensure_focused_visible(VIEWPORT.width);
    assert_eq!(visible_widths(&workspace), vec![500, 500]);

    workspace.set_focus(1, 0).unwrap();
    workspace.ensure_focused_visible(VIEWPORT.width);
    assert_eq!(visible_widths(&workspace), vec![0, 1000]);
}

#[test]
fn reverse_100_50_distribution_is_focus_dependent() {
    for mode in [CenteringMode::JustInView, CenteringMode::OnOverflow] {
        let mut workspace = workspace(&[1000, 500], mode);

        workspace.set_focus(0, 0).unwrap();
        workspace.ensure_focused_visible(VIEWPORT.width);
        assert_eq!(visible_widths(&workspace), vec![1000, 0]);

        workspace.set_focus(1, 0).unwrap();
        workspace.ensure_focused_visible(VIEWPORT.width);
        assert_eq!(visible_widths(&workspace), vec![500, 500]);
    }
}

#[test]
fn center_mode_preview_ratio_generalizes_across_widths() {
    for width in [250, 500, 750, 1000, 1250] {
        let mut workspace = workspace(&[width, width, width], CenteringMode::Center);
        workspace.set_focus(1, 0).unwrap();
        workspace.ensure_focused_visible(VIEWPORT.width);

        let expected_focus = width.min(VIEWPORT.width);
        let expected_preview = width.min((VIEWPORT.width - expected_focus) / 2);
        assert_eq!(
            visible_widths(&workspace),
            vec![expected_preview, expected_focus, expected_preview]
        );
    }
}

#[test]
fn on_overflow_centers_only_columns_that_cannot_fit() {
    let mut workspace = workspace(&[500, 1250, 500], CenteringMode::OnOverflow);
    workspace.set_focus(1, 0).unwrap();
    workspace.ensure_focused_visible(VIEWPORT.width);

    assert_eq!(workspace.scroll_offset(), 625.0);
    assert_eq!(visible_widths(&workspace), vec![0, 1000, 0]);
    let focused = focused_placement(&workspace, 1);
    assert!((focused.rect.x + focused.rect.width / 2 - VIEWPORT.width / 2).abs() <= 1);
}

#[test]
fn just_in_view_does_not_edge_snap_an_oversized_column_already_in_view() {
    let mut workspace = workspace(&[500, 1500, 500], CenteringMode::JustInView);
    workspace.scroll_by(750.0, VIEWPORT.width);
    workspace.set_focus(1, 0).unwrap();
    workspace.ensure_focused_visible(VIEWPORT.width);
    assert_eq!(workspace.scroll_offset(), 750.0);

    let focused = focused_placement(&workspace, 1);
    assert!(focused.rect.x <= VIEWPORT.x);
    assert!(focused.rect.right() >= VIEWPORT.right());
}

#[test]
fn just_in_view_oversized_column_uses_nearest_valid_edge_only_when_needed() {
    let mut from_left = workspace(&[500, 1500, 500], CenteringMode::JustInView);
    from_left.set_focus(1, 0).unwrap();
    from_left.ensure_focused_visible(VIEWPORT.width);
    assert_eq!(from_left.scroll_offset(), 500.0);

    let mut from_right = workspace(&[500, 1500, 500], CenteringMode::JustInView);
    from_right.scroll_by(10_000.0, VIEWPORT.width);
    from_right.set_focus(1, 0).unwrap();
    from_right.ensure_focused_visible(VIEWPORT.width);
    assert_eq!(from_right.scroll_offset(), 1000.0);
}

#[test]
fn animated_and_reduced_motion_paths_match_the_synchronous_target() {
    let cases = [
        (vec![500, 1000], 0usize),
        (vec![500, 1000], 1usize),
        (vec![250, 750, 500], 1usize),
        (vec![500, 1250, 500], 1usize),
    ];

    for mode in [
        CenteringMode::Center,
        CenteringMode::JustInView,
        CenteringMode::OnOverflow,
    ] {
        for (widths, focus) in &cases {
            let mut synchronous = workspace(widths, mode);
            synchronous.scroll_by(333.0, VIEWPORT.width);
            synchronous.set_focus(*focus, 0).unwrap();
            synchronous.ensure_focused_visible(VIEWPORT.width);

            let mut animated = workspace(widths, mode);
            animated.scroll_by(333.0, VIEWPORT.width);
            animated.set_focus(*focus, 0).unwrap();
            animated.ensure_focused_visible_animated(VIEWPORT.width);
            finish_animation(&mut animated);

            let mut reduced = workspace(widths, mode);
            reduced.scroll_by(333.0, VIEWPORT.width);
            reduced.set_reduce_motion(true);
            reduced.set_focus(*focus, 0).unwrap();
            reduced.ensure_focused_visible_animated(VIEWPORT.width);

            assert!((animated.scroll_offset() - synchronous.scroll_offset()).abs() < 0.001);
            assert!((reduced.scroll_offset() - synchronous.scroll_offset()).abs() < 0.001);
        }
    }
}

#[test]
fn interrupted_animation_converges_to_the_new_focus_target() {
    let mut workspace = workspace(&[500, 1000, 500], CenteringMode::JustInView);
    workspace.set_focus(1, 0).unwrap();
    workspace.ensure_focused_visible_animated(VIEWPORT.width);
    assert!(workspace.tick_animation(50));

    workspace.set_focus(0, 0).unwrap();
    workspace.ensure_focused_visible_animated(VIEWPORT.width);
    finish_animation(&mut workspace);

    assert_eq!(workspace.scroll_offset(), 0.0);
    assert_eq!(visible_widths(&workspace), vec![500, 500, 0]);
}

#[test]
fn odd_pixel_centering_has_at_most_one_pixel_of_preview_bias() {
    let viewport = Rect::new(0, 0, 1001, 800);
    let mut workspace = Workspace::with_gaps(0, 0);
    for id in 1..=3 {
        workspace.insert_window(id, Some(501)).unwrap();
    }
    workspace.set_centering_mode(CenteringMode::Center);
    workspace.set_center_past_edges(true);
    workspace.set_focus(1, 0).unwrap();
    workspace.ensure_focused_visible(viewport.width);

    let mut placements = workspace.compute_placements(viewport);
    placements.sort_by_key(|placement| placement.window_id);
    let visible: Vec<i32> = placements
        .iter()
        .map(|placement| {
            placement
                .rect
                .right()
                .min(viewport.right())
                .saturating_sub(placement.rect.x.max(viewport.x))
                .max(0)
        })
        .collect();
    assert_eq!(visible[1], 501);
    assert!((visible[0] - visible[2]).abs() <= 1);
}

#[test]
fn exhaustive_mode_matrix_preserves_each_modes_invariants() {
    let widths = [250, 500, 750, 1000, 1250];
    for mode in [
        CenteringMode::Center,
        CenteringMode::JustInView,
        CenteringMode::OnOverflow,
    ] {
        for left in widths {
            for focused_width in widths {
                for right in widths {
                    for focus in 0..3 {
                        for start_ratio in [0.0, 0.5, 1.0] {
                            let mut probe = workspace(&[left, focused_width, right], mode);
                            probe.scroll_by(10_000.0, VIEWPORT.width);
                            let maximum = probe.scroll_offset();

                            let mut workspace = workspace(&[left, focused_width, right], mode);
                            workspace.scroll_by(maximum * start_ratio, VIEWPORT.width);
                            workspace.set_focus(focus, 0).unwrap();
                            workspace.ensure_focused_visible(VIEWPORT.width);

                            assert!(workspace.scroll_offset().is_finite());
                            let placement = focused_placement(&workspace, focus);
                            assert_eq!(placement.visibility, Visibility::Visible);
                            let width = [left, focused_width, right][focus];
                            let centers = mode == CenteringMode::Center
                                || (mode == CenteringMode::OnOverflow && width > VIEWPORT.width);
                            if centers {
                                let focused_center =
                                    placement.rect.x as i64 + placement.rect.width as i64 / 2;
                                let viewport_center = VIEWPORT.x as i64 + VIEWPORT.width as i64 / 2;
                                assert!((focused_center - viewport_center).abs() <= 1);
                            } else if width <= VIEWPORT.width {
                                assert!(placement.rect.x >= VIEWPORT.x);
                                assert!(placement.rect.right() <= VIEWPORT.right());
                            } else {
                                assert!(placement.rect.x <= VIEWPORT.x);
                                assert!(placement.rect.right() >= VIEWPORT.right());
                            }
                        }
                    }
                }
            }
        }
    }
}
