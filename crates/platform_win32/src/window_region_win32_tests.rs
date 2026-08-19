use super::*;
use std::sync::Mutex;
use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateRectRgn, DeleteObject, EqualRgn, HGDIOBJ};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetWindowRgn, RegisterClassW, SetWindowRgn,
    WINDOW_EX_STYLE, WNDCLASSW, WS_OVERLAPPED,
};

static TEST_SERIAL: Mutex<()> = Mutex::new(());

unsafe extern "system" fn test_wnd_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn test_instance() -> HINSTANCE {
    static INSTANCE: std::sync::OnceLock<isize> = std::sync::OnceLock::new();
    HINSTANCE(*INSTANCE.get_or_init(|| unsafe {
        let module = GetModuleHandleW(None).expect("module handle");
        let instance = HINSTANCE(module.0);
        let class = WNDCLASSW {
            lpfnWndProc: Some(test_wnd_proc),
            hInstance: instance,
            lpszClassName: w!("LeopardWM.RegionClip.Tests.v3"),
            ..Default::default()
        };
        let atom = RegisterClassW(&class);
        assert_ne!(atom, 0, "register test window class");
        instance.0 as isize
    }) as *mut core::ffi::c_void)
}

struct TestWindow(HWND);

impl TestWindow {
    fn new() -> Self {
        let instance = test_instance();
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("LeopardWM.RegionClip.Tests.v3"),
                w!("region-test"),
                WS_OVERLAPPED,
                0,
                0,
                400,
                300,
                None,
                None,
                Some(instance),
                None,
            )
            .expect("create hidden test window")
        };
        Self(hwnd)
    }

    fn id(&self) -> WindowId {
        self.0 .0 as usize as WindowId
    }
}

impl Drop for TestWindow {
    fn drop(&mut self) {
        let _ = restore_window_region(self.id(), false);
        unsafe {
            let _ = SetWindowRgn(self.0, None, false);
            let _ = DestroyWindow(self.0);
        }
        forget_window_region(self.id());
    }
}

fn current_region_equals(hwnd: HWND, expected: Rect) -> bool {
    let actual = unsafe { CreateRectRgn(0, 0, 1, 1) }.expect("actual region");
    let kind = unsafe { GetWindowRgn(hwnd, actual) };
    assert!(kind > NULL_REGION_KIND, "window has no active region");
    let wanted = unsafe {
        CreateRectRgn(expected.x, expected.y, expected.right(), expected.bottom())
    }
    .expect("expected region");
    let equal = unsafe { EqualRgn(actual, wanted).as_bool() };
    unsafe {
        let _ = DeleteObject(HGDIOBJ(actual.0));
        let _ = DeleteObject(HGDIOBJ(wanted.0));
    }
    equal
}

fn has_no_region(hwnd: HWND) -> bool {
    let probe = unsafe { CreateRectRgn(0, 0, 1, 1) }.expect("probe region");
    let kind = unsafe { GetWindowRgn(hwnd, probe) };
    unsafe {
        let _ = DeleteObject(HGDIOBJ(probe.0));
    }
    kind == NULL_REGION_KIND
}

#[test]
fn applies_updates_and_restores_an_owned_region() {
    let _serial = TEST_SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let window = TestWindow::new();
    let id = window.id();
    let outer = Rect::new(0, 0, 400, 300);
    let visible = Rect::new(0, 0, 400, 300);

    assert_eq!(
        apply_window_region_clip(id, outer, visible, Rect::new(0, 0, 200, 300), false),
        RegionClipResult::Applied
    );
    assert!(current_region_equals(window.0, Rect::new(0, 0, 200, 300)));
    assert_eq!(
        apply_window_region_clip(id, outer, visible, Rect::new(0, 0, 200, 300), false),
        RegionClipResult::Unchanged
    );
    assert_eq!(
        apply_window_region_clip(id, outer, visible, Rect::new(100, 0, 300, 300), false),
        RegionClipResult::Applied
    );
    assert!(current_region_equals(window.0, Rect::new(100, 0, 300, 300)));

    assert!(restore_window_region(id, false));
    assert!(has_no_region(window.0));
}

#[test]
fn refuses_to_overwrite_an_application_region() {
    let _serial = TEST_SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let window = TestWindow::new();
    let app_region = Rect::new(20, 10, 350, 260);
    let region = unsafe {
        CreateRectRgn(
            app_region.x,
            app_region.y,
            app_region.right(),
            app_region.bottom(),
        )
    }
    .expect("application region");
    assert_ne!(unsafe { SetWindowRgn(window.0, Some(region), false) }, 0);

    assert!(!can_clip_window_region(window.id()));
    assert_eq!(
        apply_window_region_clip(
            window.id(),
            Rect::new(0, 0, 400, 300),
            Rect::new(0, 0, 400, 300),
            Rect::new(0, 0, 200, 300),
            false,
        ),
        RegionClipResult::Unsupported
    );
    assert!(current_region_equals(window.0, app_region));
}

#[test]
fn never_clears_a_region_replaced_by_the_application() {
    let _serial = TEST_SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let window = TestWindow::new();
    let id = window.id();
    assert!(apply_window_region_clip(
        id,
        Rect::new(0, 0, 400, 300),
        Rect::new(0, 0, 400, 300),
        Rect::new(0, 0, 200, 300),
        false,
    )
    .succeeded());

    let app_region = Rect::new(25, 15, 325, 250);
    let replacement = unsafe {
        CreateRectRgn(
            app_region.x,
            app_region.y,
            app_region.right(),
            app_region.bottom(),
        )
    }
    .expect("replacement region");
    assert_ne!(unsafe { SetWindowRgn(window.0, Some(replacement), false) }, 0);

    assert!(restore_window_region(id, false));
    assert!(current_region_equals(window.0, app_region));
}

#[test]
fn recovers_a_stale_region_after_process_state_is_lost() {
    let _serial = TEST_SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let window = TestWindow::new();
    let id = window.id();
    assert!(apply_window_region_clip(
        id,
        Rect::new(0, 0, 400, 300),
        Rect::new(0, 0, 400, 300),
        Rect::new(0, 0, 200, 300),
        false,
    )
    .succeeded());

    // Simulate a new daemon process: in-memory state is gone, but the HWND
    // properties and the physical region remain.
    forget_window_region(id);
    assert!(can_clip_window_region(id));
    assert!(has_no_region(window.0));
}
