from pathlib import Path

path = Path("crates/platform_win32/src/window_region.rs")
text = path.read_text(encoding="utf-8")
text = text.replace("    use std::ffi::c_void;\n", "", 1)
text = text.replace(
    "    use windows::Win32::Graphics::Gdi::{CreateRectRgn, DeleteObject, HGDIOBJ};\n",
    "    use windows::Win32::Graphics::Gdi::CreateRectRgn;\n",
    1,
)
path.write_text(text, encoding="utf-8", newline="\n")
print("region integration test imports normalized")
