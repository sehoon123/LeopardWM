
#[cfg(test)]
mod win32_integration_tests {
    use super::{
        actual_region_matches, apply_window_region_clip, can_clip_window_region,
        forget_window_region, restore_window_region, RegionClipResult,
    };
    use leopardwm_core_layout::Rect;
    use std::ffi::c_void;
    use std::sync::{Mutex, OnceLock};
    use windows::core::w;
    use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Gdi::CreateRectRgn;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, SetWindowRgn,
        WINDOW_EX_STYLE, WM_NCDESTROY, WNDCLASSW, WS_OVERLAPPED,
    };

    static TEST_LOCK: Mutex<()> = Mutex::new(());
    static REGISTERED: OnceLock<()> = OnceLock::new();

    unsafe extern "system" fn test_wndproc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_NCDESTROY {
            forget_window_region(hwnd.0 as usize as u64);
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }

    fn create_test_window() -> HWND {
        let module = unsafe { GetModuleHandleW(None) }.unwrap();
        let instance = HINSTANCE(module.0);
        REGISTERED.get_or_init(|| {
            let class = WNDCLASSW {
                lpfnWndProc: Some(test_wndproc),
                hInstance: instance,
                lpszClassName: w!("LeopardWM.RegionClip.Integration"),
                ..Default::default()
            };
            let atom = unsafe { RegisterClassW(&class) };
            assert_ne!(atom, 0, "failed to register region test window class");
        });
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("LeopardWM.RegionClip.Integration"),
                w!("LeopardWM region integration test"),
                WS_OVERLAPPED,
                0,
                0,
                400,
                300,
                None,
                None,
                Some(instance),
                None::<*const c_void>,
            )
        }
        .unwrap()
    }

    struct TestWindow(HWND);

    impl Drop for TestWindow {
        fn drop(&mut self) {
            let window_id = self.0 .0 as usize as u64;
            let _ = restore_window_region(window_id, false);
            unsafe {
                let _ = DestroyWindow(self.0);
            }
            forget_window_region(window_id);
        }
    }

    #[test]
    fn installs_updates_and_restores_a_real_hwnd_region() {
        let _guard = TEST_LOCK.lock().unwrap();
        let window = TestWindow(create_test_window());
        let window_id = window.0 .0 as usize as u64;

        assert!(can_clip_window_region(window_id));
        assert_eq!(
            apply_window_region_clip(
                window_id,
                Rect::new(0, 0, 400, 300),
                Rect::new(0, 0, 400, 300),
                Rect::new(0, 0, 200, 300),
                false,
            ),
            RegionClipResult::Applied
        );
        assert!(actual_region_matches(window.0, Rect::new(0, 0, 200, 300)));

        assert!(apply_window_region_clip(
            window_id,
            Rect::new(0, 0, 400, 300),
            Rect::new(0, 0, 400, 300),
            Rect::new(100, 0, 300, 300),
            false,
        )
        .succeeded());
        assert!(actual_region_matches(window.0, Rect::new(100, 0, 200, 300)));

        assert!(restore_window_region(window_id, false));
        assert!(can_clip_window_region(window_id));
    }

    #[test]
    fn never_overwrites_or_clears_an_application_region() {
        let _guard = TEST_LOCK.lock().unwrap();
        let window = TestWindow(create_test_window());
        let window_id = window.0 .0 as usize as u64;

        let application_region = unsafe { CreateRectRgn(10, 10, 300, 250) }.unwrap();
        assert_ne!(unsafe { SetWindowRgn(window.0, Some(application_region), false) }, 0);
        assert!(!can_clip_window_region(window_id));
        assert!(actual_region_matches(window.0, Rect::new(10, 10, 290, 240)));
        assert!(restore_window_region(window_id, false));
        assert!(actual_region_matches(window.0, Rect::new(10, 10, 290, 240)));
        assert_ne!(unsafe { SetWindowRgn(window.0, None, false) }, 0);
    }

    #[test]
    fn application_takeover_after_clipping_is_preserved() {
        let _guard = TEST_LOCK.lock().unwrap();
        let window = TestWindow(create_test_window());
        let window_id = window.0 .0 as usize as u64;

        assert!(apply_window_region_clip(
            window_id,
            Rect::new(0, 0, 400, 300),
            Rect::new(0, 0, 400, 300),
            Rect::new(0, 0, 200, 300),
            false,
        )
        .succeeded());

        let application_region = unsafe { CreateRectRgn(20, 20, 280, 220) }.unwrap();
        assert_ne!(unsafe { SetWindowRgn(window.0, Some(application_region), false) }, 0);
        assert!(restore_window_region(window_id, false));
        assert!(actual_region_matches(window.0, Rect::new(20, 20, 260, 200)));
        assert_ne!(unsafe { SetWindowRgn(window.0, None, false) }, 0);
    }
}
