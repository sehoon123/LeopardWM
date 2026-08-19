from pathlib import Path


def read(path: str) -> str:
    return Path(path).read_text(encoding='utf-8')


def write(path: str, text: str) -> None:
    Path(path).write_text(text, encoding='utf-8', newline='\n')


path = 'crates/platform_win32/src/window_region.rs'
text = read(path)
text = text.replace(
    '''struct RegionState {\n    identity: WindowIdentity,\n    expected_region: Rect,\n}\n\n#[derive(Debug, Clone, Copy)]\nenum MetadataSlot {\n''',
    '''struct RegionState {\n    identity: WindowIdentity,\n    expected_region: Rect,\n    current_slot: MetadataSlot,\n}\n\n#[derive(Debug, Clone, Copy, PartialEq, Eq)]\nenum MetadataSlot {\n''',
    1,
)
old_install = '''    if !ensure_current_owner(hwnd)\n        || !write_slot(hwnd, MetadataSlot::Pending, expected_region)\n    {\n        delete_region(new_region);\n        return RegionClipResult::Failed;\n    }\n    if unsafe { SetWindowRgn(hwnd, Some(new_region), redraw) } == 0 {\n        delete_region(new_region);\n        remove_slot(hwnd, MetadataSlot::Pending);\n        if read_slot(hwnd, MetadataSlot::Active).is_none() {\n            remove_all_metadata(hwnd);\n        }\n        return RegionClipResult::Failed;\n    }\n    // On success Windows owns `new_region`.\n    let region_kind = current_region_kind(hwnd);\n    if region_kind != ERROR_REGION_KIND && !actual_region_matches(hwnd, expected_region) {\n        // The application replaced the region concurrently. Relinquish only\n        // LeopardWM metadata; never clear the application's replacement.\n        remove_all_metadata(hwnd);\n        lock_states().remove(&window_id);\n        return RegionClipResult::Unsupported;\n    }\n\n    if write_slot(hwnd, MetadataSlot::Active, expected_region) {\n        remove_slot(hwnd, MetadataSlot::Pending);\n    }\n    // If promotion fails, the valid pending slot still describes the live\n    // region and remains sufficient for crash recovery.\n    lock_states().insert(\n        window_id,\n        RegionState {\n            identity: current_identity,\n            expected_region,\n        },\n    );\n'''
new_install = '''    // Alternate between two durable slots. The current slot describes the live\n    // region while the other slot is journaled with the next shape. A crash\n    // before or after SetWindowRgn therefore leaves at least one exact match,\n    // without rewriting both slots on every animation frame.\n    let next_slot = old_state\n        .as_ref()\n        .filter(|_| old_owned)\n        .map_or(MetadataSlot::Active, |state| match state.current_slot {\n            MetadataSlot::Active => MetadataSlot::Pending,\n            MetadataSlot::Pending => MetadataSlot::Active,\n        });\n    if !ensure_current_owner(hwnd) || !write_slot(hwnd, next_slot, expected_region) {\n        delete_region(new_region);\n        return RegionClipResult::Failed;\n    }\n    if unsafe { SetWindowRgn(hwnd, Some(new_region), redraw) } == 0 {\n        delete_region(new_region);\n        remove_slot(hwnd, next_slot);\n        if metadata_candidates(hwnd).is_empty() {\n            remove_all_metadata(hwnd);\n        }\n        return RegionClipResult::Failed;\n    }\n    // On success Windows owns `new_region`.\n    let region_kind = current_region_kind(hwnd);\n    if region_kind != ERROR_REGION_KIND && !actual_region_matches(hwnd, expected_region) {\n        // The application replaced the region concurrently. Relinquish only\n        // LeopardWM metadata; never clear the application's replacement.\n        remove_all_metadata(hwnd);\n        lock_states().remove(&window_id);\n        return RegionClipResult::Unsupported;\n    }\n\n    lock_states().insert(\n        window_id,\n        RegionState {\n            identity: current_identity,\n            expected_region,\n            current_slot: next_slot,\n        },\n    );\n'''
if text.count(old_install) != 1:
    raise RuntimeError('window_region.rs: journal installation block mismatch')
text = text.replace(old_install, new_install)
write(path, text)
print('v13 alternating metadata journal applied')
