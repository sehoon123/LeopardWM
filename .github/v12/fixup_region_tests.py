from pathlib import Path

path = Path("crates/platform_win32/src/window_region.rs")
text = path.read_text(encoding="utf-8")
marker = "mod win32_integration_tests"
if marker in text:
    raise RuntimeError("Win32 region integration tests already installed")

text += r'''

#[cfg(test)]
mod win32_integration_tests {
    use super::{
        apply_window_region_clip, can_clip_window_region, restore_window_region,
    };
    use leopardwm_core_layout::Rect;
    use std::ffi::c_void;
    use windows::core::w;
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Gdi::{CreateRectRgn, DeleteObject, HGDIOBJ};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, GetWindowRgnBox, SetWindowRgn, WINDOW_EX_STYLE,
        WS_OVERLAPPED,
    };

    fn test_window() -> windows::Win32::Foundation::HWND {
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!("LeopardWM region test"),
                WS_OVERLAPPED,
                0,
                0,
                400,
                300,
                None,
                None,
                None,
                None,
            )
            .expect("create test HWND")
        }
    }

    fn window_id(hwnd: windows::Win32::Foundation::HWND) -> u64 {
        hwnd.0 as usize as u64
    }

    #[test]
    fn installs_and_restores_region_on_a_real_top_level_hwnd() {
        let hwnd = test_window();
        let id = window_id(hwnd);
        assert!(can_clip_window_region(id));
        assert!(apply_window_region_clip(
            id,
            Rect::new(0, 0, 400, 300),
            Rect::new(0, 0, 220, 300),
            false,
        ));

        let mut bounds = RECT::default();
        assert_eq!(unsafe { GetWindowRgnBox(hwnd, &mut bounds) }, 2);
        assert_eq!((bounds.left, bounds.top, bounds.right, bounds.bottom), (0, 0, 220, 300));
        assert!(restore_window_region(id, false));
        assert_eq!(unsafe { GetWindowRgnBox(hwnd, &mut bounds) }, 1);
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }

    #[test]
    fn refuses_to_overwrite_an_application_region() {
        let hwnd = test_window();
        let id = window_id(hwnd);
        let region = unsafe { CreateRectRgn(10, 0, 180, 300) }.unwrap();
        assert_ne!(unsafe { SetWindowRgn(hwnd, Some(region), false) }, 0);
        assert!(!can_clip_window_region(id));

        let mut bounds = RECT::default();
        assert_eq!(unsafe { GetWindowRgnBox(hwnd, &mut bounds) }, 2);
        assert_eq!((bounds.left, bounds.right), (10, 180));
        assert_ne!(unsafe { SetWindowRgn(hwnd, None, false) }, 0);
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }

    #[test]
    fn preserves_a_region_the_application_installs_after_ours() {
        let hwnd = test_window();
        let id = window_id(hwnd);
        assert!(apply_window_region_clip(
            id,
            Rect::new(0, 0, 400, 300),
            Rect::new(0, 0, 220, 300),
            false,
        ));

        let replacement = unsafe { CreateRectRgn(30, 0, 190, 300) }.unwrap();
        assert_ne!(unsafe { SetWindowRgn(hwnd, Some(replacement), false) }, 0);
        assert!(restore_window_region(id, false));

        let mut bounds = RECT::default();
        assert_eq!(unsafe { GetWindowRgnBox(hwnd, &mut bounds) }, 2);
        assert_eq!((bounds.left, bounds.right), (30, 190));
        assert_ne!(unsafe { SetWindowRgn(hwnd, None, false) }, 0);
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
}
'''

path.write_text(text, encoding="utf-8", newline="\n")
print("real HWND region ownership tests installed")
