from pathlib import Path


def read(path: str) -> str:
    return Path(path).read_text(encoding='utf-8')


def write(path: str, text: str) -> None:
    Path(path).write_text(text, encoding='utf-8', newline='\n')


def replace_once(path: str, old: str, new: str) -> None:
    text = read(path)
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f'{path}: expected one optimization target, found {count}: {old[:140]!r}')
    write(path, text.replace(old, new))


region_path = 'crates/platform_win32/src/window_region.rs'
region = read(region_path)
region = region.replace(
    'use leopardwm_core_layout::{Rect, Visibility, WindowId};',
    'use leopardwm_core_layout::{Rect, Visibility, WindowId, WindowPlacement};',
    1,
)
old_reconcile = '''pub(crate) fn reconcile_window_regions(\n    managed_window_ids: &HashSet<WindowId>,\n    clipped_window_ids: &HashSet<WindowId>,\n    redraw: bool,\n) {\n    for window_id in managed_window_ids.difference(clipped_window_ids) {\n        let _ = restore_window_region(*window_id, redraw);\n    }\n    let stale: Vec<WindowId> = lock_states()\n        .keys()\n        .filter(|window_id| !managed_window_ids.contains(window_id))\n        .copied()\n        .collect();\n    for window_id in stale {\n        let _ = restore_window_region(window_id, redraw);\n    }\n}\n'''
new_reconcile = '''pub(crate) fn reconcile_window_regions(\n    placements: &[WindowPlacement],\n    clips: &[WindowRegionClip],\n    redraw: bool,\n) {\n    for placement in placements {\n        if !clips.iter().any(|clip| clip.window_id == placement.window_id) {\n            let _ = restore_window_region(placement.window_id, redraw);\n        }\n    }\n    let stale: Vec<WindowId> = lock_states()\n        .keys()\n        .filter(|window_id| {\n            !placements\n                .iter()\n                .any(|placement| placement.window_id == **window_id)\n        })\n        .copied()\n        .collect();\n    for window_id in stale {\n        let _ = restore_window_region(window_id, redraw);\n    }\n}\n'''
if region.count(old_reconcile) != 1:
    raise RuntimeError('window_region.rs: reconcile block mismatch')
region = region.replace(old_reconcile, new_reconcile)
write(region_path, region)

placement_path = 'crates/platform_win32/src/placement.rs'
placement = read(placement_path)
placement = placement.replace(
    '''    apply_window_region_clip, can_clip_window_region, reconcile_window_regions,\n''',
    '''    apply_window_region_clip, reconcile_window_regions,\n''',
    1,
)
old_sets = '''    let managed_window_ids: HashSet<WindowId> =\n        placements.iter().map(|placement| placement.window_id).collect();\n    let clipped_window_ids: HashSet<WindowId> =\n        region_clips.iter().map(|clip| clip.window_id).collect();\n    reconcile_window_regions(\n        &managed_window_ids,\n        &clipped_window_ids,\n        !animation_frame,\n    );\n'''
new_sets = '''    reconcile_window_regions(placements, region_clips, !animation_frame);\n'''
if placement.count(old_sets) != 1:
    raise RuntimeError('placement.rs: reconciliation allocation block mismatch')
placement = placement.replace(old_sets, new_sets)
old_preflight = '''        let region_clip = region_clips\n            .iter()\n            .find(|clip| clip.window_id == requested.window_id);\n        let clip_supported = region_clip\n            .is_some_and(|_| can_clip_window_region(requested.window_id));\n        let placement = if let Some(clip) = region_clip.filter(|_| !clip_supported) {\n            WindowPlacement {\n                window_id: requested.window_id,\n                rect: clip.fallback_rect,\n                visibility: clip.fallback_visibility,\n                column_index: requested.column_index,\n            }\n        } else {\n            requested.clone()\n        };\n'''
new_preflight = '''        let region_clip = region_clips\n            .iter()\n            .find(|clip| clip.window_id == requested.window_id);\n        // Ownership is checked exactly once in the pre-move commit stage. If\n        // unsupported, that stage converts this entry to its safe fallback\n        // before uncloak or positioning.\n        let placement = requested.clone();\n'''
if placement.count(old_preflight) != 1:
    raise RuntimeError('placement.rs: region preflight block mismatch')
placement = placement.replace(old_preflight, new_preflight)
placement = placement.replace(
    '''                region_clip_bounds: region_clip\n                    .filter(|_| clip_supported)\n                    .map(|clip| clip.clip_bounds),\n''',
    '''                region_clip_bounds: region_clip.map(|clip| clip.clip_bounds),\n''',
)
write(placement_path, placement)

print('v13 allocation and preflight optimization applied')
