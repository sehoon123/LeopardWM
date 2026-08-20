from pathlib import Path

ROOT = Path('.')
THUMBNAIL = ROOT / 'crates/platform_win32/src/thumbnail.rs'
PLACEMENT = ROOT / 'crates/platform_win32/src/placement.rs'


def read(path: Path) -> str:
    return path.read_text(encoding='utf-8')


def write(path: Path, text: str) -> None:
    path.write_text(text, encoding='utf-8', newline='\n')


def replace_once(path: Path, old: str, new: str) -> None:
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


def replace_function(path: Path, signature: str, replacement: str) -> None:
    text = read(path)
    start, end = function_span(text, signature)
    write(path, text[:start] + replacement + text[end:])


def remove_function(path: Path, signature: str) -> None:
    text = read(path)
    start, end = function_span(text, signature)
    while end < len(text) and text[end] == '\n':
        end += 1
    write(path, text[:start] + text[end:])


replace_once(
    THUMBNAIL,
    'use std::ffi::c_void;\n',
    'use std::collections::HashMap;\nuse std::ffi::c_void;\n',
)
replace_once(
    THUMBNAIL,
    '''    DwmUpdateThumbnailProperties, DWM_THUMBNAIL_PROPERTIES, DWM_TNP_OPACITY,\n    DWM_TNP_RECTDESTINATION, DWM_TNP_VISIBLE,\n''',
    '''    DwmUpdateThumbnailProperties, DWM_THUMBNAIL_PROPERTIES, DWM_TNP_OPACITY,\n    DWM_TNP_RECTDESTINATION, DWM_TNP_RECTSOURCE, DWM_TNP_VISIBLE,\n''',
)
replace_function(
    THUMBNAIL,
    'pub fn update(',
    '''pub fn update(
    handle: isize,
    dest_client_rect: Rect,
    opacity: u8,
    visible: bool,
) -> Result<(), Win32Error> {
    update_properties(handle, None, dest_client_rect, opacity, visible)
}

pub(crate) fn update_cropped(
    handle: isize,
    source_rect: Rect,
    dest_client_rect: Rect,
    opacity: u8,
    visible: bool,
) -> Result<(), Win32Error> {
    update_properties(
        handle,
        Some(source_rect),
        dest_client_rect,
        opacity,
        visible,
    )
}

fn update_properties(
    handle: isize,
    source_rect: Option<Rect>,
    dest_client_rect: Rect,
    opacity: u8,
    visible: bool,
) -> Result<(), Win32Error> {
    if handle == 0 {
        return Err(Win32Error::SetPositionFailed(
            "thumbnail::update called with null handle".into(),
        ));
    }
    let mut flags = DWM_TNP_RECTDESTINATION | DWM_TNP_OPACITY | DWM_TNP_VISIBLE;
    let mut props = DWM_THUMBNAIL_PROPERTIES {
        dwFlags: flags,
        rcDestination: RECT {
            left: dest_client_rect.x,
            top: dest_client_rect.y,
            right: dest_client_rect.x + dest_client_rect.width,
            bottom: dest_client_rect.y + dest_client_rect.height,
        },
        rcSource: RECT::default(),
        opacity,
        fVisible: BOOL::from(visible),
        fSourceClientAreaOnly: BOOL::from(false),
    };
    if let Some(source) = source_rect {
        flags |= DWM_TNP_RECTSOURCE;
        props.dwFlags = flags;
        props.rcSource = RECT {
            left: source.x,
            top: source.y,
            right: source.x + source.width,
            bottom: source.y + source.height,
        };
    }
    unsafe { DwmUpdateThumbnailProperties(handle, &props) }.map_err(|error| {
        Win32Error::SetPositionFailed(format!("DwmUpdateThumbnailProperties: {error}"))
    })
}''',
)

manager = r'''
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PersistentPreviewRequest {
    pub window_id: WindowId,
    pub source_rect: Rect,
    pub expected_source_size: (i32, i32),
    pub destination_screen_rect: Rect,
}

struct PersistentPreview {
    handle: ThumbnailHandle,
    source_size: Option<(i32, i32)>,
    expected_source_size: Option<(i32, i32)>,
}

static PERSISTENT_PREVIEWS: OnceLock<Mutex<HashMap<WindowId, PersistentPreview>>> =
    OnceLock::new();
static PERSISTENT_PREVIEW_TRANSACTION: Mutex<()> = Mutex::new(());

fn persistent_previews() -> &'static Mutex<HashMap<WindowId, PersistentPreview>> {
    PERSISTENT_PREVIEWS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_persistent_previews(
) -> std::sync::MutexGuard<'static, HashMap<WindowId, PersistentPreview>> {
    persistent_previews()
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
}

pub(crate) fn lock_persistent_preview_transaction() -> std::sync::MutexGuard<'static, ()> {
    PERSISTENT_PREVIEW_TRANSACTION
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
}

pub(crate) fn prepare_persistent_preview(window_id: WindowId) -> bool {
    #[cfg(test)]
    {
        let _ = window_id;
        false
    }
    #[cfg(not(test))]
    {
        let mut previews = lock_persistent_previews();
        if previews.contains_key(&window_id) {
            return true;
        }
        let Ok(handle) = register(window_id) else {
            return false;
        };
        let initial_size = source_size(handle.as_isize());
        previews.insert(
            window_id,
            PersistentPreview {
                handle,
                source_size: initial_size,
                expected_source_size: None,
            },
        );
        true
    }
}

fn scale_edge(value: i32, actual: i32, expected: i32) -> i32 {
    if actual <= 0 || expected <= 0 {
        return 0;
    }
    ((i64::from(value.max(0)) * i64::from(actual) + i64::from(expected) / 2)
        / i64::from(expected))
    .clamp(0, i64::from(actual)) as i32
}

fn normalized_preview_geometry(
    request: PersistentPreviewRequest,
    actual_source_size: (i32, i32),
) -> Option<(Rect, Rect)> {
    let (actual_w, actual_h) = actual_source_size;
    let (expected_w, expected_h) = request.expected_source_size;
    if actual_w <= 0 || actual_h <= 0 || expected_w <= 0 || expected_h <= 0 {
        return None;
    }
    let left = scale_edge(request.source_rect.x, actual_w, expected_w);
    let top = scale_edge(request.source_rect.y, actual_h, expected_h);
    let right = scale_edge(request.source_rect.right(), actual_w, expected_w).max(left);
    let bottom = scale_edge(request.source_rect.bottom(), actual_h, expected_h).max(top);
    if right <= left || bottom <= top {
        return None;
    }
    Some((
        Rect::new(left, top, right - left, bottom - top),
        request.destination_screen_rect,
    ))
}

fn publish_preview_requests(
    requests: &[PersistentPreviewRequest],
    refresh_size: bool,
) -> usize {
    #[cfg(test)]
    {
        let _ = (requests, refresh_size);
        0
    }
    #[cfg(not(test))]
    {
        let origin = host_origin();
        let mut failed = Vec::new();
        let mut published = 0usize;
        let mut previews = lock_persistent_previews();
        for request in requests {
            let Some(preview) = previews.get_mut(&request.window_id) else {
                continue;
            };
            if refresh_size
                && (preview.expected_source_size != Some(request.expected_source_size)
                    || preview.source_size.is_none())
            {
                preview.source_size = source_size(preview.handle.as_isize());
                preview.expected_source_size = Some(request.expected_source_size);
            }
            let Some(source_size) = preview.source_size else {
                if refresh_size {
                    failed.push(request.window_id);
                }
                continue;
            };
            let Some((source, destination_screen)) =
                normalized_preview_geometry(*request, source_size)
            else {
                if refresh_size {
                    failed.push(request.window_id);
                }
                continue;
            };
            let destination = screen_to_host_client(destination_screen, origin);
            if update_cropped(
                preview.handle.as_isize(),
                source,
                destination,
                255,
                true,
            )
            .is_ok()
            {
                published += 1;
            } else {
                failed.push(request.window_id);
            }
        }
        if refresh_size {
            for window_id in failed {
                previews.remove(&window_id);
            }
        }
        published
    }
}

pub(crate) fn publish_persistent_previews(requests: &[PersistentPreviewRequest]) -> usize {
    publish_preview_requests(requests, false)
}

pub(crate) fn commit_persistent_previews(
    requests: &[PersistentPreviewRequest],
    refresh_source_size: bool,
) -> usize {
    let published = publish_preview_requests(requests, refresh_source_size);
    let mut previews = lock_persistent_previews();
    previews.retain(|window_id, _| {
        requests
            .iter()
            .any(|request| request.window_id == *window_id)
    });
    published.min(previews.len())
}

pub(crate) fn clear_persistent_previews() {
    lock_persistent_previews().clear();
}

pub(crate) fn forget_persistent_preview(window_id: WindowId) {
    lock_persistent_previews().remove(&window_id);
}
'''
text = read(THUMBNAIL)
marker = 'pub fn source_size(handle: isize) -> Option<(i32, i32)> {'
if text.count(marker) != 1:
    raise RuntimeError('thumbnail.rs: source_size marker mismatch')
write(THUMBNAIL, text.replace(marker, manager + '\n' + marker, 1))

replace_once(
    THUMBNAIL,
    '        || class == "WinUIDesktopWin32WindowClass"\n',
    '        || class == "WinUIDesktopWin32WindowClass"\n        || class == "Notepad"\n',
)

thumbnail_tests = r'''

#[cfg(test)]
mod persistent_preview_geometry_tests {
    use super::{normalized_preview_geometry, PersistentPreviewRequest};
    use leopardwm_core_layout::Rect;

    #[test]
    fn matching_source_size_preserves_crop_and_destination() {
        let request = PersistentPreviewRequest {
            window_id: 1,
            source_rect: Rect::new(500, 0, 250, 800),
            expected_source_size: (750, 800),
            destination_screen_rect: Rect::new(1000, 0, 250, 800),
        };
        assert_eq!(
            normalized_preview_geometry(request, (750, 800)),
            Some((request.source_rect, request.destination_screen_rect))
        );
    }

    #[test]
    fn crop_scales_to_the_actual_dwm_source_size() {
        let request = PersistentPreviewRequest {
            window_id: 1,
            source_rect: Rect::new(500, 80, 250, 640),
            expected_source_size: (750, 800),
            destination_screen_rect: Rect::new(1000, 80, 250, 640),
        };
        assert_eq!(
            normalized_preview_geometry(request, (1500, 1600)),
            Some((
                Rect::new(1000, 160, 500, 1280),
                request.destination_screen_rect,
            ))
        );
    }

    #[test]
    fn invalid_or_empty_source_geometry_is_rejected() {
        let request = PersistentPreviewRequest {
            window_id: 1,
            source_rect: Rect::new(750, 0, 0, 800),
            expected_source_size: (750, 800),
            destination_screen_rect: Rect::new(1000, 0, 0, 800),
        };
        assert_eq!(normalized_preview_geometry(request, (750, 800)), None);
    }
}
'''
write(THUMBNAIL, read(THUMBNAIL).rstrip() + thumbnail_tests + '\n')

replace_once(
    PLACEMENT,
    'use crate::types::{AnimationPlacementPolicy, PlatformConfig, Win32Error};\n',
    '''use crate::thumbnail::{
    clear_persistent_previews, commit_persistent_previews, forget_persistent_preview,
    lock_persistent_preview_transaction, prepare_persistent_preview,
    publish_persistent_previews, PersistentPreviewRequest,
};
use crate::types::{AnimationPlacementPolicy, PlatformConfig, Win32Error};
''',
)
replace_once(
    PLACEMENT,
    '''use crate::window_region::{
    apply_window_region_clip, has_owned_window_region, prepare_window_region_clip,
    reconcile_window_regions, restore_all_window_regions, restore_window_region, WindowRegionClip,
};
''',
    '''use crate::window_region::{
    has_owned_window_region, reconcile_window_regions, restore_all_window_regions,
    restore_window_region, WindowRegionClip,
};
''',
)
replace_once(
    PLACEMENT,
    'pub fn dwm_uncloak_all() {\n    restore_all_window_regions();\n',
    'pub fn dwm_uncloak_all() {\n    clear_persistent_previews();\n    restore_all_window_regions();\n',
)
replace_once(
    PLACEMENT,
    '''    region_clip_bounds: Option<Rect>,
    fallback_rect: Option<Rect>,
    fallback_visibility: Option<Visibility>,
''',
    '    preview_source: bool,\n',
)
replace_once(
    PLACEMENT,
    '''        // Empty layout is also a hard region-lifecycle boundary.
        restore_all_window_regions();
''',
    '''        clear_persistent_previews();
        // Empty layout is also a hard region-lifecycle boundary.
        restore_all_window_regions();
''',
)
replace_once(
    PLACEMENT,
    '''    let managed_window_ids: HashSet<WindowId> = placements
        .iter()
        .map(|placement| placement.window_id)
        .collect();

    // Prepare all window entries — visible and off-screen alike.
''',
    '''    let managed_window_ids: HashSet<WindowId> = placements
        .iter()
        .map(|placement| placement.window_id)
        .collect();
    let _preview_transaction = lock_persistent_preview_transaction();

    // Prepare all window entries — visible and off-screen alike.
''',
)
old_flow = '''    let (mut entries, skipped) = build_defer_entries(
        placements,
        region_clips,
        &mut cache,
        animation_frame,
        config.animation_placement_policy,
        high_contrast,
    );

    // Restrict first, then reveal and move. This removes the frame in which
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
new_flow = '''    let (entries, preview_requests, skipped, safe_fallbacks) = build_defer_entries(
        placements,
        region_clips,
        &mut cache,
        animation_frame,
        config.animation_placement_policy,
        high_contrast,
    );

    uncloak_preview_sources(&preview_requests);
    let _ = publish_persistent_previews(&preview_requests);
    uncloak_becoming_visible(&entries);

    let (applied, failed_window_ids) = position_entries(&entries);

    for clip in region_clips {
        let _ = restore_window_region(clip.window_id, true);
    }
    let active_preview_count =
        commit_persistent_previews(&preview_requests, !animation_frame);
    reconcile_window_regions(&managed_window_ids, &HashSet::new(), !animation_frame);
'''
replace_once(PLACEMENT, old_flow, new_flow)
replace_once(
    PLACEMENT,
    '''        "Applied {} placements ({} skipped unchanged), {} region fallback(s), {} off-screen total",
        applied,
        skipped,
        region_fallbacks,
        offscreen_count,
''',
    '''        "Applied {} placements ({} skipped unchanged), {} DWM preview(s), {} safe fallback(s), {} off-screen total",
        applied,
        skipped,
        active_preview_count,
        safe_fallbacks,
        offscreen_count,
''',
)
replace_once(
    PLACEMENT,
    '    sync_cloak_state(&entries, placements, &failed_window_ids);\n',
    '    sync_cloak_state(&entries, placements, &failed_window_ids, &preview_requests);\n',
)
preview_helper = r'''
fn persistent_preview_request(
    window_id: WindowId,
    target_outer: Rect,
    clip_bounds: Rect,
) -> Option<PersistentPreviewRequest> {
    let left = target_outer.x.max(clip_bounds.x);
    let top = target_outer.y.max(clip_bounds.y);
    let right = target_outer.right().min(clip_bounds.right());
    let bottom = target_outer.bottom().min(clip_bounds.bottom());
    if right <= left || bottom <= top {
        return None;
    }
    let source = Rect::new(
        left.saturating_sub(target_outer.x),
        top.saturating_sub(target_outer.y),
        right - left,
        bottom - top,
    );
    let destination = Rect::new(left, top, right - left, bottom - top);
    Some(PersistentPreviewRequest {
        window_id,
        source_rect: source,
        expected_source_size: (target_outer.width.max(1), target_outer.height.max(1)),
        destination_screen_rect: destination,
    })
}

'''
text = read(PLACEMENT)
marker = '/// Build the defer-entry list for all placements, skipping cache-unchanged windows.\n'
if text.count(marker) != 1:
    raise RuntimeError('placement.rs: build-defer marker mismatch')
write(PLACEMENT, text.replace(marker, preview_helper + marker, 1))
replace_function(
    PLACEMENT,
    'fn build_defer_entries(',
    r'''fn build_defer_entries(
    placements: &[WindowPlacement],
    region_clips: &[WindowRegionClip],
    cache: &mut Option<&mut PlacementCache>,
    animation_frame: bool,
    policy: AnimationPlacementPolicy,
    high_contrast: bool,
) -> (Vec<DeferEntry>, Vec<PersistentPreviewRequest>, u32, u32) {
    let mut skipped = 0u32;
    let mut safe_fallbacks = 0u32;
    let mut entries: Vec<DeferEntry> = Vec::with_capacity(placements.len());
    let mut preview_requests = Vec::with_capacity(region_clips.len());

    for requested in placements {
        let region_clip = region_clips
            .iter()
            .find(|clip| clip.window_id == requested.window_id);
        let Ok(hwnd) = window_id_to_hwnd(requested.window_id) else {
            continue;
        };
        unsafe {
            if !IsWindow(Some(hwnd)).as_bool() || IsIconic(hwnd).as_bool() {
                continue;
            }
            if requested.visibility == Visibility::Visible
                && requested.column_index != usize::MAX
                && IsZoomed(hwnd).as_bool()
            {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
        }

        let (inset_l, inset_t, inset_r, inset_b) = if high_contrast {
            (0, 0, 0, 0)
        } else {
            cached_border_insets(hwnd, requested.window_id, cache.as_deref_mut())
        };
        let target_frame_w = requested.rect.width + inset_l + inset_r;
        let target_frame_h = requested.rect.height + inset_t + inset_b;
        let target_outer = Rect::new(
            requested.rect.x - inset_l,
            requested.rect.y - inset_t,
            target_frame_w.max(1),
            target_frame_h.max(1),
        );

        let mut preview_source = false;
        let mut placement = requested.clone();
        let mut preview_request = None;
        if let Some(clip) = region_clip {
            placement.rect = clip.fallback_rect;
            placement.visibility = clip.fallback_visibility;
            if clip.fallback_visibility != Visibility::Visible
                && !ghost_cloaked_contains(requested.window_id)
            {
                preview_request = persistent_preview_request(
                    requested.window_id,
                    target_outer,
                    clip.clip_bounds,
                );
                if preview_request.is_some() && prepare_persistent_preview(requested.window_id) {
                    preview_source = true;
                } else {
                    preview_request = None;
                    safe_fallbacks += 1;
                }
            }
        }

        let previous = cache
            .as_ref()
            .and_then(|cache| cache.positions.get(&placement.window_id).copied());
        let unchanged = previous == Some((placement.rect, placement.visibility));
        if unchanged && region_clip.is_none() && !has_owned_window_region(placement.window_id) {
            skipped += 1;
            continue;
        }
        let position_only = animation_move_is_position_only(previous, &placement);
        let managed_transition = region_clip.is_some()
            || has_owned_window_region(placement.window_id)
            || preview_source;
        let dispatch = if animation_frame {
            let sensitive = managed_transition
                || (policy == AnimationPlacementPolicy::AdaptiveCompositorSafe
                    && cached_compositor_sensitive(
                        hwnd,
                        placement.window_id,
                        cache.as_deref_mut(),
                    ));
            let hung = sensitive && unsafe { IsHungAppWindow(hwnd).as_bool() };
            if managed_transition && !hung {
                AnimationDispatchMode::Synchronous
            } else {
                animation_dispatch_mode(policy, sensitive, hung)
            }
        } else {
            AnimationDispatchMode::Synchronous
        };
        if dispatch == AnimationDispatchMode::SkipHungSensitive {
            skipped += 1;
            continue;
        }

        if let Some(request) = preview_request {
            preview_requests.push(request);
        }

        if preview_source {
            let flags = SWP_NOZORDER | SWP_NOACTIVATE;
            entries.push(DeferEntry {
                hwnd,
                window_id: placement.window_id,
                x: placement.rect.x.saturating_sub(inset_l),
                y: placement.rect.y.saturating_sub(inset_t),
                w: target_frame_w.max(1),
                h: target_frame_h.max(1),
                layout_rect: placement.rect,
                used_insets: (inset_l, inset_t, inset_r, inset_b),
                validate_insets: !high_contrast,
                visibility: placement.visibility,
                flags,
                column_index: placement.column_index,
                preview_source: true,
            });
        } else if placement.visibility == Visibility::Visible {
            let frame_w = placement.rect.width + inset_l + inset_r;
            let frame_h = placement.rect.height + inset_t + inset_b;
            let flags = visible_position_flags(animation_frame, dispatch, position_only);
            entries.push(DeferEntry {
                hwnd,
                window_id: placement.window_id,
                x: placement.rect.x - inset_l,
                y: placement.rect.y - inset_t,
                w: frame_w,
                h: frame_h,
                layout_rect: placement.rect,
                used_insets: (inset_l, inset_t, inset_r, inset_b),
                validate_insets: !high_contrast,
                visibility: placement.visibility,
                flags,
                column_index: placement.column_index,
                preview_source: false,
            });
        } else {
            let frame_w = placement.rect.width + inset_l + inset_r;
            let (x, y) = offscreen_position(&placement, inset_l, inset_t);
            let mut flags = SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE;
            if animation_frame && dispatch == AnimationDispatchMode::Asynchronous {
                flags |= SWP_ASYNCWINDOWPOS;
            }
            entries.push(DeferEntry {
                hwnd,
                window_id: placement.window_id,
                x,
                y,
                w: frame_w,
                h: 0,
                layout_rect: placement.rect,
                used_insets: (inset_l, inset_t, inset_r, inset_b),
                validate_insets: !high_contrast,
                visibility: placement.visibility,
                flags,
                column_index: placement.column_index,
                preview_source: false,
            });
        }
    }

    (entries, preview_requests, skipped, safe_fallbacks)
}''',
)
for signature in (
    'fn configure_entry_fallback(',
    'fn set_entry_to_fallback(',
    'fn prepare_entry_region_clips(',
    'fn apply_entry_region_clips(',
):
    remove_function(PLACEMENT, signature)
cloak_helpers = r'''
fn uncloak_preview_sources(requests: &[PersistentPreviewRequest]) {
    if requests.is_empty() {
        return;
    }
    let _commit = lock_cloak_commit();
    let mut changed = Vec::new();
    {
        let mut cloaked = lock_cloaked();
        if let Some(ref mut set) = *cloaked {
            for request in requests {
                if set.remove(&request.window_id) {
                    changed.push(request.window_id);
                }
            }
        }
    }
    for window_id in changed {
        let _ = apply_cloak_state_locked(window_id);
    }
}

fn should_cloak_entry(
    entry: &DeferEntry,
    placement_exists: bool,
    positioning_failed: bool,
) -> bool {
    placement_exists
        && !positioning_failed
        && entry.visibility != Visibility::Visible
        && !entry.preview_source
}

'''
text = read(PLACEMENT)
marker = '/// Uncloak entries becoming visible and drop them from the tracking set.\n'
if text.count(marker) != 1:
    raise RuntimeError('placement.rs: uncloak marker mismatch')
write(PLACEMENT, text.replace(marker, cloak_helpers + marker, 1))
replace_function(
    PLACEMENT,
    'fn sync_cloak_state(',
    r'''fn sync_cloak_state(
    entries: &[DeferEntry],
    placements: &[WindowPlacement],
    failed_window_ids: &HashSet<u64>,
    preview_requests: &[PersistentPreviewRequest],
) {
    let preview_ids: HashSet<WindowId> = preview_requests
        .iter()
        .map(|request| request.window_id)
        .collect();
    let _commit = lock_cloak_commit();
    let mut changed: Vec<WindowId> = Vec::new();
    {
        let mut guard = lock_cloaked();
        let cloaked = guard.get_or_insert_with(HashSet::new);
        for entry in entries {
            let placement_exists = placements
                .iter()
                .any(|placement| placement.window_id == entry.window_id);
            let should_cloak = should_cloak_entry(
                entry,
                placement_exists,
                failed_window_ids.contains(&entry.window_id),
            );
            if should_cloak {
                if cloaked.insert(entry.window_id) {
                    changed.push(entry.window_id);
                }
            } else if cloaked.remove(&entry.window_id) {
                changed.push(entry.window_id);
            }
        }
        for window_id in &preview_ids {
            if cloaked.remove(window_id) {
                changed.push(*window_id);
            }
        }
        let current_ids: HashSet<u64> = placements
            .iter()
            .map(|placement| placement.window_id)
            .collect();
        let stale: Vec<WindowId> = cloaked
            .iter()
            .filter(|window_id| !current_ids.contains(window_id))
            .copied()
            .collect();
        for window_id in stale {
            cloaked.remove(&window_id);
            changed.push(window_id);
        }
    }
    changed.sort_unstable();
    changed.dedup();
    for window_id in changed {
        let _ = apply_cloak_state_locked(window_id);
    }
}''',
)
replace_once(
    PLACEMENT,
    'pub fn clear_suspected_oversize(window_id: WindowId) {\n',
    'pub fn clear_suspected_oversize(window_id: WindowId) {\n    forget_persistent_preview(window_id);\n',
)
placement_tests = r'''

#[cfg(test)]
mod persistent_preview_placement_tests {
    use super::{persistent_preview_request, should_cloak_entry, DeferEntry};
    use leopardwm_core_layout::{Rect, Visibility};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::SET_WINDOW_POS_FLAGS;

    fn preview_entry() -> DeferEntry {
        DeferEntry {
            hwnd: HWND::default(),
            window_id: 1,
            x: 0,
            y: 0,
            w: 750,
            h: 800,
            layout_rect: Rect::new(0, 0, 750, 800),
            used_insets: (0, 0, 0, 0),
            validate_insets: false,
            visibility: Visibility::OffScreenLeft,
            flags: SET_WINDOW_POS_FLAGS::default(),
            column_index: 0,
            preview_source: true,
        }
    }

    #[test]
    fn left_preview_is_cropped_strictly_to_the_owner_monitor() {
        let request = persistent_preview_request(
            1,
            Rect::new(500, 0, 750, 800),
            Rect::new(1000, 0, 1000, 800),
        )
        .unwrap();
        assert_eq!(request.source_rect, Rect::new(500, 0, 250, 800));
        assert_eq!(
            request.destination_screen_rect,
            Rect::new(1000, 0, 250, 800)
        );
        assert_eq!(request.expected_source_size, (750, 800));
        assert!(!request
            .destination_screen_rect
            .intersects(&Rect::new(0, 0, 1000, 800)));
    }

    #[test]
    fn right_preview_is_symmetric_and_cannot_touch_the_next_monitor() {
        let request = persistent_preview_request(
            1,
            Rect::new(1750, 0, 750, 800),
            Rect::new(1000, 0, 1000, 800),
        )
        .unwrap();
        assert_eq!(request.source_rect, Rect::new(0, 0, 250, 800));
        assert_eq!(
            request.destination_screen_rect,
            Rect::new(1750, 0, 250, 800)
        );
        assert!(!request
            .destination_screen_rect
            .intersects(&Rect::new(2000, 0, 1000, 800)));
    }

    #[test]
    fn preview_source_remains_uncloaked_while_ordinary_offscreen_windows_cloak() {
        let preview = preview_entry();
        assert!(!should_cloak_entry(&preview, true, false));
        let ordinary = DeferEntry {
            preview_source: false,
            ..preview
        };
        assert!(should_cloak_entry(&ordinary, true, false));
        assert!(!should_cloak_entry(&ordinary, false, false));
        assert!(!should_cloak_entry(&ordinary, true, true));
    }
}
'''
write(PLACEMENT, read(PLACEMENT).rstrip() + placement_tests + '\n')

print('DWM thumbnail preview proxy patch applied')
