use super::apply_monitor_overflow_policy;
use crate::config::MonitorOverflowModeConfig;
use leopardwm_core_layout::{Rect, Visibility, WindowPlacement};
use leopardwm_platform_win32::{MonitorId, MonitorInfo, WindowRegionClip};
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

fn placement(window_id: u64, x: i32, width: i32, column_index: usize) -> WindowPlacement {
    WindowPlacement {
        window_id,
        rect: Rect::new(x, 40, width, 800),
        visibility: Visibility::Visible,
        column_index,
    }
}

fn apply(
    placements: &mut [WindowPlacement],
    owner_id: MonitorId,
    focused_column: Option<usize>,
    mode: MonitorOverflowModeConfig,
    monitors: &HashMap<MonitorId, MonitorInfo>,
) -> Vec<WindowRegionClip> {
    let monitor_rects: Vec<_> = monitors.values().map(|monitor| monitor.rect).collect();
    let mut clips = Vec::new();
    apply_monitor_overflow_policy(
        placements,
        owner_id,
        focused_column,
        mode,
        monitors,
        &monitor_rects,
        &mut clips,
    );
    clips
}

#[test]
fn clip_mode_preserves_partial_peek_and_emits_owner_bounds() {
    let monitors = HashMap::from([(1, monitor(1, 0)), (2, monitor(2, 1920))]);
    let original = Rect::new(1800, 40, 600, 800);
    let mut placements = vec![placement(1, original.x, original.width, 0)];

    let clips = apply(
        &mut placements,
        1,
        Some(1),
        MonitorOverflowModeConfig::Clip,
        &monitors,
    );

    assert_eq!(placements[0].visibility, Visibility::Visible);
    assert_eq!(placements[0].rect, original);
    assert_eq!(clips.len(), 1);
    assert_eq!(clips[0].window_id, 1);
    assert_eq!(clips[0].bounds, monitors[&1].work_area);
    assert_eq!(clips[0].fallback_visibility, Visibility::OffScreenRight);
    assert!(monitors
        .values()
        .all(|monitor| !clips[0].fallback_rect.intersects(&monitor.rect)));
}

#[test]
fn clip_mode_focused_fallback_remains_visible_and_contained() {
    let monitors = HashMap::from([(1, monitor(1, 0)), (2, monitor(2, 1920))]);
    let mut placements = vec![placement(2, 1800, 600, 3)];

    let clips = apply(
        &mut placements,
        1,
        Some(3),
        MonitorOverflowModeConfig::Clip,
        &monitors,
    );

    assert_eq!(placements[0].rect, Rect::new(1800, 40, 600, 800));
    assert_eq!(clips.len(), 1);
    assert_eq!(clips[0].fallback_visibility, Visibility::Visible);
    assert_eq!(clips[0].fallback_rect, Rect::new(1320, 40, 600, 800));
}

#[test]
fn hide_mode_preserves_the_conservative_fallback() {
    let monitors = HashMap::from([(1, monitor(1, 0)), (2, monitor(2, 1920))]);
    let mut nonfocused = vec![placement(3, 1800, 600, 0)];
    let clips = apply(
        &mut nonfocused,
        1,
        Some(1),
        MonitorOverflowModeConfig::Hide,
        &monitors,
    );
    assert!(clips.is_empty());
    assert_eq!(nonfocused[0].visibility, Visibility::OffScreenRight);
    assert!(monitors
        .values()
        .all(|monitor| !nonfocused[0].rect.intersects(&monitor.rect)));

    let mut focused = vec![placement(4, 1800, 600, 1)];
    let clips = apply(
        &mut focused,
        1,
        Some(1),
        MonitorOverflowModeConfig::Hide,
        &monitors,
    );
    assert!(clips.is_empty());
    assert_eq!(focused[0].visibility, Visibility::Visible);
    assert_eq!(focused[0].rect, Rect::new(1320, 40, 600, 800));
}

#[test]
fn oversized_focused_fallback_fits_the_work_area() {
    let monitors = HashMap::from([(1, monitor(1, 0)), (2, monitor(2, 1920))]);
    let mut placements = vec![placement(5, -300, 2500, 0)];

    let clips = apply(
        &mut placements,
        1,
        Some(0),
        MonitorOverflowModeConfig::Clip,
        &monitors,
    );

    assert_eq!(clips.len(), 1);
    assert_eq!(clips[0].fallback_rect, Rect::new(0, 40, 1920, 800));
    assert_eq!(clips[0].fallback_visibility, Visibility::Visible);
}

#[test]
fn floating_and_mirrored_windows_are_not_region_managed() {
    let mirrored = HashMap::from([(1, monitor(1, 0)), (2, monitor(2, 0))]);
    let mut inside = vec![placement(6, 100, 800, 0)];
    assert!(apply(
        &mut inside,
        1,
        Some(0),
        MonitorOverflowModeConfig::Clip,
        &mirrored,
    )
    .is_empty());
    assert_eq!(inside[0].rect, Rect::new(100, 40, 800, 800));

    let adjacent = HashMap::from([(1, monitor(1, 0)), (2, monitor(2, 1920))]);
    let mut floating = vec![placement(7, 1800, 600, usize::MAX)];
    assert!(apply(
        &mut floating,
        1,
        None,
        MonitorOverflowModeConfig::Clip,
        &adjacent,
    )
    .is_empty());
    assert_eq!(floating[0].rect, Rect::new(1800, 40, 600, 800));
}

#[test]
fn offscreen_placements_are_always_parked_even_in_clip_mode() {
    let monitors = HashMap::from([(1, monitor(1, 0)), (2, monitor(2, 1920))]);
    let original = Rect::new(2100, 40, 600, 800);
    let mut placements = vec![WindowPlacement {
        window_id: 8,
        rect: original,
        visibility: Visibility::OffScreenRight,
        column_index: 2,
    }];

    let clips = apply(
        &mut placements,
        1,
        None,
        MonitorOverflowModeConfig::Clip,
        &monitors,
    );

    assert!(clips.is_empty());
    assert_ne!(placements[0].rect, original);
    assert!(monitors
        .values()
        .all(|monitor| !placements[0].rect.intersects(&monitor.rect)));
}

#[test]
fn middle_monitor_emits_independent_left_and_right_clips() {
    let monitors = HashMap::from([
        (1, monitor(1, 0)),
        (2, monitor(2, 1920)),
        (3, monitor(3, 3840)),
    ]);
    let mut placements = vec![placement(9, 1800, 600, 0), placement(10, 3700, 600, 2)];

    let clips = apply(
        &mut placements,
        2,
        Some(1),
        MonitorOverflowModeConfig::Clip,
        &monitors,
    );

    assert_eq!(clips.len(), 2);
    assert!(placements
        .iter()
        .all(|placement| placement.visibility == Visibility::Visible));
    assert!(clips
        .iter()
        .all(|clip| clip.bounds == monitors[&2].work_area));
}
