from pathlib import Path

path = Path("crates/platform_win32/src/window_region.rs")
text = path.read_text(encoding="utf-8")
old = """        apply_window_region_clip, can_clip_window_region, restore_window_region,
"""
new = """        apply_window_region_clip, can_clip_window_region, forget_window_region,
        restore_window_region,
"""
if text.count(old) != 1:
    raise RuntimeError("region test import marker mismatch")
text = text.replace(old, new)
# Every integration test has one final DestroyWindow block. Clear global test
# bookkeeping first so rapid HWND reuse cannot inherit a same-process entry.
needle = """        unsafe {
            let _ = DestroyWindow(hwnd);
        }
"""
if text.count(needle) != 3:
    raise RuntimeError("unexpected integration-test cleanup count")
replacement = """        forget_window_region(id);
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
"""
text = text.replace(needle, replacement)
path.write_text(text, encoding="utf-8", newline="\n")
print("region integration test bookkeeping cleanup installed")
