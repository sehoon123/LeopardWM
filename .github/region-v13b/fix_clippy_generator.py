from pathlib import Path

path = Path('.github/region-v13b/apply_atomic_region.py')
text = path.read_text(encoding='utf-8')

old_prepare = '''    let current_region = if let Some(region) = current_owned {
        region
    } else {
        let Some((current_outer, current_visible)) = current_window_geometry(hwnd) else {
            return RegionClipResult::Failed;
        };
        allowed_region(current_outer, current_visible, clip_bounds)
    };
    let bridge = intersect_regions(current_region, target_region);
'''
new_prepare = '''    let bridge = if let Some(region) = current_owned {
        intersect_regions(region, target_region)
    } else {
        let Some((current_outer, current_visible)) = current_window_geometry(hwnd) else {
            return RegionClipResult::Failed;
        };
        bridge_clip_region(
            current_outer,
            current_visible,
            target_outer,
            target_visible,
            clip_bounds,
        )
    };
'''
if text.count(old_prepare) != 1:
    raise SystemExit(f'prepare bridge block mismatch: {text.count(old_prepare)}')
text = text.replace(old_prepare, new_prepare, 1)

marker = "write(REGION, text.replace(marker, marker + helpers, 1))\n"
cleanup = '''write(REGION, text.replace(marker, marker + helpers, 1))

# Ownership recovery is integrated into `owned_region_for_identity`.
# Remove the superseded helper and keep the convenience predicate test-only.
text = read(REGION)
start, end = function_span(
    text,
    'fn recover_stale_metadata(hwnd: HWND, redraw: bool) -> bool',
)
text = text[:start] + text[end:]
no_region = 'fn window_has_no_region(hwnd: HWND) -> bool {'
if text.count(no_region) != 1:
    raise RuntimeError('window_region.rs: no-region helper mismatch')
text = text.replace(no_region, '#[cfg(test)]\\n' + no_region, 1)
write(REGION, text)
'''
if text.count(marker) != 1:
    raise SystemExit(f'helper insertion marker mismatch: {text.count(marker)}')
text = text.replace(marker, cleanup, 1)

path.write_text(text, encoding='utf-8', newline='\n')
