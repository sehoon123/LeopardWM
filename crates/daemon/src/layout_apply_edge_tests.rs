use super::{
    bottommost_visible_tiled_hwnd, drifted_off_monitor_window, park_offscreen_avoiding_neighbors,
    prepare_monitor_overflow, preview_clip_bounds, suppress_persistent_previews_during_animation,
    OverflowContext,
};
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

#[test]
fn animation_frames_park_interactive_previews_but_keep_ghost_crops() {
    let fallback = Rect::new(-100_000, -100_000, 800, 600);
    let mut placements = vec![
        WindowPlacement {
            window_id: 10,
            rect: Rect::new(1800, 0, 800, 600),
            visibility: Visibility::Visible,
            column_index: 0,
        },
        WindowPlacement {
            window_id: 20,
            rect: Rect::new(1800, 0, 800, 600),
            visibility: Visibility::Visible,
            column_index: 1,
        },
    ];
    let mut clips = vec![
        leopardwm_platform_win32::WindowRegionClip {
            window_id: 10,
            clip_bounds: Rect::new(1800, 0, 120, 600),
            fallback_rect: fallback,
            fallback_visibility: Visibility::OffScreenRight,
        },
        leopardwm_platform_win32::WindowRegionClip {
            window_id: 20,
            clip_bounds: Rect::new(1800, 0, 120, 600),
            fallback_rect: fallback,
            fallback_visibility: Visibility::OffScreenRight,
        },
    ];
    let ghosts = std::collections::HashSet::from([20]);

    suppress_persistent_previews_during_animation(&mut placements, &mut clips, Some(&ghosts));

    assert_eq!(placements[0].rect, fallback);
    assert_eq!(placements[0].visibility, Visibility::OffScreenRight);
    assert_eq!(placements[1].rect, Rect::new(1800, 0, 800, 600));
    assert_eq!(
        clips.iter().map(|clip| clip.window_id).collect::<Vec<_>>(),
        vec![20]
    );
}

#[test]
fn band_anchor_is_the_bottommost_visible_tiled_window() {
    let info = |hwnd, x| leopardwm_platform_win32::WindowInfo {
        hwnd,
        title: String::new(),
        class_name: "Test".into(),
        process_id: hwnd as u32,
        rect: Rect::new(x, 0, 100, 100),
        visible: true,
    };
    // EnumWindows order is top-to-bottom: an unmanaged window sits between the
    // two tiled HWNDs, so anchoring to the topmost tiled window would leave it
    // below the preview host.
    let windows = vec![info(10, 10), info(20, 20), info(30, 30)];
    let tiled = std::collections::HashSet::from([10, 30]);

    assert_eq!(bottommost_visible_tiled_hwnd(&windows, &tiled), Some(30));
}

#[test]
fn band_anchor_is_absent_without_a_visible_tiled_window() {
    let info = |hwnd| leopardwm_platform_win32::WindowInfo {
        hwnd,
        title: String::new(),
        class_name: "Test".into(),
        process_id: hwnd as u32,
        rect: Rect::new(0, 0, 100, 100),
        visible: true,
    };
    let windows = vec![info(1), info(2)];

    assert_eq!(
        bottommost_visible_tiled_hwnd(&windows, &std::collections::HashSet::new()),
        None
    );
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
        &OverflowContext {
            monitors: &monitors,
            monitor_rects: &monitor_rects,
            preview_host_below: Some(1),
        },
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

fn visible_tiled(window_id: u64, rect: Rect) -> WindowPlacement {
    WindowPlacement {
        window_id,
        rect,
        visibility: Visibility::Visible,
        column_index: 0,
    }
}

#[test]
fn drift_detection_flags_a_window_relocated_onto_another_monitor() {
    let monitors = side_by_side_monitors();
    let placements = vec![visible_tiled(40, Rect::new(2000, 40, 700, 800))];

    // Owned by monitor 2 but physically sitting on monitor 1.
    let drifted = drifted_off_monitor_window(
        &placements,
        &monitors,
        |_| Some(2),
        |_| Some(Rect::new(300, 40, 700, 800)),
    );
    assert_eq!(drifted, Some(40));
}

#[test]
fn drift_detection_tolerates_invisible_borders_and_missing_geometry() {
    let monitors = side_by_side_monitors();
    let owner = monitors[&2].rect;
    let placements = vec![visible_tiled(41, Rect::new(owner.x + 10, 40, 700, 800))];

    // The authoritative outer rect includes the invisible resize border, so a
    // window flush against the shared edge overhangs it by a few pixels.
    let outer = Rect::new(owner.x - 9, 40, 718, 818);
    assert!(outer.intersects(&monitors[&1].rect));
    assert_eq!(
        drifted_off_monitor_window(&placements, &monitors, |_| Some(2), |_| Some(outer)),
        None
    );

    // A dead or unreadable HWND must never force placement.
    assert_eq!(
        drifted_off_monitor_window(&placements, &monitors, |_| Some(2), |_| None),
        None
    );
    // Neither must a window whose owning monitor is unknown.
    assert_eq!(
        drifted_off_monitor_window(
            &placements,
            &monitors,
            |_| None,
            |_| Some(Rect::new(300, 40, 700, 800))
        ),
        None
    );
}

#[test]
fn drift_detection_ignores_mirrored_outputs_floating_and_parked_windows() {
    let mirrored = HashMap::from([(1, monitor(1, 0)), (2, monitor(2, 0))]);
    let inside = Rect::new(100, 40, 700, 800);
    assert_eq!(
        drifted_off_monitor_window(
            &[visible_tiled(42, inside)],
            &mirrored,
            |_| Some(2),
            |_| Some(inside)
        ),
        None
    );
    // Mirrored outputs share a rectangle, so a window hanging far into empty
    // virtual-desktop space still overlaps the mirror through its contained
    // part. Only pixels that reach another output *and* sit outside the owner
    // count, so this must not disable the unchanged-layout fast path.
    let overhanging = Rect::new(1500, 40, 900, 800);
    assert!(overhanging.intersects(&mirrored[&1].rect));
    assert_eq!(
        drifted_off_monitor_window(
            &[visible_tiled(45, overhanging)],
            &mirrored,
            |_| Some(2),
            |_| Some(overhanging)
        ),
        None
    );

    let monitors = side_by_side_monitors();
    let elsewhere = Rect::new(300, 40, 700, 800);
    let floating = WindowPlacement {
        window_id: 43,
        rect: Rect::new(2000, 40, 700, 800),
        visibility: Visibility::Visible,
        column_index: usize::MAX,
    };
    let parked = WindowPlacement {
        window_id: 44,
        rect: Rect::new(2000, 40, 700, 800),
        visibility: Visibility::OffScreenLeft,
        column_index: 0,
    };
    assert_eq!(
        drifted_off_monitor_window(
            &[floating, parked],
            &monitors,
            |_| Some(2),
            |_| Some(elsewhere)
        ),
        None
    );
}

/// Run the clip policy for owner monitor 2 of a side-by-side pair.
fn clip_overflow(
    placements: &mut [WindowPlacement],
    focused_column: Option<usize>,
) -> Vec<leopardwm_platform_win32::WindowRegionClip> {
    let monitors = side_by_side_monitors();
    let monitor_rects: Vec<_> = monitors.values().map(|monitor| monitor.rect).collect();
    let mut clips = Vec::new();
    prepare_monitor_overflow(
        placements,
        2,
        focused_column,
        MonitorOverflowModeConfig::Clip,
        &OverflowContext {
            monitors: &monitors,
            monitor_rects: &monitor_rects,
            preview_host_below: Some(1),
        },
        &mut clips,
    );
    clips
}

#[test]
fn clip_mode_contains_a_focused_column_that_left_its_owner_entirely() {
    let monitors = side_by_side_monitors();
    let owner = monitors[&2].rect;
    let mut placements = vec![WindowPlacement {
        window_id: 30,
        // Fully on monitor 1 while it still belongs to monitor 2's workspace.
        rect: Rect::new(600, 40, 800, 800),
        visibility: Visibility::Visible,
        column_index: 1,
    }];

    let clips = clip_overflow(&mut placements, Some(1));

    // A region could only blank it, so the placement itself must be corrected.
    assert!(clips.is_empty());
    assert_eq!(placements[0].visibility, Visibility::Visible);
    assert!(placements[0].rect.x >= owner.x);
    assert!(placements[0].rect.right() <= owner.right());
    assert!(!placements[0].rect.intersects(&monitors[&1].rect));
}

#[test]
fn clip_mode_parks_an_unfocused_column_that_left_its_owner_entirely() {
    let monitors = side_by_side_monitors();
    let mut placements = vec![WindowPlacement {
        window_id: 31,
        rect: Rect::new(600, 40, 800, 800),
        visibility: Visibility::Visible,
        column_index: 0,
    }];

    let clips = clip_overflow(&mut placements, Some(1));

    assert!(clips.is_empty());
    assert_ne!(placements[0].visibility, Visibility::Visible);
    assert!(monitors
        .values()
        .all(|monitor| !placements[0].rect.intersects(&monitor.rect)));
}

#[test]
fn clip_mode_never_plans_a_region_that_cannot_show_pixels() {
    let monitors = side_by_side_monitors();
    let owner = monitors[&2].rect;

    for width in [200, 900, 1920, 2400] {
        for x in (-600..=4200).step_by(101) {
            for focused in [false, true] {
                let original = Rect::new(x, 40, width, 800);
                let mut placements = vec![WindowPlacement {
                    window_id: 32,
                    rect: original,
                    visibility: Visibility::Visible,
                    column_index: 0,
                }];

                let clips = clip_overflow(&mut placements, focused.then_some(0));
                let placement = &placements[0];

                let inside_owner = |rect: Rect| {
                    rect.x >= owner.x
                        && rect.right() <= owner.right()
                        && rect.y >= owner.y
                        && rect.bottom() <= owner.bottom()
                };

                if let Some(clip) = clips.first() {
                    // Every planned clip keeps a non-empty area on its owner,
                    // leaves the layout geometry untouched, and carries a
                    // fallback that is itself monitor-safe.
                    assert_eq!(clip.clip_bounds, owner);
                    assert_eq!(placement.rect, original);
                    assert_eq!(placement.visibility, Visibility::Visible);
                    assert!(placement.rect.intersects(&owner));
                    if clip.fallback_visibility == Visibility::Visible {
                        assert!(inside_owner(clip.fallback_rect));
                    } else {
                        assert!(monitors
                            .values()
                            .all(|monitor| !clip.fallback_rect.intersects(&monitor.rect)));
                    }
                } else if placement.visibility == Visibility::Visible {
                    // No clip and still visible: it must be inside its owner, or
                    // hang only into empty virtual-desktop space.
                    assert!(
                        inside_owner(placement.rect)
                            || !placement.rect.intersects(&monitors[&1].rect),
                        "visible placement {:?} leaked onto the neighbor without a clip",
                        placement.rect
                    );
                } else {
                    assert!(monitors
                        .values()
                        .all(|monitor| !placement.rect.intersects(&monitor.rect)));
                }
            }
        }
    }
}

#[test]
fn clip_mode_publishes_the_whole_strip_under_a_covering_float() {
    let monitors = side_by_side_monitors();
    let owner = monitors[&2].rect;
    // A left-edge preview strip on monitor 2, covered end to end by a floating
    // window. The preview host is anchored below the tiled band, so the float
    // keeps its pixels and its input while the full strip stays published
    // behind it. Narrowing the clip here is what truncated real previews
    // whenever a launcher or dialog sat over the edge strip.
    let mut placements = vec![
        visible_tiled(50, Rect::new(owner.x - 600, 40, 800, 800)),
        WindowPlacement {
            window_id: 51,
            rect: Rect::new(owner.x - 10, owner.y, 400, owner.height),
            visibility: Visibility::Visible,
            column_index: usize::MAX,
        },
    ];

    let clips = clip_overflow(&mut placements, Some(1));

    assert_eq!(clips.len(), 1, "a covering float must not cut the preview");
    assert_eq!(clips[0].window_id, 50);
    assert_eq!(clips[0].clip_bounds, owner);
    assert_eq!(placements[0].visibility, Visibility::Visible);
    // The float itself is untouched.
    assert_eq!(
        placements[1].rect,
        Rect::new(owner.x - 10, owner.y, 400, owner.height)
    );
    assert_eq!(placements[1].visibility, Visibility::Visible);
}

#[test]
fn an_unproven_band_anchor_publishes_no_preview() {
    let monitors = side_by_side_monitors();
    let owner = monitors[&2].rect;
    let monitor_rects: Vec<_> = monitors.values().map(|monitor| monitor.rect).collect();
    let mut placements = vec![visible_tiled(50, Rect::new(owner.x - 600, 40, 800, 800))];
    let mut clips = Vec::new();

    prepare_monitor_overflow(
        &mut placements,
        2,
        Some(1),
        MonitorOverflowModeConfig::Clip,
        &OverflowContext {
            monitors: &monitors,
            monitor_rects: &monitor_rects,
            preview_host_below: None,
        },
        &mut clips,
    );

    assert!(
        clips.is_empty(),
        "without a proven band anchor the host would paint over unknown owners"
    );
    assert_ne!(placements[0].visibility, Visibility::Visible);
    assert!(monitors
        .values()
        .all(|monitor| !placements[0].rect.intersects(&monitor.rect)));
}

#[test]
fn clip_mode_keeps_a_preview_a_float_does_not_cover() {
    let monitors = side_by_side_monitors();
    let owner = monitors[&2].rect;
    // Same geometry, but the float sits well clear of the 200px strip.
    let mut placements = vec![
        visible_tiled(52, Rect::new(owner.x - 600, 40, 800, 800)),
        WindowPlacement {
            window_id: 53,
            rect: Rect::new(owner.x + 900, 100, 400, 400),
            visibility: Visibility::Visible,
            column_index: usize::MAX,
        },
    ];

    let clips = clip_overflow(&mut placements, Some(1));

    assert_eq!(clips.len(), 1);
    assert_eq!(clips[0].window_id, 52);
    assert_eq!(placements[0].visibility, Visibility::Visible);
}

#[test]
fn clip_mode_preserves_25_50_25_on_a_rightmost_monitor() {
    assert_clip_preview_distribution(960, 480);
}

#[test]
fn clip_mode_preserves_12_5_75_12_5_on_a_rightmost_monitor() {
    assert_clip_preview_distribution(1440, 240);
}

/// The clip-bounds policy on its own: an unobstructed strip must be exactly the
/// owner rectangle, so nothing about ordinary previews changes, and a float may
/// only take the pixels it actually covers.
mod preview_clip_bounds_policy {
    use super::*;

    /// A 1000x800 monitor starting at x=1000.
    const OWNER: Rect = Rect {
        x: 1000,
        y: 0,
        width: 1000,
        height: 800,
    };

    /// A column hanging 600px off the left edge, leaving a 200px strip.
    const CROSSING: Rect = Rect {
        x: 400,
        y: 40,
        width: 800,
        height: 700,
    };

    #[test]
    fn an_on_owner_strip_publishes_the_whole_owner_rect() {
        assert_eq!(
            preview_clip_bounds(CROSSING, OWNER),
            Some(OWNER),
            "the strip is never reduced; windows above the band anchor cover it instead"
        );
    }

    #[test]
    fn a_one_pixel_strip_still_publishes() {
        let sliver = Rect::new(OWNER.x - 799, OWNER.y, 800, 700);
        assert_eq!(preview_clip_bounds(sliver, OWNER), Some(OWNER));
    }

    #[test]
    fn a_placement_off_the_owner_publishes_nothing() {
        let far_away = Rect::new(0, 0, 300, 300);
        assert_eq!(preview_clip_bounds(far_away, OWNER), None);
    }
}
