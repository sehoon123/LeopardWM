use super::park_offscreen_avoiding_neighbors;
use crate::config::MonitorOverflowConfig;
use leopardwm_core_layout::{Rect, Visibility, WindowPlacement};
use leopardwm_platform_win32::{MonitorId, MonitorInfo, WindowRegionClipSpec};
use std::collections::HashMap;

fn monitor(id: MonitorId, x: i32, y: i32) -> MonitorInfo {
    let rect = Rect::new(x, y, 1920, 1080);
    MonitorInfo {
        id,
        rect,
        work_area: Rect::new(x, y, 1920, 1040),
        is_primary: id == 1,
        device_name: format!("DISPLAY{id}"),
        scale_factor: 1.0,
    }
}

fn placement(window_id: u64, rect: Rect, column_index: usize) -> WindowPlacement {
    WindowPlacement {
        window_id,
        rect,
        visibility: Visibility::Visible,
        column_index,
    }
}

fn run(
    placements: &mut [WindowPlacement],
    owner_id: MonitorId,
    focused_column: Option<usize>,
    mode: MonitorOverflowConfig,
    monitors: &HashMap<MonitorId, MonitorInfo>,
) -> Vec<WindowRegionClipSpec> {
    let monitor_rects: Vec<_> = monitors.values().map(|monitor| monitor.rect).collect();
    let mut clips = Vec::new();
    park_offscreen_avoiding_neighbors(
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
fn clip_mode_preserves_partial_preview_and_emits_safe_fallback() {
    let monitors = HashMap::from([(1, monitor(1, 0, 0)), (2, monitor(2, 1920, 0))]);
    let original = Rect::new(1800, 40, 600, 800);
    let mut placements = vec![placement(10, original, 0)];

    let clips = run(
        &mut placements,
        1,
        None,
        MonitorOverflowConfig::Clip,
        &monitors,
    );

    assert_eq!(placements[0].rect, original);
    assert_eq!(placements[0].visibility, Visibility::Visible);
    assert_eq!(clips.len(), 1);
    assert_eq!(clips[0].clip_bounds, monitors[&1].rect);
    assert_ne!(clips[0].fallback_visibility, Visibility::Visible);
    assert!(monitors
        .values()
        .all(|monitor| !clips[0].fallback_rect.intersects(&monitor.rect)));
}

#[test]
fn focused_clip_fallback_remains_visible_inside_work_area() {
    let monitors = HashMap::from([(1, monitor(1, 0, 0)), (2, monitor(2, 1920, 0))]);
    let original = Rect::new(1800, 40, 600, 800);
    let mut placements = vec![placement(11, original, 3)];

    let clips = run(
        &mut placements,
        1,
        Some(3),
        MonitorOverflowConfig::Clip,
        &monitors,
    );

    assert_eq!(placements[0].rect, original);
    assert_eq!(clips.len(), 1);
    assert_eq!(clips[0].fallback_visibility, Visibility::Visible);
    assert_eq!(clips[0].fallback_rect, Rect::new(1320, 40, 600, 800));
}

#[test]
fn hide_mode_keeps_the_existing_whole_window_policy() {
    let monitors = HashMap::from([(1, monitor(1, 0, 0)), (2, monitor(2, 1920, 0))]);
    let mut placements = vec![placement(12, Rect::new(1800, 40, 600, 800), 0)];

    let clips = run(
        &mut placements,
        1,
        None,
        MonitorOverflowConfig::Hide,
        &monitors,
    );

    assert!(clips.is_empty());
    assert_ne!(placements[0].visibility, Visibility::Visible);
    assert!(monitors
        .values()
        .all(|monitor| !placements[0].rect.intersects(&monitor.rect)));
}

#[test]
fn vertical_crossing_uses_fallback_instead_of_horizontal_region_clipping() {
    let monitors = HashMap::from([(1, monitor(1, 0, 0)), (2, monitor(2, 0, 1080))]);
    let mut placements = vec![placement(13, Rect::new(100, 900, 800, 400), 0)];

    let clips = run(
        &mut placements,
        1,
        None,
        MonitorOverflowConfig::Clip,
        &monitors,
    );

    assert!(clips.is_empty());
    assert_ne!(placements[0].visibility, Visibility::Visible);
}

#[test]
fn floating_windows_are_never_region_managed() {
    let monitors = HashMap::from([(1, monitor(1, 0, 0)), (2, monitor(2, 1920, 0))]);
    let original = Rect::new(1800, 40, 600, 800);
    let mut placements = vec![placement(14, original, usize::MAX)];

    let clips = run(
        &mut placements,
        1,
        None,
        MonitorOverflowConfig::Clip,
        &monitors,
    );

    assert!(clips.is_empty());
    assert_eq!(placements[0].rect, original);
    assert_eq!(placements[0].visibility, Visibility::Visible);
}

#[test]
fn duplicate_window_requests_are_upserted_not_accumulated() {
    let monitors = HashMap::from([
        (1, monitor(1, 0, 0)),
        (2, monitor(2, 1920, 0)),
        (3, monitor(3, -1920, 0)),
    ]);
    let mut placements = vec![
        placement(15, Rect::new(1800, 40, 600, 800), 0),
        placement(15, Rect::new(-200, 40, 600, 800), 0),
    ];

    let clips = run(
        &mut placements,
        1,
        None,
        MonitorOverflowConfig::Clip,
        &monitors,
    );

    assert_eq!(clips.len(), 1);
    assert_eq!(clips[0].window_id, 15);
}
