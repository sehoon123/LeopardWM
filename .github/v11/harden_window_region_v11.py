from pathlib import Path

ROOT = Path.cwd()


def replace_once(path: Path, old: str, new: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: expected one replacement target, found {count}")
    path.write_text(text.replace(old, new), encoding="utf-8", newline="\n")


def function_span(text: str, name: str) -> tuple[int, int]:
    candidates = (f"fn {name}(", f"pub fn {name}(", f"pub(crate) fn {name}(")
    start = next((text.find(candidate) for candidate in candidates if text.find(candidate) >= 0), -1)
    if start < 0:
        raise RuntimeError(f"function {name} not found")
    brace = text.find("{", start)
    depth = 0
    for index in range(brace, len(text)):
        if text[index] == "{":
            depth += 1
        elif text[index] == "}":
            depth -= 1
            if depth == 0:
                return start, index + 1
    raise RuntimeError(f"unbalanced function {name}")


def replace_function(path: Path, name: str, replacement: str) -> None:
    text = path.read_text(encoding="utf-8")
    start, end = function_span(text, name)
    path.write_text(text[:start] + replacement + text[end:], encoding="utf-8", newline="\n")


def insert_after_function_open(path: Path, name: str, statement: str) -> None:
    text = path.read_text(encoding="utf-8")
    start, _ = function_span(text, name)
    brace = text.find("{", start)
    if statement.strip() in text[brace + 1 : brace + 600]:
        return
    path.write_text(
        text[: brace + 1] + "\n" + statement + text[brace + 1 :],
        encoding="utf-8",
        newline="\n",
    )


# ---------------------------------------------------------------------------
# Region ownership: retain state on clear failure, compare complete fallback
# specs, and safely recover when an application removes/replaces our region.
# ---------------------------------------------------------------------------
region = ROOT / "crates/platform_win32/src/window_region.rs"
replace_once(
    region,
    "use crate::{recover_poisoned_mutex, window_id_to_hwnd};",
    "use crate::{recover_poisoned_mutex, window_id_to_hwnd, WindowRegionClip};",
)
replace_once(
    region,
    """enum RegionState {
    Owned {
        expected: LocalClipRect,
        spec_bounds: Rect,
        process_id: u32,
    },
    Unsupported {
        spec_bounds: Rect,
        process_id: u32,
    },
}""",
    """enum RegionState {
    Owned {
        expected: LocalClipRect,
        spec: WindowRegionClip,
        process_id: u32,
    },
    Unsupported {
        spec: WindowRegionClip,
        process_id: u32,
    },
}""",
)
replace_function(
    region,
    "apply_managed_region",
    r'''pub(crate) fn apply_managed_region(
    window_id: WindowId,
    outer: Rect,
    spec: WindowRegionClip,
    redraw: bool,
) -> RegionApplyOutcome {
    let Ok(hwnd) = window_id_to_hwnd(window_id) else {
        return RegionApplyOutcome::Retry;
    };
    if !unsafe { IsWindow(Some(hwnd)).as_bool() } {
        forget_managed_window_region(window_id);
        return RegionApplyOutcome::Retry;
    }
    let Some(pid) = process_id(hwnd) else {
        return RegionApplyOutcome::Retry;
    };
    let Some(desired) = local_clip_rect(outer, spec.bounds) else {
        return RegionApplyOutcome::Retry;
    };
    if desired.is_full_window(outer) {
        return clear_managed_region(window_id, redraw)
            .then_some(RegionApplyOutcome::Applied)
            .unwrap_or(RegionApplyOutcome::Retry);
    }

    let mut guard = lock_regions();
    let states = guard.get_or_insert_with(HashMap::new);
    if states.get(&window_id).is_some_and(|state| match *state {
        RegionState::Owned { process_id, .. } | RegionState::Unsupported { process_id, .. } => {
            process_id != pid
        }
    }) {
        states.remove(&window_id);
    }

    loop {
        match states.get(&window_id).copied() {
            Some(RegionState::Unsupported {
                spec: current,
                process_id,
            }) if process_id == pid && current == spec => {
                return RegionApplyOutcome::Unsupported;
            }
            Some(RegionState::Unsupported { .. }) => {
                let Some(probe) = OwnedRegion::empty() else {
                    return RegionApplyOutcome::Retry;
                };
                match region_kind(hwnd, probe.handle()) {
                    REGION_ERROR => return RegionApplyOutcome::Retry,
                    NULL_REGION => {
                        states.remove(&window_id);
                        continue;
                    }
                    _ => {
                        states.insert(
                            window_id,
                            RegionState::Unsupported {
                                spec,
                                process_id: pid,
                            },
                        );
                        return RegionApplyOutcome::Unsupported;
                    }
                }
            }
            Some(RegionState::Owned {
                expected,
                process_id,
                ..
            }) => {
                if !valid_identity(hwnd, process_id) {
                    states.remove(&window_id);
                    return RegionApplyOutcome::Retry;
                }
                if !current_region_matches(hwnd, expected) {
                    let Some(probe) = OwnedRegion::empty() else {
                        return RegionApplyOutcome::Retry;
                    };
                    match region_kind(hwnd, probe.handle()) {
                        REGION_ERROR => return RegionApplyOutcome::Retry,
                        NULL_REGION => {
                            states.remove(&window_id);
                            continue;
                        }
                        _ => {
                            states.insert(
                                window_id,
                                RegionState::Unsupported {
                                    spec,
                                    process_id: pid,
                                },
                            );
                            return RegionApplyOutcome::Unsupported;
                        }
                    }
                }
                if expected == desired {
                    states.insert(
                        window_id,
                        RegionState::Owned {
                            expected,
                            spec,
                            process_id: pid,
                        },
                    );
                    return RegionApplyOutcome::Applied;
                }
                let Some(region) = OwnedRegion::rectangle(desired) else {
                    return RegionApplyOutcome::Retry;
                };
                if install_region(hwnd, region, redraw) {
                    states.insert(
                        window_id,
                        RegionState::Owned {
                            expected: desired,
                            spec,
                            process_id: pid,
                        },
                    );
                    return RegionApplyOutcome::Applied;
                }
                return RegionApplyOutcome::Retry;
            }
            None => {
                let Some(probe) = OwnedRegion::empty() else {
                    return RegionApplyOutcome::Retry;
                };
                match region_kind(hwnd, probe.handle()) {
                    REGION_ERROR => return RegionApplyOutcome::Retry,
                    NULL_REGION => {}
                    _ => {
                        states.insert(
                            window_id,
                            RegionState::Unsupported {
                                spec,
                                process_id: pid,
                            },
                        );
                        return RegionApplyOutcome::Unsupported;
                    }
                }
                let Some(region) = OwnedRegion::rectangle(desired) else {
                    return RegionApplyOutcome::Retry;
                };
                if install_region(hwnd, region, redraw) {
                    states.insert(
                        window_id,
                        RegionState::Owned {
                            expected: desired,
                            spec,
                            process_id: pid,
                        },
                    );
                    return RegionApplyOutcome::Applied;
                }
                return RegionApplyOutcome::Retry;
            }
        }
    }
}''',
)
replace_function(
    region,
    "clear_managed_region",
    r'''pub(crate) fn clear_managed_region(window_id: WindowId, redraw: bool) -> bool {
    let Ok(hwnd) = window_id_to_hwnd(window_id) else {
        forget_managed_window_region(window_id);
        return false;
    };
    let mut guard = lock_regions();
    let Some(states) = guard.as_mut() else {
        return true;
    };
    match states.get(&window_id).copied() {
        None => true,
        Some(RegionState::Unsupported { .. }) => {
            states.remove(&window_id);
            true
        }
        Some(RegionState::Owned {
            expected,
            process_id,
            ..
        }) => {
            if !valid_identity(hwnd, process_id) || !current_region_matches(hwnd, expected) {
                states.remove(&window_id);
                return true;
            }
            if clear_region(hwnd, redraw) {
                states.remove(&window_id);
                true
            } else {
                // Retain ownership so the next layout/recovery pass retries.
                false
            }
        }
    }
}''',
)
replace_function(
    region,
    "managed_regions_match",
    r'''pub fn managed_regions_match(
    active_ids: impl IntoIterator<Item = WindowId>,
    desired: &[WindowRegionClip],
) -> bool {
    let guard = lock_regions();
    let Some(states) = guard.as_ref() else {
        return desired.is_empty();
    };
    if states.is_empty() {
        return desired.is_empty();
    }

    let active: HashSet<_> = active_ids.into_iter().collect();
    if states.keys().any(|window_id| !active.contains(window_id)) {
        return false;
    }
    if desired.iter().any(|clip| {
        !states.get(&clip.window_id).is_some_and(|state| match *state {
            RegionState::Owned { spec, .. } | RegionState::Unsupported { spec, .. } => spec == *clip,
        })
    }) {
        return false;
    }
    states
        .keys()
        .all(|window_id| desired.iter().any(|clip| clip.window_id == *window_id))
}''',
)

# Append real HWND integration tests. They verify ownership transfer, custom
# region preservation, replacement detection, and full-spec cache matching.
text = region.read_text(encoding="utf-8")
if "mod win32_integration_tests" not in text:
    text += r'''

#[cfg(test)]
mod win32_integration_tests {
    use super::*;
    use windows::core::w;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, GetWindowRgn, SetWindowRgn, WINDOW_EX_STYLE,
        WS_OVERLAPPED,
    };

    struct TestWindow(HWND);

    impl TestWindow {
        fn new() -> Self {
            let hwnd = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("STATIC"),
                    w!("LeopardWM region test"),
                    WS_OVERLAPPED,
                    100,
                    100,
                    200,
                    100,
                    None,
                    None,
                    None,
                    None,
                )
                .expect("create hidden test window")
            };
            Self(hwnd)
        }

        fn id(&self) -> WindowId {
            self.0.0 as usize as u64
        }
    }

    impl Drop for TestWindow {
        fn drop(&mut self) {
            forget_managed_window_region(self.id());
            unsafe {
                let _ = SetWindowRgn(self.0, None, false);
                let _ = DestroyWindow(self.0);
            }
        }
    }

    fn clip(window_id: WindowId, fallback_x: i32) -> WindowRegionClip {
        WindowRegionClip {
            window_id,
            bounds: Rect::new(150, 100, 100, 100),
            fallback_rect: Rect::new(fallback_x, -100_000, 200, 100),
            fallback_visibility: leopardwm_core_layout::Visibility::OffScreenRight,
        }
    }

    fn current_equals(hwnd: HWND, expected: LocalClipRect) -> bool {
        let current = OwnedRegion::empty().expect("current region");
        if region_kind(hwnd, current.handle()) <= NULL_REGION {
            return false;
        }
        let expected = OwnedRegion::rectangle(expected).expect("expected region");
        unsafe { EqualRgn(current.handle(), expected.handle()).as_bool() }
    }

    #[test]
    fn owned_region_is_installed_and_cleared() {
        let window = TestWindow::new();
        let spec = clip(window.id(), -100_000);
        assert_eq!(
            apply_managed_region(window.id(), Rect::new(100, 100, 200, 100), spec, false),
            RegionApplyOutcome::Applied
        );
        assert!(current_equals(
            window.0,
            LocalClipRect {
                left: 50,
                top: 0,
                right: 150,
                bottom: 100,
            }
        ));
        assert!(managed_regions_match([window.id()], &[spec]));
        assert!(!managed_regions_match(
            [window.id()],
            &[clip(window.id(), -90_000)]
        ));
        assert!(clear_managed_region(window.id(), false));
        let probe = OwnedRegion::empty().expect("probe region");
        assert_eq!(region_kind(window.0, probe.handle()), NULL_REGION);
    }

    #[test]
    fn application_region_is_never_overwritten() {
        let window = TestWindow::new();
        let custom = LocalClipRect {
            left: 5,
            top: 6,
            right: 120,
            bottom: 80,
        };
        let region = OwnedRegion::rectangle(custom).expect("custom region");
        assert!(install_region(window.0, region, false));

        let spec = clip(window.id(), -100_000);
        assert_eq!(
            apply_managed_region(window.id(), Rect::new(100, 100, 200, 100), spec, false),
            RegionApplyOutcome::Unsupported
        );
        assert!(current_equals(window.0, custom));
        assert!(managed_regions_match([window.id()], &[spec]));
        assert!(clear_managed_region(window.id(), false));
        assert!(current_equals(window.0, custom));
    }

    #[test]
    fn application_replacement_is_not_cleared_as_ours() {
        let window = TestWindow::new();
        let spec = clip(window.id(), -100_000);
        assert_eq!(
            apply_managed_region(window.id(), Rect::new(100, 100, 200, 100), spec, false),
            RegionApplyOutcome::Applied
        );

        let custom = LocalClipRect {
            left: 10,
            top: 10,
            right: 180,
            bottom: 90,
        };
        let region = OwnedRegion::rectangle(custom).expect("replacement region");
        assert!(install_region(window.0, region, false));

        assert!(clear_managed_region(window.id(), false));
        assert!(current_equals(window.0, custom));
    }
}
'''
region.write_text(text, encoding="utf-8", newline="\n")

# ---------------------------------------------------------------------------
# Placement fallback: preserve dispatch flags, recompute visible fallback size,
# retry unsupported/cache-mismatched clips, and avoid pre-move redraw flashes.
# ---------------------------------------------------------------------------
placement = ROOT / "crates/platform_win32/src/placement.rs"
replace_function(
    placement,
    "apply_region_fallback",
    r'''fn apply_region_fallback(entry: &mut DeferEntry, clip: WindowRegionClip) {
    entry.visibility = clip.fallback_visibility;
    entry.x = clip
        .fallback_rect
        .x
        .saturating_sub(entry.used_insets.0);
    entry.y = clip
        .fallback_rect
        .y
        .saturating_sub(entry.used_insets.1);
    entry.layout_rect = clip.fallback_rect;
    entry.flags = SET_WINDOW_POS_FLAGS(entry.flags.0 & !SWP_NOSIZE.0);

    if clip.fallback_visibility == Visibility::Visible {
        entry.w = clip
            .fallback_rect
            .width
            .saturating_add(entry.used_insets.0)
            .saturating_add(entry.used_insets.2)
            .max(1);
        entry.h = clip
            .fallback_rect
            .height
            .saturating_add(entry.used_insets.1)
            .saturating_add(entry.used_insets.3)
            .max(1);
    } else {
        entry.flags |= SWP_NOSIZE;
        entry.h = 0;
    }
}''',
)
replace_once(
    placement,
    "crate::window_region::clear_managed_region(entry.window_id, !animation_frame)",
    "crate::window_region::clear_managed_region(entry.window_id, false)",
)
replace_once(
    placement,
    """            entry.window_id,
            outer,
            clip.bounds,
            !animation_frame,
        )""",
    """            entry.window_id,
            outer,
            clip,
            false,
        )""",
)
replace_once(
    placement,
    """            crate::window_region::RegionApplyOutcome::Unsupported => {
                apply_region_fallback(entry, clip);
                result
                    .visibility_overrides
                    .insert(entry.window_id, clip.fallback_visibility);
            }""",
    """            crate::window_region::RegionApplyOutcome::Unsupported => {
                apply_region_fallback(entry, clip);
                result
                    .visibility_overrides
                    .insert(entry.window_id, clip.fallback_visibility);
                // Do not cache a fallback as if the requested clipped placement
                // landed. The manager's Unsupported state makes this retry cheap.
                result.retry_ids.insert(entry.window_id);
            }""",
)
replace_once(
    placement,
    """            .filter(|e| !failed_window_ids.contains(&e.window_id))
            .map(|e| e.window_id)""",
    """            .filter(|e| {
                !failed_window_ids.contains(&e.window_id)
                    && !region_preparation.retry_ids.contains(&e.window_id)
            })
            .map(|e| e.window_id)""",
)

text = placement.read_text(encoding="utf-8")
if "mod region_fallback_tests" not in text:
    text += r'''

#[cfg(test)]
mod region_fallback_tests {
    use super::*;

    fn entry(visibility: Visibility) -> DeferEntry {
        DeferEntry {
            hwnd: HWND::default(),
            window_id: 42,
            x: 0,
            y: 0,
            w: 900,
            h: 700,
            layout_rect: Rect::new(0, 0, 900, 700),
            used_insets: (2, 3, 5, 7),
            validate_insets: true,
            visibility,
            flags: SWP_NOZORDER | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS | SWP_NOSIZE,
            column_index: 0,
            region_clip: None,
        }
    }

    fn clip(visibility: Visibility, rect: Rect) -> WindowRegionClip {
        WindowRegionClip {
            window_id: 42,
            bounds: Rect::new(0, 0, 1920, 1080),
            fallback_rect: rect,
            fallback_visibility: visibility,
        }
    }

    #[test]
    fn visible_region_fallback_recomputes_full_outer_geometry() {
        let mut entry = entry(Visibility::Visible);
        apply_region_fallback(
            &mut entry,
            clip(Visibility::Visible, Rect::new(100, 50, 300, 200)),
        );

        assert_eq!((entry.x, entry.y), (98, 47));
        assert_eq!((entry.w, entry.h), (307, 210));
        assert_eq!(entry.layout_rect, Rect::new(100, 50, 300, 200));
        assert_eq!(entry.visibility, Visibility::Visible);
        assert!(entry.flags.contains(SWP_ASYNCWINDOWPOS));
        assert!(!entry.flags.contains(SWP_NOSIZE));
    }

    #[test]
    fn hidden_region_fallback_preserves_position_only_parking() {
        let mut entry = entry(Visibility::Visible);
        apply_region_fallback(
            &mut entry,
            clip(
                Visibility::OffScreenRight,
                Rect::new(-100_000, -100_000, 300, 200),
            ),
        );

        assert_eq!((entry.x, entry.y), (-100_002, -100_003));
        assert_eq!(entry.layout_rect, Rect::new(-100_000, -100_000, 300, 200));
        assert_eq!(entry.visibility, Visibility::OffScreenRight);
        assert!(entry.flags.contains(SWP_ASYNCWINDOWPOS));
        assert!(entry.flags.contains(SWP_NOSIZE));
        assert_eq!(entry.h, 0);
    }
}
'''
placement.write_text(text, encoding="utf-8", newline="\n")

# ---------------------------------------------------------------------------
# Daemon policy: deduplicate per-HWND clip requests and verify replacement.
# ---------------------------------------------------------------------------
layout = ROOT / "crates/daemon/src/layout_apply.rs"
text = layout.read_text(encoding="utf-8")
if "fn upsert_window_region_clip(" not in text:
    marker = "fn apply_monitor_overflow_policy("
    offset = text.find(marker)
    if offset < 0:
        raise RuntimeError("monitor-overflow policy not found")
    helper = '''fn upsert_window_region_clip(
    clips: &mut Vec<leopardwm_platform_win32::WindowRegionClip>,
    clip: leopardwm_platform_win32::WindowRegionClip,
) {
    if let Some(existing) = clips
        .iter_mut()
        .find(|existing| existing.window_id == clip.window_id)
    {
        *existing = clip;
    } else {
        clips.push(clip);
    }
}

'''
    text = text[:offset] + helper + text[offset:]
old_push = '''            region_clips.push(leopardwm_platform_win32::WindowRegionClip {
                window_id: placement.window_id,
                bounds: owner.work_area,
                fallback_rect,
                fallback_visibility,
            });'''
new_push = '''            upsert_window_region_clip(
                region_clips,
                leopardwm_platform_win32::WindowRegionClip {
                    window_id: placement.window_id,
                    bounds: owner.work_area,
                    fallback_rect,
                    fallback_visibility,
                },
            );'''
if old_push in text:
    text = text.replace(old_push, new_push)
elif new_push not in text:
    raise RuntimeError("region clip insertion point not found")
layout.write_text(text, encoding="utf-8", newline="\n")

layout_tests = ROOT / "crates/daemon/src/layout_region_tests.rs"
text = layout_tests.read_text(encoding="utf-8")
if "duplicate_clip_specs_are_replaced_in_place" not in text:
    text += r'''

#[test]
fn duplicate_clip_specs_are_replaced_in_place() {
    let monitors = HashMap::from([(1, monitor(1, 0)), (2, monitor(2, 1920))]);
    let monitor_rects: Vec<_> = monitors.values().map(|monitor| monitor.rect).collect();
    let mut clips = Vec::new();
    let mut first = vec![placement(11, 1800, 600, 0)];
    apply_monitor_overflow_policy(
        &mut first,
        1,
        None,
        MonitorOverflowModeConfig::Clip,
        &monitors,
        &monitor_rects,
        &mut clips,
    );
    assert_eq!(clips.len(), 1);

    let mut second = vec![placement(11, 1700, 700, 0)];
    apply_monitor_overflow_policy(
        &mut second,
        1,
        Some(0),
        MonitorOverflowModeConfig::Clip,
        &monitors,
        &monitor_rects,
        &mut clips,
    );

    assert_eq!(clips.len(), 1);
    assert_eq!(clips[0].window_id, 11);
    assert_eq!(clips[0].fallback_visibility, Visibility::Visible);
    assert_eq!(clips[0].fallback_rect, Rect::new(1220, 40, 700, 800));
}
'''
layout_tests.write_text(text, encoding="utf-8", newline="\n")

# ---------------------------------------------------------------------------
# Emergency and panic recovery must restore every LeopardWM-owned region.
# ---------------------------------------------------------------------------
visibility = ROOT / "crates/platform_win32/src/visibility.rs"
for name, statement, required in (
    (
        "restore_window_moved_offscreen",
        "    let _ = crate::window_region::clear_managed_region(window_id, true);\n",
        True,
    ),
    (
        "restore_all_windows_moved_offscreen_best_effort",
        "    crate::window_region::restore_all_managed_window_regions();\n",
        True,
    ),
    (
        "restore_windows_moved_offscreen",
        "    crate::window_region::restore_all_managed_window_regions();\n",
        True,
    ),
    (
        "uncloak_all_managed_windows",
        "    crate::window_region::restore_all_managed_window_regions();\n",
        True,
    ),
    (
        "uncloak_all_visible_windows",
        "    crate::window_region::restore_all_managed_window_regions();\n",
        False,
    ),
):
    try:
        insert_after_function_open(visibility, name, statement)
    except RuntimeError:
        if required:
            raise

print("window-region v11 hardening applied")
