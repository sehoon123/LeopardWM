use super::park_offscreen_avoiding_neighbors;
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
