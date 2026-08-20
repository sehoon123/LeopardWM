from pathlib import Path
import re

ROOT = Path('.')


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding='utf-8')


def write(path: str, text: str) -> None:
    (ROOT / path).write_text(text, encoding='utf-8', newline='\n')


def replace_once(path: str, old: str, new: str) -> None:
    text = read(path)
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f'{path}: expected one occurrence, found {count}: {old[:120]!r}')
    write(path, text.replace(old, new, 1))


def replace_regex(path: str, pattern: str, replacement: str, flags: int = re.S) -> None:
    text = read(path)
    updated, count = re.subn(pattern, replacement, text, count=1, flags=flags)
    if count != 1:
        raise RuntimeError(f'{path}: regex replacement count={count}: {pattern[:120]!r}')
    write(path, updated)


region_path = 'crates/platform_win32/src/window_region.rs'
placement_path = 'crates/platform_win32/src/placement.rs'

# ---------------------------------------------------------------------------
# Region ownership: preserve the live region while replacing it, track a
# deferred redraw from animation frames, and force one real landing redraw.
# ---------------------------------------------------------------------------
replace_once(
    region_path,
    '''struct RegionState {
    identity: WindowIdentity,
    expected_region: Rect,
}
''',
    '''struct RegionState {
    identity: WindowIdentity,
    expected_region: Rect,
    /// Intermediate animation frames install regions without repainting. The
    /// exact landing consumes this bit and performs one authoritative redraw.
    redraw_pending: bool,
}
''',
)

replace_regex(
    region_path,
    r'''fn write_metadata\(hwnd: HWND, rect: Rect\) -> bool \{.*?\n\}\n\nfn read_metadata''',
    '''fn write_metadata(hwnd: HWND, rect: Rect) -> bool {
    // Publish the owner marker last. A concurrent or restarted LeopardWM can
    // therefore observe either complete metadata or no ownership at all.
    let coordinates = unsafe {
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
    if !coordinates.into_iter().all(|result| result.is_ok()) {
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
}

fn read_metadata''',
)

replace_once(
    region_path,
    '''fn delete_region(region: windows::Win32::Graphics::Gdi::HRGN) {
    unsafe {
        let _ = DeleteObject(HGDIOBJ(region.0));
    }
}
''',
    '''fn delete_region(region: windows::Win32::Graphics::Gdi::HRGN) {
    unsafe {
        let _ = DeleteObject(HGDIOBJ(region.0));
    }
}

fn install_region(hwnd: HWND, rect: Rect, redraw: bool) -> bool {
    let Some(region) = create_region(rect) else {
        return false;
    };
    if unsafe { SetWindowRgn(hwnd, Some(region), redraw) } == 0 {
        delete_region(region);
        false
    } else {
        // SetWindowRgn transfers ownership to Windows on success.
        true
    }
}
''',
)

replace_regex(
    region_path,
    r'''pub\(crate\) fn apply_window_region_clip\(.*?\n\}\n\n/// Restore a window only when''',
    '''pub(crate) fn apply_window_region_clip(
    window_id: WindowId,
    outer_rect: Rect,
    visible_rect: Rect,
    clip_bounds: Rect,
    redraw: bool,
) -> RegionClipResult {
    let Some(expected_region) = relative_clip_region(outer_rect, visible_rect, clip_bounds) else {
        return RegionClipResult::Unsupported;
    };

    let _commit = lock_commit();
    let Some(current_identity) = identity(window_id) else {
        lock_states().remove(&window_id);
        return RegionClipResult::Failed;
    };
    let Ok(hwnd) = window_id_to_hwnd(window_id) else {
        return RegionClipResult::Failed;
    };

    if let Some(mut state) = lock_states().get(&window_id).cloned() {
        let same_identity = state.identity == current_identity;
        let still_owned = same_identity
            && has_owner_marker(hwnd)
            && actual_region_matches(hwnd, state.expected_region);

        if still_owned && state.expected_region == expected_region {
            if !redraw {
                if !state.redraw_pending {
                    state.redraw_pending = true;
                    lock_states().insert(window_id, state);
                }
                return RegionClipResult::Unchanged;
            }
            if state.redraw_pending {
                if !install_region(hwnd, expected_region, true) {
                    return RegionClipResult::Failed;
                }
                state.redraw_pending = false;
                lock_states().insert(window_id, state);
                return RegionClipResult::Applied;
            }
            return RegionClipResult::Unchanged;
        }

        if still_owned {
            // Replace our old HRGN directly. Clearing it first creates an
            // unclipped composition frame on an adjacent monitor.
            if !write_metadata(hwnd, expected_region) {
                return RegionClipResult::Failed;
            }
            if !install_region(hwnd, expected_region, redraw) {
                let _ = write_metadata(hwnd, state.expected_region);
                return RegionClipResult::Failed;
            }
            state.expected_region = expected_region;
            state.redraw_pending = !redraw;
            lock_states().insert(window_id, state);
            return RegionClipResult::Applied;
        }

        lock_states().remove(&window_id);
        if has_owner_marker(hwnd) {
            // The live shape no longer equals the one we installed. Relinquish
            // metadata without touching the application's replacement region.
            remove_metadata(hwnd);
        }
        if same_identity {
            return RegionClipResult::Unsupported;
        }
    }

    if !recover_stale_metadata(hwnd, false) {
        return RegionClipResult::Failed;
    }
    if !window_has_no_region(hwnd) {
        return RegionClipResult::Unsupported;
    }
    if !write_metadata(hwnd, expected_region) {
        return RegionClipResult::Failed;
    }
    if !install_region(hwnd, expected_region, redraw) {
        remove_metadata(hwnd);
        return RegionClipResult::Failed;
    }
    lock_states().insert(
        window_id,
        RegionState {
            identity: current_identity,
            expected_region,
            redraw_pending: !redraw,
        },
    );
    RegionClipResult::Applied
}

/// Restore a window only when''',
)

# Add focused tests without touching the existing test module imports.
region_tests = r'''

#[cfg(test)]
mod atomic_region_transaction_tests {
    use super::*;
    use std::sync::OnceLock;
    use windows::core::w;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassExW, WINDOW_EX_STYLE,
        WNDCLASSEXW, WS_OVERLAPPED,
    };

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    }

    fn test_window() -> HWND {
        static REGISTERED: OnceLock<()> = OnceLock::new();
        let instance = unsafe { GetModuleHandleW(None).unwrap() };
        REGISTERED.get_or_init(|| {
            let class = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(wndproc),
                hInstance: instance.into(),
                lpszClassName: w!("LeopardWMAtomicRegionTest"),
                ..Default::default()
            };
            assert_ne!(unsafe { RegisterClassExW(&class) }, 0);
        });
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("LeopardWMAtomicRegionTest"),
                w!(""),
                WS_OVERLAPPED,
                0,
                0,
                1000,
                800,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .unwrap()
        }
    }

    fn id(hwnd: HWND) -> WindowId {
        hwnd.0 as usize as u64
    }

    struct TestWindow(HWND);
    impl TestWindow {
        fn new() -> Self {
            Self(test_window())
        }
    }
    impl Drop for TestWindow {
        fn drop(&mut self) {
            let _ = restore_window_region(id(self.0), false);
            unsafe {
                let _ = DestroyWindow(self.0);
            }
        }
    }

    #[test]
    fn landing_consumes_the_deferred_region_redraw() {
        let window = TestWindow::new();
        let window_id = id(window.0);
        let outer = Rect::new(250, 0, 750, 800);
        let owner = Rect::new(1000, 0, 1000, 800);

        assert!(apply_window_region_clip(window_id, outer, outer, owner, false).succeeded());
        assert!(lock_states().get(&window_id).unwrap().redraw_pending);
        assert!(apply_window_region_clip(window_id, outer, outer, owner, true).succeeded());
        assert!(!lock_states().get(&window_id).unwrap().redraw_pending);
    }

    #[test]
    fn replacing_owned_regions_never_requires_an_unclipped_state() {
        let window = TestWindow::new();
        let window_id = id(window.0);
        let owner = Rect::new(1000, 0, 1000, 800);

        for step in 0..128 {
            let x = 200 + step * 3;
            let outer = Rect::new(x, 0, 1000, 800);
            assert!(apply_window_region_clip(window_id, outer, outer, owner, false).succeeded());
            let expected = relative_clip_region(outer, outer, owner).unwrap();
            assert!(actual_region_matches(window.0, expected));
        }
        assert!(restore_window_region(window_id, true));
        assert!(window_has_no_region(window.0));
    }

    #[test]
    fn a_left_preview_never_occupies_the_left_physical_monitor() {
        // Monitor 1: 0..1000, monitor 2(owner): 1000..2000.
        // A 75%-wide left neighbour in a 25/50/25 view spans 250..1250.
        // Only 1000..1250 may remain visible.
        let monitor_one = Rect::new(0, 0, 1000, 800);
        let owner = Rect::new(1000, 0, 1000, 800);
        let window = Rect::new(250, 0, 1000, 800);
        let region = relative_clip_region(window, window, owner).unwrap();
        assert_eq!(region, Rect::new(750, 0, 250, 800));
        let visible_screen = Rect::new(window.x + region.x, 0, region.width, 800);
        assert_eq!(visible_screen, Rect::new(1000, 0, 250, 800));
        assert!(!visible_screen.intersects(&monitor_one));
    }
}
'''
text = read(region_path)
if 'mod atomic_region_transaction_tests' in text:
    raise RuntimeError('window_region.rs: atomic tests already present')
write(region_path, text.rstrip() + region_tests)

# ---------------------------------------------------------------------------
# Placement ordering: region first, synchronous move second, landing redraw
# last. Ordinary windows retain the existing defer/async hot path.
# ---------------------------------------------------------------------------
replace_once(
    placement_path,
    '''    SWP_ASYNCWINDOWPOS, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SW_RESTORE,
''',
    '''    SWP_ASYNCWINDOWPOS, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOCOPYBITS, SWP_NOSIZE,
    SWP_NOZORDER, SW_RESTORE,
''',
)

replace_once(
    placement_path,
    '''fn visible_position_flags(
    animation_frame: bool,
    dispatch: AnimationDispatchMode,
    position_only: bool,
) -> SET_WINDOW_POS_FLAGS {
''',
    '''fn region_position_flags(
    flags: SET_WINDOW_POS_FLAGS,
    animation_frame: bool,
) -> SET_WINDOW_POS_FLAGS {
    let mut flags = SET_WINDOW_POS_FLAGS(flags.0 & !SWP_ASYNCWINDOWPOS.0);
    // The exact landing discards stale client bits once. Intermediate frames
    // retain them for smoothness and avoid a per-frame repaint tax.
    if !animation_frame {
        flags |= SWP_NOCOPYBITS;
    }
    flags
}

fn visible_position_flags(
    animation_frame: bool,
    dispatch: AnimationDispatchMode,
    position_only: bool,
) -> SET_WINDOW_POS_FLAGS {
''',
)

# Force every clipped HWND onto the synchronous path, independently of class.
replace_regex(
    placement_path,
    r'''        let dispatch = if animation_frame \{\n            let sensitive = policy == AnimationPlacementPolicy::AdaptiveCompositorSafe.*?        \} else \{\n            AnimationDispatchMode::Synchronous\n        \};''',
    '''        let dispatch = if region_clip.is_some() {
            if animation_frame && unsafe { IsHungAppWindow(hwnd).as_bool() } {
                AnimationDispatchMode::SkipHungSensitive
            } else {
                AnimationDispatchMode::Synchronous
            }
        } else if animation_frame {
            let sensitive = policy == AnimationPlacementPolicy::AdaptiveCompositorSafe
                && cached_compositor_sensitive(hwnd, placement.window_id, cache.as_deref_mut());
            let hung = sensitive && unsafe { IsHungAppWindow(hwnd).as_bool() };
            animation_dispatch_mode(policy, sensitive, hung)
        } else {
            AnimationDispatchMode::Synchronous
        };''',
)

replace_once(
    placement_path,
    '''            let flags = visible_position_flags(animation_frame, dispatch, position_only);
            entries.push(DeferEntry {
''',
    '''            let flags = visible_position_flags(animation_frame, dispatch, position_only);
            let flags = if region_clip.is_some() {
                region_position_flags(flags, animation_frame)
            } else {
                flags
            };
            entries.push(DeferEntry {
''',
)

# Reorder the apply pass so a window cannot be unveiled or moved at its target
# without the target-local HRGN already installed.
replace_once(
    placement_path,
    '''    // Uncloak windows that are becoming visible BEFORE positioning,
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
''',
    '''    // Install target-local HRGNs before moving clipped windows. This prevents
    // the target frame from ever being composed unbounded on a neighbour.
    let mut failed_window_ids = HashSet::new();
    let mut region_fallbacks =
        prepare_entry_region_clips(&mut entries, &mut failed_window_ids);

    let (applied, position_failures) = position_entries(&entries);
    failed_window_ids.extend(position_failures);
    region_fallbacks += finalize_entry_region_clips(
        &mut entries,
        &mut failed_window_ids,
        animation_frame,
    );

    // Reveal only after both geometry and any region/fallback transaction have
    // committed. Off-screen-to-visible transitions therefore cannot flash an
    // unclipped gray surface on an adjacent monitor.
    uncloak_becoming_visible(&entries);
''',
)

# Replace the old post-move region function and fallback helper with the
# two-phase transaction.
replace_regex(
    placement_path,
    r'''fn set_entry_to_fallback\(.*?\n\}\n\n/// Uncloak entries becoming visible''',
    '''fn configure_entry_fallback(entry: &mut DeferEntry, animation_frame: bool) -> bool {
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
        entry.w = rect.width.saturating_add(inset_l).saturating_add(inset_r);
        entry.h = rect.height.saturating_add(inset_t).saturating_add(inset_b);
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

fn position_entry(entry: &DeferEntry) -> bool {
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
}

fn apply_entry_fallback(entry: &mut DeferEntry, animation_frame: bool) -> bool {
    configure_entry_fallback(entry, animation_frame) && position_entry(entry)
}

/// Pre-install each target-local HRGN before the HWND move. Definite ownership
/// or API failures are converted to the precomputed safe fallback before the
/// batch is submitted.
fn prepare_entry_region_clips(
    entries: &mut [DeferEntry],
    failed_window_ids: &mut HashSet<u64>,
) -> u32 {
    let mut fallback_count = 0;
    for entry in entries {
        let Some(clip_bounds) = entry.region_clip_bounds else {
            continue;
        };
        let outer_rect = Rect::new(entry.x, entry.y, entry.w.max(1), entry.h.max(1));
        if apply_window_region_clip(
            entry.window_id,
            outer_rect,
            entry.layout_rect,
            clip_bounds,
            false,
        )
        .succeeded()
        {
            continue;
        }

        let _ = restore_window_region(entry.window_id, false);
        fallback_count += 1;
        if !configure_entry_fallback(entry, true) {
            failed_window_ids.insert(entry.window_id);
        }
    }
    fallback_count
}

/// Finish the transaction after positioning. The exact landing performs one
/// redraw even when the local HRGN did not change during the final animation
/// frame. A failed move or redraw is replaced by the safe fallback immediately.
fn finalize_entry_region_clips(
    entries: &mut [DeferEntry],
    failed_window_ids: &mut HashSet<u64>,
    animation_frame: bool,
) -> u32 {
    let mut fallback_count = 0;
    for entry in entries {
        let Some(clip_bounds) = entry.region_clip_bounds else {
            continue;
        };

        if failed_window_ids.contains(&entry.window_id) {
            let _ = restore_window_region(entry.window_id, false);
            fallback_count += 1;
            if apply_entry_fallback(entry, animation_frame) {
                failed_window_ids.remove(&entry.window_id);
            }
            continue;
        }
        if animation_frame {
            continue;
        }

        let outer_rect = Rect::new(entry.x, entry.y, entry.w.max(1), entry.h.max(1));
        if apply_window_region_clip(
            entry.window_id,
            outer_rect,
            entry.layout_rect,
            clip_bounds,
            true,
        )
        .succeeded()
        {
            continue;
        }

        let _ = restore_window_region(entry.window_id, false);
        fallback_count += 1;
        if !apply_entry_fallback(entry, false) {
            failed_window_ids.insert(entry.window_id);
        }
    }
    fallback_count
}

/// Uncloak entries becoming visible''',
)

placement_tests = r'''

#[cfg(test)]
mod atomic_region_move_tests {
    use super::*;

    #[test]
    fn clipped_animation_moves_are_synchronous() {
        let base = SWP_NOZORDER | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS;
        let flags = region_position_flags(base, true);
        assert_eq!(flags.0 & SWP_ASYNCWINDOWPOS.0, 0);
        assert_eq!(flags.0 & SWP_NOCOPYBITS.0, 0);
    }

    #[test]
    fn clipped_landings_discard_stale_client_bits_once() {
        let base = SWP_NOZORDER | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS;
        let flags = region_position_flags(base, false);
        assert_eq!(flags.0 & SWP_ASYNCWINDOWPOS.0, 0);
        assert_ne!(flags.0 & SWP_NOCOPYBITS.0, 0);
    }

    #[test]
    fn fallback_configuration_removes_the_region_request() {
        let mut entry = DeferEntry {
            hwnd: HWND::default(),
            window_id: 1,
            x: 0,
            y: 0,
            w: 100,
            h: 100,
            layout_rect: Rect::new(0, 0, 100, 100),
            used_insets: (0, 0, 0, 0),
            validate_insets: true,
            visibility: Visibility::Visible,
            flags: SWP_NOZORDER,
            column_index: 0,
            region_clip_bounds: Some(Rect::new(1000, 0, 1000, 800)),
            fallback_rect: Some(Rect::new(-2000, -2000, 100, 100)),
            fallback_visibility: Some(Visibility::OffScreenLeft),
        };
        assert!(configure_entry_fallback(&mut entry, true));
        assert_eq!(entry.region_clip_bounds, None);
        assert_eq!(entry.visibility, Visibility::OffScreenLeft);
        assert_eq!(entry.layout_rect, Rect::new(-2000, -2000, 100, 100));
    }
}
'''
text = read(placement_path)
if 'mod atomic_region_move_tests' in text:
    raise RuntimeError('placement.rs: atomic tests already present')
write(placement_path, text.rstrip() + placement_tests)

print('atomic SetWindowRgn transaction patch applied')
