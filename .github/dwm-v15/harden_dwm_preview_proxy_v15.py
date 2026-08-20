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
        raise RuntimeError(f'{path}: expected one occurrence, found {count}: {old[:120]!r}')
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


def remove_function(path: Path, signature: str) -> None:
    text = read(path)
    start, end = function_span(text, signature)
    while end < len(text) and text[end] == '\n':
        end += 1
    write(path, text[:start] + text[end:])


# The host already exposes its synchronized virtual-screen origin.
replace_once(THUMBNAIL, 'let origin = host_origin();', 'let origin = host().origin();')

# Production references these members; test stubs intentionally do not. Keep
# test-only dead-code diagnostics from obscuring real warnings.
replace_once(
    THUMBNAIL,
    'pub(crate) fn update_cropped(\n',
    '#[cfg_attr(test, allow(dead_code))]\npub(crate) fn update_cropped(\n',
)
replace_once(
    THUMBNAIL,
    'struct PersistentPreview {\n',
    '#[cfg_attr(test, allow(dead_code))]\nstruct PersistentPreview {\n',
)

# Publishing before the source HWND is parked can expose one DWM frame on the
# neighboring monitor. The hardened path publishes only after a synchronous
# off-screen landing, so this wrapper is deliberately removed.
remove_function(THUMBNAIL, 'pub(crate) fn publish_persistent_previews(')

preview_query = '''
pub(crate) fn has_persistent_preview(window_id: WindowId) -> bool {
    #[cfg(test)]
    {
        let _ = window_id;
        false
    }
    #[cfg(not(test))]
    {
        lock_persistent_previews().contains_key(&window_id)
    }
}

'''
text = read(THUMBNAIL)
marker = 'fn scale_edge(value: i32, actual: i32, expected: i32) -> i32 {'
if text.count(marker) != 1:
    raise RuntimeError('thumbnail.rs: scale_edge marker mismatch')
write(THUMBNAIL, text.replace(marker, preview_query + marker, 1))

replace_once(
    PLACEMENT,
    '''    clear_persistent_previews, commit_persistent_previews, forget_persistent_preview,
    lock_persistent_preview_transaction, prepare_persistent_preview,
    publish_persistent_previews, PersistentPreviewRequest,
''',
    '''    clear_persistent_previews, commit_persistent_previews, forget_persistent_preview,
    has_persistent_preview, lock_persistent_preview_transaction, prepare_persistent_preview,
    PersistentPreviewRequest,
''',
)

old_flow = '''    let (entries, preview_requests, skipped, safe_fallbacks) = build_defer_entries(
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
new_flow = '''    let (entries, preview_requests, new_preview_count, skipped, safe_fallbacks) =
        build_defer_entries(
            placements,
            region_clips,
            &mut cache,
            animation_frame,
            config.animation_placement_policy,
            high_contrast,
        );

    uncloak_becoming_visible(&entries);
    let (applied, failed_window_ids) = position_entries(&entries);

    let committed_preview_requests: Vec<_> = preview_requests
        .iter()
        .copied()
        .filter(|request| !failed_window_ids.contains(&request.window_id))
        .collect();

    // The real source is now outside every monitor. Only now may it be
    // uncloaked and its legacy HRGN removed. A new thumbnail is not published
    // until DWM has committed that safe source landing.
    uncloak_preview_sources(&committed_preview_requests);
    for clip in region_clips {
        if !failed_window_ids.contains(&clip.window_id) {
            let _ = restore_window_region(clip.window_id, true);
        }
    }
    if new_preview_count > 0
        || (!animation_frame && !committed_preview_requests.is_empty())
    {
        unsafe {
            let _ = DwmFlush();
        }
    }
    let active_preview_count = commit_persistent_previews(
        &committed_preview_requests,
        !animation_frame || new_preview_count > 0,
    );

    // If the source move failed, retain an older LeopardWM region rather than
    // exposing the full foreign HWND. No new SetWindowRgn state is created by
    // this backend.
    let preserved_region_ids: HashSet<WindowId> = region_clips
        .iter()
        .filter(|clip| failed_window_ids.contains(&clip.window_id))
        .map(|clip| clip.window_id)
        .collect();
    reconcile_window_regions(
        &managed_window_ids,
        &preserved_region_ids,
        !animation_frame,
    );
'''
replace_once(PLACEMENT, old_flow, new_flow)

replace_once(
    PLACEMENT,
    '    sync_cloak_state(&entries, placements, &failed_window_ids, &preview_requests);\n',
    '''    sync_cloak_state(
        &entries,
        placements,
        &failed_window_ids,
        &committed_preview_requests,
    );
''',
)

replace_once(
    PLACEMENT,
    ') -> (Vec<DeferEntry>, Vec<PersistentPreviewRequest>, u32, u32) {\n',
    ') -> (Vec<DeferEntry>, Vec<PersistentPreviewRequest>, usize, u32, u32) {\n',
)
replace_once(
    PLACEMENT,
    '''    let mut entries: Vec<DeferEntry> = Vec::with_capacity(placements.len());
    let mut preview_requests = Vec::with_capacity(region_clips.len());
''',
    '''    let mut entries: Vec<DeferEntry> = Vec::with_capacity(placements.len());
    let mut preview_requests = Vec::with_capacity(region_clips.len());
    let mut new_preview_count = 0usize;
''',
)
replace_once(
    PLACEMENT,
    '''                if preview_request.is_some() && prepare_persistent_preview(requested.window_id) {
                    preview_source = true;
                } else {
''',
    '''                let preview_existed = has_persistent_preview(requested.window_id);
                if preview_request.is_some() && prepare_persistent_preview(requested.window_id) {
                    preview_source = true;
                    if !preview_existed {
                        new_preview_count += 1;
                    }
                } else {
''',
)
replace_once(
    PLACEMENT,
    '    (entries, preview_requests, skipped, safe_fallbacks)\n}\n',
    '    (entries, preview_requests, new_preview_count, skipped, safe_fallbacks)\n}\n',
)

# A failed SetWindowPos must preserve the previous cloak state. Removing the
# cloak on a source that never reached its safe parking coordinate recreates the
# exact gray spill this backend is intended to eliminate.
replace_once(
    PLACEMENT,
    '''        for entry in entries {
            let placement_exists = placements
                .iter()
                .any(|placement| placement.window_id == entry.window_id);
''',
    '''        for entry in entries {
            if failed_window_ids.contains(&entry.window_id) {
                continue;
            }
            let placement_exists = placements
                .iter()
                .any(|placement| placement.window_id == entry.window_id);
''',
)

# Pure regression coverage for the flush policy: a new proxy always waits for
# DWM source landing, while existing proxies avoid a per-frame vsync stall.
flush_helper = '''
fn preview_commit_needs_flush(
    animation_frame: bool,
    new_preview_count: usize,
    committed_preview_count: usize,
) -> bool {
    // existing animation frames do not block on DwmFlush; only activation and
    // the exact landing synchronize with the compositor.
    new_preview_count > 0 || (!animation_frame && committed_preview_count > 0)
}

'''
text = read(PLACEMENT)
marker = 'fn persistent_preview_request(\n'
if text.count(marker) != 1:
    raise RuntimeError('placement.rs: persistent_preview_request marker mismatch')
write(PLACEMENT, text.replace(marker, flush_helper + marker, 1))
replace_once(
    PLACEMENT,
    '''    if new_preview_count > 0
        || (!animation_frame && !committed_preview_requests.is_empty())
    {
''',
    '''    if preview_commit_needs_flush(
        animation_frame,
        new_preview_count,
        committed_preview_requests.len(),
    ) {
''',
)

text = read(PLACEMENT)
test_marker = '''    #[test]
    fn left_preview_is_cropped_strictly_to_the_owner_monitor() {
'''
flush_tests = '''    #[test]
    fn new_preview_flushes_once_but_existing_animation_frames_do_not() {
        assert!(super::preview_commit_needs_flush(true, 1, 1));
        assert!(!super::preview_commit_needs_flush(true, 0, 1));
        assert!(super::preview_commit_needs_flush(false, 0, 1));
        assert!(!super::preview_commit_needs_flush(false, 0, 0));
    }

'''
if text.count(test_marker) != 1:
    raise RuntimeError('placement.rs: preview test insertion marker mismatch')
write(PLACEMENT, text.replace(test_marker, flush_tests + test_marker, 1))

print('DWM preview proxy hardening applied')
exec(
    Path(__file__).with_name('cleanup_legacy_region_v15.py').read_text(encoding='utf-8'),
    {'__name__': '__main__'},
)
