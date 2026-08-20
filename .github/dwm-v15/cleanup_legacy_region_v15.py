from pathlib import Path

ROOT = Path('.')
THUMBNAIL = ROOT / 'crates/platform_win32/src/thumbnail.rs'
REGION = ROOT / 'crates/platform_win32/src/window_region.rs'


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


# Modern Notepad is a compositor-sensitive WinUI window. The production
# predicate was intentionally extended by the proxy patch; keep its frozen
# classification test aligned with that contract.
replace_once(
    THUMBNAIL,
    '        assert!(!is_ghost_animation_class_str("Notepad"));\n',
    '        assert!(is_ghost_animation_class_str("Notepad"));\n',
)

# The DWM thumbnail proxy no longer installs new SetWindowRgn clips in
# production. Retain the old geometry and ownership machinery only in tests so
# we preserve migration/recovery coverage without shipping dead hot-path code.
replace_once(
    REGION,
    'use std::ffi::c_void;\n',
    '#[cfg(test)]\nuse std::ffi::c_void;\n',
)
replace_once(
    REGION,
    'use windows::Win32::Foundation::{HANDLE, HWND, RECT};\n',
    'use windows::Win32::Foundation::{HANDLE, HWND};\n#[cfg(test)]\nuse windows::Win32::Foundation::RECT;\n',
)
replace_once(
    REGION,
    'use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};\n',
    '#[cfg(test)]\nuse windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};\n',
)
replace_once(
    REGION,
    '''use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetPropW, GetWindowRect, GetWindowThreadProcessId, IsWindow, RemovePropW,
    SetPropW,
};
''',
    '''use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetPropW, GetWindowThreadProcessId, IsWindow, RemovePropW,
};
#[cfg(test)]
use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, SetPropW};
''',
)

replace_once(
    REGION,
    '#[derive(Debug, Clone, Copy, PartialEq, Eq)]\npub(crate) enum RegionClipResult {\n',
    '#[cfg(test)]\n#[derive(Debug, Clone, Copy, PartialEq, Eq)]\npub(crate) enum RegionClipResult {\n',
)
replace_once(
    REGION,
    'impl RegionClipResult {\n',
    '#[cfg(test)]\nimpl RegionClipResult {\n',
)

for signature in [
    'fn handle_from_usize(value: usize) -> HANDLE {',
    'fn encode_coordinate(value: i32) -> HANDLE {',
    'fn write_metadata(hwnd: HWND, rect: Rect) -> bool {',
    'fn current_region_kind(hwnd: HWND) -> Option<WindowRegionKind> {',
    'fn rect_from_win32(rect: RECT) -> Option<Rect> {',
    'fn current_window_geometry(hwnd: HWND) -> Option<(Rect, Rect)> {',
    'fn intersect_regions(left: Rect, right: Rect) -> Rect {',
    'fn allowed_region(outer_rect: Rect, visible_rect: Rect, clip_bounds: Rect) -> Rect {',
    'pub(crate) fn bridge_clip_region(',
    'fn install_owned_region_locked(',
    'fn owned_region_for_identity(',
    'pub(crate) fn prepare_window_region_clip(',
    'pub(crate) fn relative_clip_region(',
    'pub(crate) fn apply_window_region_clip(',
]:
    replace_once(REGION, signature, '#[cfg(test)]\n' + signature)

print('Legacy SetWindowRgn backend retired from the production target')
