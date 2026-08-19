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
        raise RuntimeError(f'{path}: expected one lifecycle target, found {count}: {old[:160]!r}')
    write(path, text.replace(old, new))


# ---------------------------------------------------------------------------
# Region ownership and recovery hardening.
# ---------------------------------------------------------------------------
region_path = 'crates/platform_win32/src/window_region.rs'
region = read(region_path)
old_alive = '''fn owner_process_is_alive(owner: OwnerToken) -> bool {\n    let current = current_owner();\n    if owner == current {\n        return true;\n    }\n    let Ok(process) = (unsafe {\n        OpenProcess(\n            PROCESS_QUERY_LIMITED_INFORMATION,\n            false,\n            owner.process_id,\n        )\n    }) else {\n        // Access denial is not proof that the owner is dead. Fail closed.\n        return true;\n    };\n    let creation_matches = process_creation_time(process) == Some(owner.creation_time);\n    unsafe {\n        let _ = CloseHandle(process);\n    }\n    creation_matches\n}\n'''
new_alive = '''fn owner_process_is_alive(owner: OwnerToken) -> bool {\n    const HRESULT_INVALID_PARAMETER: i32 = 0x8007_0057u32 as i32;\n\n    let current = current_owner();\n    if owner == current {\n        return true;\n    }\n    // A zero creation time is an ambiguous token produced only when the OS\n    // refused GetProcessTimes. Never let another process clear it.\n    if owner.creation_time == 0 {\n        return true;\n    }\n    let process = match unsafe {\n        OpenProcess(\n            PROCESS_QUERY_LIMITED_INFORMATION,\n            false,\n            owner.process_id,\n        )\n    } {\n        Ok(process) => process,\n        Err(error) => {\n            // OpenProcess reports ERROR_INVALID_PARAMETER when the PID no\n            // longer exists. Access denial and unknown failures are not proof\n            // of death, so preserve the other process's region.\n            return error.code().0 != HRESULT_INVALID_PARAMETER;\n        }\n    };\n    let creation_matches = process_creation_time(process) == Some(owner.creation_time);\n    unsafe {\n        let _ = CloseHandle(process);\n    }\n    creation_matches\n}\n'''
if region.count(old_alive) != 1:
    raise RuntimeError('window_region.rs: owner liveness block mismatch')
region = region.replace(old_alive, new_alive)
region = region.replace(
    '''fn clear_region(hwnd: HWND, redraw: bool) -> bool {\n    unsafe { SetWindowRgn(hwnd, None, redraw) != 0 }\n}\n''',
    '''fn clear_region(hwnd: HWND, redraw: bool) -> bool {\n    if unsafe { SetWindowRgn(hwnd, None, redraw) } != 0 {\n        return true;\n    }\n    // SetWindowRgn can transiently fail while the target is processing a\n    // simultaneous non-client update. One yield/retry avoids stranding an\n    // owned clip without introducing an unbounded wait.\n    std::thread::yield_now();\n    unsafe { SetWindowRgn(hwnd, None, redraw) != 0 }\n}\n''',
    1,
)
region = region.replace(
    '''pub fn forget_window_region(window_id: WindowId) {\n    lock_states().remove(&window_id);\n}\n''',
    '''pub fn forget_window_region(window_id: WindowId) {\n    let _commit = lock_commit();\n    lock_states().remove(&window_id);\n}\n''',
    1,
)
write(region_path, region)

# ---------------------------------------------------------------------------
# Never leave a target-shape region behind when its SetWindowPos failed.
# ---------------------------------------------------------------------------
placement_path = 'crates/platform_win32/src/placement.rs'
placement = read(placement_path)
old_call = '''    let (applied, mut failed_window_ids) = position_entries(&entries);\n    region_fallbacks += confirm_entry_region_clips(\n'''
new_call = '''    let (applied, mut failed_window_ids) = position_entries(&entries);\n    clear_regions_for_failed_positions(&mut entries, &failed_window_ids);\n    region_fallbacks += confirm_entry_region_clips(\n'''
if placement.count(old_call) != 1:
    raise RuntimeError('placement.rs: failed-position cleanup call marker mismatch')
placement = placement.replace(old_call, new_call)
helper_marker = '''/// Revalidate after movement to catch an application replacing the region in\n'''
helper = '''fn clear_regions_for_failed_positions(\n    entries: &mut [DeferEntry],\n    failed_window_ids: &HashSet<u64>,\n) {\n    for entry in entries {\n        if entry.region_clip_bounds.is_some()\n            && failed_window_ids.contains(&entry.window_id)\n        {\n            let _ = restore_window_region(entry.window_id, false);\n            entry.region_clip_bounds = None;\n        }\n    }\n}\n\n'''
if placement.count(helper_marker) != 1:
    raise RuntimeError('placement.rs: failed-region helper marker mismatch')
placement = placement.replace(helper_marker, helper + helper_marker)
write(placement_path, placement)

# ---------------------------------------------------------------------------
# Recover regions abandoned by a crashed predecessor before enumeration. The\n# owner token prevents a second live daemon from touching the first daemon.\n# ---------------------------------------------------------------------------
main_path = 'crates/daemon/src/main.rs'
main = read(main_path)
pattern = re.compile(r'(?m)^(fn main\s*\(\s*\)\s*(?:->\s*[^\{\n]+)?\{\s*\n)')
main, count = pattern.subn(
    r'\1    leopardwm_platform_win32::restore_all_window_regions();\n',
    main,
    count=1,
)
if count != 1:
    raise RuntimeError('main.rs: main function marker mismatch')
write(main_path, main)

print('v13 lifecycle hardening applied')
