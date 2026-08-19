from pathlib import Path
import re


def read(path: str) -> str:
    return Path(path).read_text(encoding='utf-8')


def write(path: str, text: str) -> None:
    Path(path).write_text(text, encoding='utf-8', newline='\n')


def replace_once(path: str, old: str, new: str) -> None:
    text = read(path)
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f'{path}: expected one post-patch target, found {count}: {old[:140]!r}')
    write(path, text.replace(old, new))


region_path = 'crates/platform_win32/src/window_region.rs'
replace_once(
    region_path,
    '''fn create_region(rect: Rect) -> Option<HRGN> {\n    if rect.width <= 0\n        || rect.height <= 0\n        || rect.x.abs() > GDI_COORD_MAX\n        || rect.y.abs() > GDI_COORD_MAX\n        || rect.right().abs() > GDI_COORD_MAX\n        || rect.bottom().abs() > GDI_COORD_MAX\n    {\n''',
    '''fn create_region(rect: Rect) -> Option<HRGN> {\n    let in_gdi_range = |value: i32| (-GDI_COORD_MAX..=GDI_COORD_MAX).contains(&value);\n    if rect.width <= 0\n        || rect.height <= 0\n        || !in_gdi_range(rect.x)\n        || !in_gdi_range(rect.y)\n        || !in_gdi_range(rect.right())\n        || !in_gdi_range(rect.bottom())\n    {\n''',
)
region = read(region_path)
owner_marker = '''fn owner_status(hwnd: HWND) -> OwnerStatus {\n    let Some(owner) = read_owner(hwnd) else {\n        return OwnerStatus::None;\n    };\n    if owner == current_owner() {\n        OwnerStatus::Current\n    } else if owner_process_is_alive(owner) {\n        OwnerStatus::OtherAlive\n    } else {\n        OwnerStatus::Stale\n    }\n}\n\n'''
if region.count(owner_marker) != 1:
    raise RuntimeError('window_region.rs: owner_status marker mismatch')
region = region.replace(
    owner_marker,
    owner_marker
    + '''fn ensure_current_owner(hwnd: HWND) -> bool {\n    match owner_status(hwnd) {\n        OwnerStatus::Current => true,\n        OwnerStatus::OtherAlive => false,\n        OwnerStatus::Stale => {\n            recover_metadata(hwnd, false) && write_owner(hwnd, current_owner())\n        }\n        OwnerStatus::None => write_owner(hwnd, current_owner()),\n    }\n}\n\n''',
)
region = region.replace(
    '    if !write_owner(hwnd, current_owner())\n',
    '    if !ensure_current_owner(hwnd)\n',
    1,
)
region = region.replace(
    '''    if !actual_region_matches(hwnd, expected_region) {\n        // The application replaced the region concurrently. Relinquish only\n        // LeopardWM metadata; never clear the application's replacement.\n        remove_all_metadata(hwnd);\n        lock_states().remove(&window_id);\n        return RegionClipResult::Unsupported;\n    }\n''',
    '''    let region_kind = current_region_kind(hwnd);\n    if region_kind != ERROR_REGION_KIND && !actual_region_matches(hwnd, expected_region) {\n        // The application replaced the region concurrently. Relinquish only\n        // LeopardWM metadata; never clear the application's replacement.\n        remove_all_metadata(hwnd);\n        lock_states().remove(&window_id);\n        return RegionClipResult::Unsupported;\n    }\n''',
    1,
)
write(region_path, region)

placement_path = 'crates/platform_win32/src/placement.rs'
placement = read(placement_path)
placement = placement.replace('        let mut placement = if let Some(clip)', '        let placement = if let Some(clip)', 1)
placement = placement.replace(
    '''        if !animation_frame {\n            entry.flags |= SWP_FRAMECHANGED;\n        }\n    } else {\n''',
    '''        if animation_frame {\n            entry.flags |= SWP_ASYNCWINDOWPOS;\n        } else {\n            entry.flags |= SWP_FRAMECHANGED;\n        }\n    } else {\n''',
    1,
)
write(placement_path, placement)

# Fill the new field in every FrameRequest literal, including worker tests.
for path in Path('crates/daemon/src').rglob('*.rs'):
    text = path.read_text(encoding='utf-8')
    pattern = re.compile(
        r'(FrameRequest\s*\{\s*\n\s*placements:\s*[^\n]+,\s*\n)(?!\s*region_clips:)'
    )
    updated, count = pattern.subn(r'\1            region_clips: Vec::new(),\n', text)
    if count:
        path.write_text(updated, encoding='utf-8', newline='\n')

print('v13 post-patch hardening applied')
