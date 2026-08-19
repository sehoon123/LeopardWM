use super::{park_offscreen_avoiding_neighbors, prepare_monitor_overflow};
use crate::config::MonitorOverflowModeConfig;
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

fn isolate(placements: &mut [WindowPlacement], owner_id: MonitorId, focused_column: Option<usize>) {
    let monitors = side_by_side_monitors();
    let rects: Vec<_> = monitors.values().map(|monitor| monitor.rect).collect();
    park_offscreen_avoiding_neighbors(placements, owner_id, focused_column, &monitors, &rects);
}

#[test]
fn non_focused_tiled_overflow_is_hidden_clear_of_every_monitor() {
    for (owner_id, rect) in [
        (1, Rect::new(1800, 40, 400, 800)),
        (2, Rect::new(1800, 40, 400, 800)),
    ] {
        let mut placements = vec![WindowPlacement {
            window_id: owner_id as u64,
            rect,
            visibility: Visibility::Visible,
            column_index: 0,
        }];

        isolate(&mut placements, owner_id, None);

        assert_ne!(placements[0].visibility, Visibility::Visible);
        assert!(side_by_side_monitors()
            .values()
            .all(|monitor| !placements[0].rect.intersects(&monitor.rect)));
    }
}

#[test]
fn focused_tiled_overflow_is_contained_instead_of_disappearing() {
    let mut placements = vec![WindowPlacement {
        window_id: 3,
        rect: Rect::new(1800, 40, 400, 800),
        visibility: Visibility::Visible,
        column_index: 2,
    }];

    isolate(&mut placements, 1, Some(2));

    assert_eq!(placements[0].visibility, Visibility::Visible);
    assert_eq!(placements[0].rect, Rect::new(1520, 40, 400, 800));
    assert!(!placements[0]
        .rect
        .intersects(&side_by_side_monitors()[&2].rect));
}

#[test]
fn oversized_focused_column_is_fitted_to_the_owner_work_area() {
    let mut placements = vec![WindowPlacement {
        window_id: 4,
        rect: Rect::new(-300, 40, 2500, 800),
        visibility: Visibility::Visible,
        column_index: 0,
    }];

    isolate(&mut placements, 1, Some(0));

    assert_eq!(placements[0].visibility, Visibility::Visible);
    assert_eq!(placements[0].rect, Rect::new(0, 40, 1920, 800));
    assert!(!placements[0]
        .rect
        .intersects(&side_by_side_monitors()[&2].rect));
}

#[test]
fn focused_containment_respects_an_offset_work_area() {
    let mut owner = monitor(1, 0);
    owner.work_area = Rect::new(120, 0, 1800, 1080);
    let right = monitor(2, 1920);
    let monitors = HashMap::from([(1, owner), (2, right)]);
    let rects: Vec<_> = monitors.values().map(|monitor| monitor.rect).collect();
    let mut placements = vec![WindowPlacement {
        window_id: 5,
        rect: Rect::new(1700, 40, 400, 800),
        visibility: Visibility::Visible,
        column_index: 1,
    }];

    park_offscreen_avoiding_neighbors(&mut placements, 1, Some(1), &monitors, &rects);

    assert_eq!(placements[0].visibility, Visibility::Visible);
    assert_eq!(placements[0].rect, Rect::new(1520, 40, 400, 800));
}

#[test]
fn floating_windows_may_span_monitors_intentionally() {
    let original = Rect::new(1800, 40, 400, 800);
    let mut placements = vec![WindowPlacement {
        window_id: 6,
        rect: original,
        visibility: Visibility::Visible,
        column_index: usize::MAX,
    }];

    isolate(&mut placements, 1, Some(0));

    assert_eq!(placements[0].visibility, Visibility::Visible);
    assert_eq!(placements[0].rect, original);
}

#[test]
fn mirrored_coordinates_do_not_hide_a_window_inside_its_owner() {
    let owner = monitor(1, 0);
    let mirror = monitor(2, 0);
    let monitors = HashMap::from([(1, owner), (2, mirror)]);
    let rects: Vec<_> = monitors.values().map(|monitor| monitor.rect).collect();
    let original = Rect::new(100, 40, 800, 800);
    let mut placements = vec![WindowPlacement {
        window_id: 7,
        rect: original,
        visibility: Visibility::Visible,
        column_index: 0,
    }];

    park_offscreen_avoiding_neighbors(&mut placements, 1, Some(0), &monitors, &rects);

    assert_eq!(placements[0].visibility, Visibility::Visible);
    assert_eq!(placements[0].rect, original);
}

#[test]
fn middle_monitor_is_isolated_from_both_horizontal_neighbors() {
    let monitors = HashMap::from([
        (1, monitor(1, 0)),
        (2, monitor(2, 1920)),
        (3, monitor(3, 3840)),
    ]);
    let rects: Vec<_> = monitors.values().map(|monitor| monitor.rect).collect();
    let mut placements = vec![
        WindowPlacement {
            window_id: 8,
            rect: Rect::new(1800, 40, 400, 800),
            visibility: Visibility::Visible,
            column_index: 0,
        },
        WindowPlacement {
            window_id: 9,
            rect: Rect::new(3700, 40, 400, 800),
            visibility: Visibility::Visible,
            column_index: 2,
        },
    ];

    park_offscreen_avoiding_neighbors(&mut placements, 2, Some(1), &monitors, &rects);

    assert!(placements
        .iter()
        .all(|placement| placement.visibility != Visibility::Visible));
    assert!(placements.iter().all(|placement| monitors
        .values()
        .all(|monitor| !placement.rect.intersects(&monitor.rect))));
}

#[test]
fn horizontal_isolation_matrix_never_leaks_tiled_windows() {
    let owner = Rect::new(0, 0, 1920, 1080);
    let neighbor = Rect::new(1920, 0, 1920, 1080);

    for width in [200, 600, 1920, 2500] {
        for x in (-800..=2600).step_by(137) {
            let original = Rect::new(x, 40, width, 800);
            let crosses = original.x < owner.x || original.right() > owner.right();
            let leaks = original.intersects(&neighbor);

            for focused in [false, true] {
                let mut placements = vec![WindowPlacement {
                    window_id: 10,
                    rect: original,
                    visibility: Visibility::Visible,
                    column_index: 0,
                }];
                isolate(&mut placements, 1, focused.then_some(0));
                let placement = &placements[0];

                if leaks && crosses && focused {
                    assert_eq!(placement.visibility, Visibility::Visible);
                    assert!(placement.rect.x >= owner.x);
                    assert!(placement.rect.right() <= owner.right());
                    assert!(!placement.rect.intersects(&neighbor));
                } else if leaks && crosses {
                    assert_ne!(placement.visibility, Visibility::Visible);
                    assert!(side_by_side_monitors()
                        .values()
                        .all(|monitor| !placement.rect.intersects(&monitor.rect)));
                } else {
                    assert_eq!(placement.visibility, Visibility::Visible);
                    assert_eq!(placement.rect, original);
                }
            }
        }
    }
}

fn visible_width(rect: Rect, viewport: Rect) -> i32 {
    rect.right().min(viewport.right()) - rect.x.max(viewport.x)
}

fn assert_clip_preview_distribution(column_width: i32, expected_preview: i32) {
    // The owner is the rightmost physical monitor. The left preview intersects
    // monitor 1 and needs a region; the right preview extends only into empty
    // virtual-desktop space and must remain visible without a region.
    let monitors = side_by_side_monitors();
    let owner = monitors[&2].rect;
    let monitor_rects: Vec<_> = monitors.values().map(|monitor| monitor.rect).collect();
    let focused_x = owner.x + expected_preview;
    let mut placements = vec![
        WindowPlacement {
            window_id: 20,
            rect: Rect::new(focused_x - column_width, 0, column_width, 800),
            visibility: Visibility::Visible,
            column_index: 0,
        },
        WindowPlacement {
            window_id: 21,
            rect: Rect::new(focused_x, 0, column_width, 800),
            visibility: Visibility::Visible,
            column_index: 1,
        },
        WindowPlacement {
            window_id: 22,
            rect: Rect::new(focused_x + column_width, 0, column_width, 800),
            visibility: Visibility::Visible,
            column_index: 2,
        },
    ];
    let original: Vec<_> = placements
        .iter()
        .map(|placement| {
            (
                placement.window_id,
                placement.rect,
                placement.visibility,
                placement.column_index,
            )
        })
        .collect();
    let mut clips = Vec::new();

    prepare_monitor_overflow(
        &mut placements,
        2,
        Some(1),
        MonitorOverflowModeConfig::Clip,
        &monitors,
        &monitor_rects,
        &mut clips,
    );

    let actual: Vec<_> = placements
        .iter()
        .map(|placement| {
            (
                placement.window_id,
                placement.rect,
                placement.visibility,
                placement.column_index,
            )
        })
        .collect();
    assert_eq!(actual, original);
    assert!(placements
        .iter()
        .all(|placement| placement.visibility == Visibility::Visible));
    assert_eq!(
        placements
            .iter()
            .map(|placement| visible_width(placement.rect, owner))
            .collect::<Vec<_>>(),
        vec![expected_preview, column_width, expected_preview]
    );
    assert_eq!(clips.len(), 1);
    assert_eq!(clips[0].window_id, 20);
    assert_eq!(clips[0].clip_bounds, owner);
}

#[test]
fn clip_mode_preserves_25_50_25_on_a_rightmost_monitor() {
    assert_clip_preview_distribution(960, 480);
}

#[test]
fn clip_mode_preserves_12_5_75_12_5_on_a_rightmost_monitor() {
    assert_clip_preview_distribution(1440, 240);
}
