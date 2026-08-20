from pathlib import Path

ROOT = Path('.')


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding='utf-8')


def write(path: str, text: str) -> None:
    (ROOT / path).write_text(text, encoding='utf-8', newline='\n')


def replace_once(path: str, old: str, new: str) -> None:
    text = read(path)
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f'{path}: expected one occurrence, found {count}: {old[:100]!r}')
    write(path, text.replace(old, new, 1))


def function_span(text: str, signature: str) -> tuple[int, int]:
    start = text.find(signature)
    if start < 0:
        raise RuntimeError(f'function not found: {signature}')
    brace = text.find('{', start)
    if brace < 0:
        raise RuntimeError(f'opening brace not found: {signature}')
    depth = 0
    in_string = False
    escaped = False
    for index in range(brace, len(text)):
        char = text[index]
        if in_string:
            if escaped:
                escaped = False
            elif char == '\\':
                escaped = True
            elif char == '"':
                in_string = False
            continue
        if char == '"':
            in_string = True
        elif char == '{':
            depth += 1
        elif char == '}':
            depth -= 1
            if depth == 0:
                return start, index + 1
    raise RuntimeError(f'closing brace not found: {signature}')


def replace_function(path: str, signature: str, replacement: str) -> None:
    text = read(path)
    start, end = function_span(text, signature)
    write(path, text[:start] + replacement + text[end:])


REGION = 'crates/platform_win32/src/window_region.rs'
PLACEMENT = 'crates/platform_win32/src/placement.rs'

# Win32 geometry queries used to derive a bridge that is safe at both endpoints.
text = read(REGION)
text = text.replace(
    'use windows::Win32::Foundation::{HANDLE, HWND};',
    'use windows::Win32::Foundation::{HANDLE, HWND, RECT};',
)
text = text.replace(
    '''use windows::Win32::Graphics::Gdi::{
    CreateRectRgn, DeleteObject, EqualRgn, GetWindowRgn, SetWindowRgn, HGDIOBJ,
};''',
    '''use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::Graphics::Gdi::{
    CreateRectRgn, DeleteObject, EqualRgn, GetWindowRgn, SetWindowRgn, HGDIOBJ,
};''',
)
text = text.replace(
    '''    GetClassNameW, GetPropW, GetWindowThreadProcessId, IsWindow, RemovePropW, SetPropW,
};''',
    '''    GetClassNameW, GetPropW, GetWindowRect, GetWindowThreadProcessId, IsWindow,
    RemovePropW, SetPropW,
};''',
)
write(REGION, text)

replace_function(
    REGION,
    'fn write_metadata(hwnd: HWND, rect: Rect) -> bool',
    '''fn write_metadata(hwnd: HWND, rect: Rect) -> bool {
    // Publish coordinates first and the owner marker last. Another process can
    // observe either a complete record or no record, never a partial rectangle.
    let payload = unsafe {
        [
            SetPropW(
                hwnd,
                w!("LeopardWM.RegionClip.v2.Left"),
                Some(encode_coordinate(rect.x)),
            ),
            SetPropW(
                hwnd,
                w!("LeopardWM.RegionClip.v2.Top"),
                Some(encode_coordinate(rect.y)),
            ),
            SetPropW(
                hwnd,
                w!("LeopardWM.RegionClip.v2.Right"),
                Some(encode_coordinate(rect.right())),
            ),
            SetPropW(
                hwnd,
                w!("LeopardWM.RegionClip.v2.Bottom"),
                Some(encode_coordinate(rect.bottom())),
            ),
        ]
    };
    if payload.into_iter().any(|result| result.is_err()) {
        remove_metadata(hwnd);
        return false;
    }
    if unsafe {
        SetPropW(
            hwnd,
            w!("LeopardWM.RegionClip.v2.Owner"),
            Some(handle_from_usize(OWNER_MAGIC)),
        )
    }
    .is_err()
    {
        remove_metadata(hwnd);
        return false;
    }
    true
}''',
)

# An empty region is a valid temporary bridge for an opposite-edge jump.
# Both `read_metadata` and `relative_clip_region` intentionally accept
# zero-area rectangles: the former recovers an empty bridge after a
# crash, while the latter represents a safe fully-clipped transition.
text = read(REGION)
old_guard = '''    if right <= left || bottom <= top {
        return None;
    }'''
new_guard = '''    if right < left || bottom < top {
        return None;
    }'''
if text.count(old_guard) != 2:
    raise RuntimeError(
        f'{REGION}: expected two empty-region guards, found {text.count(old_guard)}'
    )
write(REGION, text.replace(old_guard, new_guard))

replace_function(
    REGION,
    'fn actual_region_matches(hwnd: HWND, expected: Rect) -> bool',
    '''fn actual_region_matches(hwnd: HWND, expected: Rect) -> bool {
    let Some(actual) = create_region(Rect::new(0, 0, 1, 1)) else {
        return false;
    };
    let raw = unsafe { GetWindowRgn(hwnd, actual) }.0;
    let Some(kind) = classify_window_region_kind(raw) else {
        delete_region(actual);
        return false;
    };
    if kind == WindowRegionKind::NoRegion {
        delete_region(actual);
        return false;
    }
    let Some(expected_region) = create_region(expected) else {
        delete_region(actual);
        return false;
    };
    let equal = unsafe { EqualRgn(actual, expected_region).as_bool() };
    delete_region(actual);
    delete_region(expected_region);
    equal
}''',
)

marker = '''fn window_has_no_region(hwnd: HWND) -> bool {
    matches!(current_region_kind(hwnd), Some(WindowRegionKind::NoRegion))
}
'''
helpers = '''

fn rect_from_win32(rect: RECT) -> Option<Rect> {
    let width = rect.right.saturating_sub(rect.left);
    let height = rect.bottom.saturating_sub(rect.top);
    (width > 0 && height > 0).then(|| Rect::new(rect.left, rect.top, width, height))
}

fn current_window_geometry(hwnd: HWND) -> Option<(Rect, Rect)> {
    let mut outer = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut outer) }.ok()?;
    let outer = rect_from_win32(outer)?;

    let mut visible = RECT::default();
    let visible = if unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut visible as *mut _ as _,
            std::mem::size_of::<RECT>() as u32,
        )
    }
    .is_ok()
    {
        rect_from_win32(visible).unwrap_or(outer)
    } else {
        outer
    };
    Some((outer, visible))
}

fn intersect_regions(left: Rect, right: Rect) -> Rect {
    let x = left.x.max(right.x);
    let y = left.y.max(right.y);
    let right_edge = left.right().min(right.right());
    let bottom_edge = left.bottom().min(right.bottom());
    Rect::new(
        x,
        y,
        right_edge.saturating_sub(x).max(0),
        bottom_edge.saturating_sub(y).max(0),
    )
}

fn allowed_region(outer_rect: Rect, visible_rect: Rect, clip_bounds: Rect) -> Rect {
    relative_clip_region(outer_rect, visible_rect, clip_bounds)
        .unwrap_or_else(|| Rect::new(0, 0, 0, 0))
}

/// Local shape that is safe at both the old and target HWND positions. Since a
/// monitor rectangle is convex, the same bridge is also safe during any DWM
/// interpolation between those endpoints.
pub(crate) fn bridge_clip_region(
    current_outer: Rect,
    current_visible: Rect,
    target_outer: Rect,
    target_visible: Rect,
    clip_bounds: Rect,
) -> Rect {
    intersect_regions(
        allowed_region(current_outer, current_visible, clip_bounds),
        allowed_region(target_outer, target_visible, clip_bounds),
    )
}

fn install_owned_region_locked(
    window_id: WindowId,
    hwnd: HWND,
    identity: WindowIdentity,
    expected_region: Rect,
    redraw: bool,
) -> RegionClipResult {
    let Some(region) = create_region(expected_region) else {
        return RegionClipResult::Failed;
    };
    if unsafe { SetWindowRgn(hwnd, Some(region), redraw) } == 0 {
        delete_region(region);
        return RegionClipResult::Failed;
    }
    // Windows owns HRGN after a successful SetWindowRgn call.
    if !write_metadata(hwnd, expected_region) {
        // In-process state remains authoritative, allowing normal cleanup even
        // when HWND property storage was temporarily unavailable.
        remove_metadata(hwnd);
    }
    lock_states().insert(
        window_id,
        RegionState {
            identity,
            expected_region,
        },
    );
    RegionClipResult::Applied
}

fn owned_region_for_identity(
    window_id: WindowId,
    hwnd: HWND,
    identity: &WindowIdentity,
) -> Result<Option<Rect>, RegionClipResult> {
    if let Some(state) = lock_states().get(&window_id).cloned() {
        if state.identity == *identity && actual_region_matches(hwnd, state.expected_region) {
            return Ok(Some(state.expected_region));
        }
        lock_states().remove(&window_id);
        if state.identity == *identity && has_owner_marker(hwnd) {
            // Application takeover: discard only our marker, never its shape.
            remove_metadata(hwnd);
            return Err(RegionClipResult::Unsupported);
        }
    }

    if has_owner_marker(hwnd) {
        if let Some(expected) = read_metadata(hwnd) {
            if actual_region_matches(hwnd, expected) {
                return Ok(Some(expected));
            }
        }
        remove_metadata(hwnd);
        return Err(RegionClipResult::Unsupported);
    }

    match current_region_kind(hwnd) {
        Some(WindowRegionKind::NoRegion) => Ok(None),
        Some(_) => Err(RegionClipResult::Unsupported),
        None => Err(RegionClipResult::Failed),
    }
}

/// Install a restrictive bridge before the HWND is uncloaked or moved.
pub(crate) fn prepare_window_region_clip(
    window_id: WindowId,
    target_outer: Rect,
    target_visible: Rect,
    clip_bounds: Rect,
) -> RegionClipResult {
    let target_region = allowed_region(target_outer, target_visible, clip_bounds);
    let _commit = lock_commit();
    let Some(current_identity) = identity(window_id) else {
        lock_states().remove(&window_id);
        return RegionClipResult::Failed;
    };
    let Ok(hwnd) = window_id_to_hwnd(window_id) else {
        return RegionClipResult::Failed;
    };

    let current_owned = match owned_region_for_identity(window_id, hwnd, &current_identity) {
        Ok(region) => region,
        Err(result) => return result,
    };
    let current_region = if let Some(region) = current_owned {
        region
    } else {
        let Some((current_outer, current_visible)) = current_window_geometry(hwnd) else {
            return RegionClipResult::Failed;
        };
        allowed_region(current_outer, current_visible, clip_bounds)
    };
    let bridge = intersect_regions(current_region, target_region);
    if current_owned == Some(bridge) && actual_region_matches(hwnd, bridge) {
        return RegionClipResult::Unchanged;
    }
    install_owned_region_locked(
        window_id,
        hwnd,
        current_identity,
        bridge,
        false,
    )
}

pub(crate) fn has_owned_window_region(window_id: WindowId) -> bool {
    if lock_states().contains_key(&window_id) {
        return true;
    }
    window_id_to_hwnd(window_id)
        .ok()
        .is_some_and(has_owner_marker)
}
'''
text = read(REGION)
if text.count(marker) != 1:
    raise RuntimeError('window_region.rs: no-region marker mismatch')
write(REGION, text.replace(marker, marker + helpers, 1))

replace_function(
    REGION,
    'pub(crate) fn apply_window_region_clip(',
    '''pub(crate) fn apply_window_region_clip(
    window_id: WindowId,
    outer_rect: Rect,
    visible_rect: Rect,
    clip_bounds: Rect,
    redraw: bool,
) -> RegionClipResult {
    let target_region = allowed_region(outer_rect, visible_rect, clip_bounds);
    let _commit = lock_commit();
    let Some(current_identity) = identity(window_id) else {
        lock_states().remove(&window_id);
        return RegionClipResult::Failed;
    };
    let Ok(hwnd) = window_id_to_hwnd(window_id) else {
        return RegionClipResult::Failed;
    };

    let current_owned = match owned_region_for_identity(window_id, hwnd, &current_identity) {
        Ok(region) => region,
        Err(result) => return result,
    };
    if current_owned == Some(target_region) && actual_region_matches(hwnd, target_region) {
        return RegionClipResult::Unchanged;
    }

    // Replace the bridge directly. Clearing first creates an unbounded
    // rectangular DWM frame between the two SetWindowRgn calls.
    install_owned_region_locked(
        window_id,
        hwnd,
        current_identity,
        target_region,
        redraw,
    )
}''',
)

# Extend the existing Windows tests with transition-safety matrices and real HWND ordering.
text = read(REGION)
text = text.replace(
    '''        apply_window_region_clip, classify_window_region_kind, decode_coordinate,
        encode_coordinate, relative_clip_region, restore_window_region, window_has_no_region,
        WindowRegionKind, ERROR_REGION_KIND,
''',
    '''        actual_region_matches, apply_window_region_clip, bridge_clip_region,
        classify_window_region_kind, decode_coordinate, encode_coordinate,
        prepare_window_region_clip, relative_clip_region, restore_window_region,
        window_has_no_region, WindowRegionKind, ERROR_REGION_KIND,
''',
    1,
)
text = text.replace(
    '''        CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassExW, WINDOW_EX_STYLE,
        WNDCLASSEXW, WS_OVERLAPPED,
''',
    '''        CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassExW, SetWindowPos,
        WINDOW_EX_STYLE, WNDCLASSEXW, SWP_NOACTIVATE, SWP_NOZORDER, WS_OVERLAPPED,
''',
    1,
)
extra_tests = '''

    fn screen_region(outer: Rect, local: Rect) -> Rect {
        Rect::new(
            outer.x.saturating_add(local.x),
            outer.y.saturating_add(local.y),
            local.width,
            local.height,
        )
    }

    fn position_test_window(hwnd: HWND, rect: Rect) {
        unsafe {
            SetWindowPos(
                hwnd,
                None,
                rect.x,
                rect.y,
                rect.width,
                rect.height,
                SWP_NOZORDER | SWP_NOACTIVATE,
            )
            .unwrap();
        }
    }

    #[test]
    fn bridge_is_safe_at_endpoints_and_intermediate_positions() {
        let owner = Rect::new(1000, 0, 1000, 800);
        for width in [250, 500, 750, 1000, 1250] {
            for old_x in (250..=2250).step_by(125) {
                for new_x in (250..=2250).step_by(125) {
                    let old = Rect::new(old_x, 0, width, 800);
                    let new = Rect::new(new_x, 0, width, 800);
                    let bridge = bridge_clip_region(old, old, new, new, owner);
                    for step in 0..=8 {
                        let x = old_x + (new_x - old_x) * step / 8;
                        let translated = screen_region(Rect::new(x, 0, width, 800), bridge);
                        if bridge.width > 0 && bridge.height > 0 {
                            assert!(translated.x >= owner.x);
                            assert!(translated.right() <= owner.right());
                            assert!(translated.y >= owner.y);
                            assert!(translated.bottom() <= owner.bottom());
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn seventy_five_percent_preview_masks_the_negative_monitor_half() {
        let owner = Rect::new(1000, 0, 1000, 800);
        let old = Rect::new(1000, 0, 750, 800);
        let target = Rect::new(500, 0, 750, 800);
        let bridge = bridge_clip_region(old, old, target, target, owner);
        assert_eq!(bridge, Rect::new(500, 0, 250, 800));
        let target_screen = screen_region(target, bridge);
        assert_eq!(target_screen, Rect::new(1000, 0, 250, 800));
        assert!(!target_screen.intersects(&Rect::new(0, 0, 1000, 800)));
    }

    #[test]
    fn opposite_edge_jump_uses_an_empty_safe_bridge() {
        let owner = Rect::new(1000, 0, 1000, 800);
        let left = Rect::new(500, 0, 750, 800);
        let right = Rect::new(1750, 0, 750, 800);
        assert_eq!(bridge_clip_region(left, left, right, right, owner).width, 0);
    }

    #[test]
    fn outward_move_restricts_before_positioning() {
        let window = TestWindow::new();
        let id = window_id(window.0);
        let owner = Rect::new(0, 0, 1000, 800);
        let current = Rect::new(-250, 0, 750, 800);
        let target = Rect::new(-500, 0, 750, 800);
        position_test_window(window.0, current);
        assert!(apply_window_region_clip(id, current, current, owner, false).succeeded());
        assert!(actual_region_matches(window.0, Rect::new(250, 0, 500, 800)));

        assert!(prepare_window_region_clip(id, target, target, owner).succeeded());
        assert!(actual_region_matches(window.0, Rect::new(500, 0, 250, 800)));
    }

    #[test]
    fn inward_move_expands_only_after_positioning() {
        let window = TestWindow::new();
        let id = window_id(window.0);
        let owner = Rect::new(0, 0, 1000, 800);
        let current = Rect::new(-500, 0, 750, 800);
        let target = Rect::new(-250, 0, 750, 800);
        position_test_window(window.0, current);
        assert!(apply_window_region_clip(id, current, current, owner, false).succeeded());
        assert!(actual_region_matches(window.0, Rect::new(500, 0, 250, 800)));

        assert!(prepare_window_region_clip(id, target, target, owner).succeeded());
        assert!(actual_region_matches(window.0, Rect::new(500, 0, 250, 800)));

        position_test_window(window.0, target);
        assert!(apply_window_region_clip(id, target, target, owner, false).succeeded());
        assert!(actual_region_matches(window.0, Rect::new(250, 0, 500, 800)));
    }
'''
last = text.rfind('}')
if last < 0:
    raise RuntimeError('window_region.rs: final test brace not found')
write(REGION, text[:last] + extra_tests + text[last:])

# Placement order: bridge -> uncloak -> synchronous boundary move -> final region -> restore.
text = read(PLACEMENT)
text = text.replace(
    '''    apply_window_region_clip, reconcile_window_regions, restore_all_window_regions,
    restore_window_region, WindowRegionClip,
''',
    '''    apply_window_region_clip, has_owned_window_region, prepare_window_region_clip,
    reconcile_window_regions, restore_all_window_regions, restore_window_region, WindowRegionClip,
''',
    1,
)
old_reconcile = '''    let clipped_window_ids: HashSet<WindowId> =
        region_clips.iter().map(|clip| clip.window_id).collect();
    reconcile_window_regions(&managed_window_ids, &clipped_window_ids, !animation_frame);
'''
if text.count(old_reconcile) != 1:
    raise RuntimeError('placement.rs: early reconcile block mismatch')
text = text.replace(old_reconcile, '', 1)

old_order = '''    // Uncloak windows that are becoming visible BEFORE positioning,
    // so DWM starts compositing them at the correct location on this frame.
    // Also remove from the tracking set — the post-positioning block will
    // re-add if the window ends up off-screen on this frame.
    //
    // Routed through `apply_cloak_state` so a window that's also in
    // `GHOST_CLOAKED` (e.g. scrolling off-screen → on-screen with ghost
    // animation in flight) stays cloaked until the ghost path also
    // releases it.
    uncloak_becoming_visible(&entries);

    let (applied, mut failed_window_ids) = position_entries(&entries);
    let region_fallbacks =
        apply_entry_region_clips(&mut entries, &mut failed_window_ids, animation_frame);
'''
new_order = '''    // Restrict first, then reveal and move. This removes the frame in which
    // DWM could previously display a gray rectangular backing surface outside
    // the owner monitor before SetWindowRgn was committed.
    let mut failed_window_ids = HashSet::new();
    let pre_fallbacks =
        prepare_entry_region_clips(&mut entries, &mut failed_window_ids, animation_frame);

    uncloak_becoming_visible(&entries);

    let (applied, position_failures) = position_entries(&entries);
    failed_window_ids.extend(position_failures);
    let post_fallbacks =
        apply_entry_region_clips(&mut entries, &mut failed_window_ids, animation_frame);

    let mut active_clipped_window_ids: HashSet<WindowId> =
        region_clips.iter().map(|clip| clip.window_id).collect();
    for entry in &entries {
        if entry.region_clip_bounds.is_none() {
            active_clipped_window_ids.remove(&entry.window_id);
        }
    }
    // Regions on windows becoming fully contained keep their old restrictive region until
    // the move completes; only then is the region removed.
    reconcile_window_regions(
        &managed_window_ids,
        &active_clipped_window_ids,
        !animation_frame,
    );
    let region_fallbacks = pre_fallbacks + post_fallbacks;
'''
if text.count(old_order) != 1:
    raise RuntimeError('placement.rs: placement ordering block mismatch')
text = text.replace(old_order, new_order, 1)

old_dispatch = '''        let dispatch = if animation_frame {
            let sensitive = policy == AnimationPlacementPolicy::AdaptiveCompositorSafe
                && cached_compositor_sensitive(hwnd, placement.window_id, cache.as_deref_mut());
            let hung = sensitive && unsafe { IsHungAppWindow(hwnd).as_bool() };
            animation_dispatch_mode(policy, sensitive, hung)
        } else {
            AnimationDispatchMode::Synchronous
        };
'''
new_dispatch = '''        let region_managed =
            region_clip.is_some() || has_owned_window_region(placement.window_id);
        let dispatch = if animation_frame {
            let sensitive = region_managed
                || (policy == AnimationPlacementPolicy::AdaptiveCompositorSafe
                    && cached_compositor_sensitive(
                        hwnd,
                        placement.window_id,
                        cache.as_deref_mut(),
                    ));
            let hung = sensitive && unsafe { IsHungAppWindow(hwnd).as_bool() };
            if region_managed && !hung {
                // Keep SetWindowRgn and SetWindowPos ordered only for boundary
                // HWNDs. The normal in-monitor animation path remains async.
                AnimationDispatchMode::Synchronous
            } else {
                animation_dispatch_mode(policy, sensitive, hung)
            }
        } else {
            AnimationDispatchMode::Synchronous
        };
'''
if text.count(old_dispatch) != 1:
    raise RuntimeError('placement.rs: animation dispatch block mismatch')
text = text.replace(old_dispatch, new_dispatch, 1)
write(PLACEMENT, text)

text = read(PLACEMENT)
start, end = function_span(
    text,
    'fn set_entry_to_fallback(entry: &mut DeferEntry, animation_frame: bool) -> bool',
)
replacement = '''fn configure_entry_fallback(entry: &mut DeferEntry, animation_frame: bool) -> bool {
    let (Some(rect), Some(visibility)) = (entry.fallback_rect, entry.fallback_visibility) else {
        return false;
    };
    let (inset_l, inset_t, inset_r, inset_b) = entry.used_insets;
    entry.layout_rect = rect;
    entry.visibility = visibility;
    entry.region_clip_bounds = None;
    entry.x = rect.x.saturating_sub(inset_l);
    entry.y = rect.y.saturating_sub(inset_t);
    if visibility == Visibility::Visible {
        entry.w = rect
            .width
            .saturating_add(inset_l)
            .saturating_add(inset_r);
        entry.h = rect
            .height
            .saturating_add(inset_t)
            .saturating_add(inset_b);
        entry.flags = SWP_NOZORDER | SWP_NOACTIVATE;
        if !animation_frame {
            entry.flags |= SWP_FRAMECHANGED;
        }
    } else {
        entry.w = 0;
        entry.h = 0;
        entry.flags = SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE;
    }
    true
}

fn set_entry_to_fallback(entry: &mut DeferEntry, animation_frame: bool) -> bool {
    if !configure_entry_fallback(entry, animation_frame) {
        return false;
    }
    unsafe {
        SetWindowPos(
            entry.hwnd,
            None,
            entry.x,
            entry.y,
            entry.w,
            entry.h,
            entry.flags,
        )
        .is_ok()
    }
}'''
text = text[:start] + replacement + text[end:]
commit_marker = '''/// Commit requested regions after the HWND batch lands. A rare ownership or
'''
pre_commit = '''/// Install a bridge before uncloaking or moving. Unsupported application-owned
/// regions use the existing safe whole-window fallback before presentation.
fn prepare_entry_region_clips(
    entries: &mut [DeferEntry],
    failed_window_ids: &mut HashSet<u64>,
    animation_frame: bool,
) -> u32 {
    let mut fallback_count = 0;
    for entry in entries {
        let Some(clip_bounds) = entry.region_clip_bounds else {
            continue;
        };
        let target_outer = Rect::new(entry.x, entry.y, entry.w.max(1), entry.h.max(1));
        let result = prepare_window_region_clip(
            entry.window_id,
            target_outer,
            entry.layout_rect,
            clip_bounds,
        );
        if result.succeeded() {
            continue;
        }

        let _ = restore_window_region(entry.window_id, false);
        fallback_count += 1;
        if !configure_entry_fallback(entry, animation_frame) {
            failed_window_ids.insert(entry.window_id);
        }
    }
    fallback_count
}

'''
if text.count(commit_marker) != 1:
    raise RuntimeError('placement.rs: post-commit marker mismatch')
write(PLACEMENT, text.replace(commit_marker, pre_commit + commit_marker, 1))

print('atomic region transition patch applied')
