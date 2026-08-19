use super::prepare_monitor_overflow;
use crate::config::MonitorOverflowModeConfig;
use leopardwm_core_layout::{CenteringMode, Rect, Visibility, WindowPlacement, Workspace};
use leopardwm_platform_win32::{MonitorId, MonitorInfo};
use std::collections::HashMap;

const VIEWPORT_WIDTH: i32 = 1000;
const VIEWPORT_HEIGHT: i32 = 800;

fn monitor(id: MonitorId, x: i32) -> MonitorInfo {
    let rect = Rect::new(x, 0, VIEWPORT_WIDTH, VIEWPORT_HEIGHT);
    MonitorInfo {
        id,
        rect,
        work_area: rect,
        is_primary: id == 2,
        device_name: format!("DISPLAY{id}"),
        scale_factor: 1.0,
    }
}

fn three_monitors() -> HashMap<MonitorId, MonitorInfo> {
    HashMap::from([
        (1, monitor(1, 0)),
        (2, monitor(2, VIEWPORT_WIDTH)),
        (3, monitor(3, VIEWPORT_WIDTH * 2)),
    ])
}

fn workspace(widths: &[i32], mode: CenteringMode, focus: usize) -> Workspace {
    let mut workspace = Workspace::with_gaps(0, 0);
    for (index, width) in widths.iter().copied().enumerate() {
        workspace
            .insert_window(index as u64 + 1, Some(width))
            .unwrap();
    }
    workspace.set_centering_mode(mode);
    workspace.set_center_past_edges(true);
    workspace.set_focus(focus, 0).unwrap();
    workspace.ensure_focused_visible(VIEWPORT_WIDTH);
    workspace
}

fn visible_width(rect: Rect, viewport: Rect) -> i32 {
    if !rect.intersects(&viewport) {
        return 0;
    }
    rect.right()
        .min(viewport.right())
        .saturating_sub(rect.x.max(viewport.x))
        .max(0)
}

fn apply_clip(
    workspace: &Workspace,
) -> (
    Vec<WindowPlacement>,
    Vec<leopardwm_platform_win32::WindowRegionClip>,
) {
    let monitors = three_monitors();
    let owner = monitors[&2].rect;
    let monitor_rects: Vec<_> = monitors.values().map(|monitor| monitor.rect).collect();
    let mut placements = workspace.compute_placements(owner);
    let mut clips = Vec::new();
    prepare_monitor_overflow(
        &mut placements,
        2,
        Some(workspace.focused_column_index()),
        MonitorOverflowModeConfig::Clip,
        &monitors,
        &monitor_rects,
        &mut clips,
    );
    (placements, clips)
}

fn distribution(placements: &[WindowPlacement]) -> Vec<i32> {
    let owner = three_monitors()[&2].rect;
    let mut ordered = placements.to_vec();
    ordered.sort_by_key(|placement| placement.window_id);
    ordered
        .into_iter()
        .map(|placement| {
            if placement.visibility == Visibility::Visible {
                visible_width(placement.rect, owner)
            } else {
                0
            }
        })
        .collect()
}

#[test]
fn clip_preserves_just_in_view_50_100_for_each_focus() {
    let first = workspace(&[500, 1000], CenteringMode::JustInView, 0);
    let (placements, clips) = apply_clip(&first);
    assert_eq!(distribution(&placements), vec![500, 500]);
    assert_eq!(clips.len(), 1);
    assert_eq!(clips[0].window_id, 2);

    let second = workspace(&[500, 1000], CenteringMode::JustInView, 1);
    let (placements, clips) = apply_clip(&second);
    assert_eq!(distribution(&placements), vec![0, 1000]);
    assert!(clips.is_empty());
}

#[test]
fn clip_preserves_reverse_100_50_for_each_focus() {
    let first = workspace(&[1000, 500], CenteringMode::OnOverflow, 0);
    let (placements, clips) = apply_clip(&first);
    assert_eq!(distribution(&placements), vec![1000, 0]);
    assert!(clips.is_empty());

    let second = workspace(&[1000, 500], CenteringMode::OnOverflow, 1);
    let (placements, clips) = apply_clip(&second);
    assert_eq!(distribution(&placements), vec![500, 500]);
    assert_eq!(clips.len(), 1);
    assert_eq!(clips[0].window_id, 1);
}

#[test]
fn clip_preserves_center_and_on_overflow_distributions() {
    let centered = workspace(&[500, 500, 500], CenteringMode::Center, 1);
    let (placements, clips) = apply_clip(&centered);
    assert_eq!(distribution(&placements), vec![250, 500, 250]);
    assert_eq!(clips.len(), 2);

    let oversized = workspace(&[500, 1250, 500], CenteringMode::OnOverflow, 1);
    let (placements, clips) = apply_clip(&oversized);
    assert_eq!(distribution(&placements), vec![0, 1000, 0]);
    assert_eq!(clips.len(), 1);
    assert_eq!(clips[0].window_id, 2);
}

#[test]
fn clip_mode_never_mutates_visible_layout_geometry() {
    let widths = [250, 500, 750, 1000, 1250];
    for mode in [
        CenteringMode::Center,
        CenteringMode::JustInView,
        CenteringMode::OnOverflow,
    ] {
        for left in widths {
            for focused in widths {
                for right in widths {
                    for focus in 0..3 {
                        let workspace = workspace(&[left, focused, right], mode, focus);
                        let owner = three_monitors()[&2].rect;
                        let before: Vec<_> = workspace
                            .compute_placements(owner)
                            .into_iter()
                            .filter(|placement| placement.visibility == Visibility::Visible)
                            .map(|placement| {
                                (
                                    placement.window_id,
                                    placement.rect,
                                    placement.visibility,
                                    placement.column_index,
                                )
                            })
                            .collect();
                        let (after, _) = apply_clip(&workspace);
                        let after: Vec<_> = after
                            .into_iter()
                            .filter(|placement| placement.visibility == Visibility::Visible)
                            .map(|placement| {
                                (
                                    placement.window_id,
                                    placement.rect,
                                    placement.visibility,
                                    placement.column_index,
                                )
                            })
                            .collect();
                        assert_eq!(after, before);
                    }
                }
            }
        }
    }
}

#[test]
fn clip_plan_is_limited_to_windows_that_cross_a_neighbor() {
    let workspace = workspace(&[500, 500, 500], CenteringMode::Center, 1);
    let (placements, clips) = apply_clip(&workspace);
    let owner = three_monitors()[&2].rect;

    for clip in clips {
        let placement = placements
            .iter()
            .find(|placement| placement.window_id == clip.window_id)
            .unwrap();
        assert!(placement.rect.x < owner.x || placement.rect.right() > owner.right());
        assert_eq!(clip.clip_bounds, owner);
    }
}
