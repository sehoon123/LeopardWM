#![cfg(windows)]

use leopardwm_core_layout::{Rect, Visibility, WindowPlacement};
use leopardwm_platform_win32::{
    apply_placements_with_regions, apply_placements_with_regions_fenced, PlatformConfig,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, FindWindowW, GetMessageW,
    IsWindow, PostThreadMessageW, RegisterClassW, UnregisterClassW, MSG, WINDOWPOS, WM_QUIT,
    WM_WINDOWPOSCHANGING, WNDCLASSW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

static FORCE_STAY_AT_SENTINEL: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn return_rejecting_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_WINDOWPOSCHANGING {
        let position = &mut *(lparam.0 as *mut WINDOWPOS);
        let is_sentinel = position.x <= -10_000 && position.y <= -10_000;
        if !is_sentinel {
            if FORCE_STAY_AT_SENTINEL.load(Ordering::Acquire) {
                position.x = -32_768;
                position.y = -32_768;
            } else {
                position.x = 100;
                position.y = 100;
            }
        }
    }
    DefWindowProcW(hwnd, message, wparam, lparam)
}

struct SourceWindow {
    hwnd: isize,
    thread_id: u32,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SourceWindow {
    fn new() -> Self {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(isize, u32), String>>();
        let thread = std::thread::Builder::new()
            .name("floating-return-integration-source".into())
            .spawn(move || unsafe {
                let class: Vec<u16> = format!(
                    "LeopardWMFloatingReturnSource-{}-{}\0",
                    std::process::id(),
                    GetCurrentThreadId()
                )
                .encode_utf16()
                .collect();
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(return_rejecting_window_proc),
                    lpszClassName: PCWSTR(class.as_ptr()),
                    ..Default::default()
                };
                if RegisterClassW(&wc) == 0 {
                    let _ = ready_tx.send(Err(format!(
                        "RegisterClassW failed: {}",
                        windows::core::Error::from_thread()
                    )));
                    return;
                }
                let title: Vec<u16> = "LeopardWM floating return source\0"
                    .encode_utf16()
                    .collect();
                let hwnd = match CreateWindowExW(
                    Default::default(),
                    PCWSTR(class.as_ptr()),
                    PCWSTR(title.as_ptr()),
                    WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                    100,
                    100,
                    640,
                    480,
                    None,
                    None,
                    None,
                    None,
                ) {
                    Ok(hwnd) => hwnd,
                    Err(error) => {
                        let _ = ready_tx.send(Err(format!("CreateWindowExW failed: {error}")));
                        let _ = UnregisterClassW(PCWSTR(class.as_ptr()), None);
                        return;
                    }
                };
                let thread_id = GetCurrentThreadId();
                if ready_tx.send(Ok((hwnd.0 as isize, thread_id))).is_err() {
                    let _ = DestroyWindow(hwnd);
                    let _ = UnregisterClassW(PCWSTR(class.as_ptr()), None);
                    return;
                }
                let mut message = MSG::default();
                while GetMessageW(&mut message, None, 0, 0).0 > 0 {
                    let _ = DispatchMessageW(&message);
                }
                if IsWindow(Some(hwnd)).as_bool() {
                    let _ = DestroyWindow(hwnd);
                }
                let _ = UnregisterClassW(PCWSTR(class.as_ptr()), None);
            })
            .expect("source thread must spawn");
        let (hwnd, thread_id) = ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("source readiness timed out")
            .expect("source window creation failed");
        Self {
            hwnd,
            thread_id,
            thread: Some(thread),
        }
    }
}

impl Drop for SourceWindow {
    fn drop(&mut self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn assert_daemon_absent() {
    let daemon_class: Vec<u16> = "LeopardWMSysEventClass\0".encode_utf16().collect();
    assert!(
        unsafe { FindWindowW(PCWSTR(daemon_class.as_ptr()), PCWSTR::null()) }.is_err(),
        "stop LeopardWM before running owned-HWND probes; a live daemon would manage and corrupt probe windows"
    );
}

#[test]
fn controlled_visible_float_return_contract() {
    assert!(leopardwm_platform_win32::set_dpi_awareness());
    assert_daemon_absent();
    let monitor = leopardwm_platform_win32::enumerate_monitors()
        .expect("monitor enumeration")
        .into_iter()
        .find(|monitor| monitor.is_primary)
        .expect("primary monitor");
    let source = SourceWindow::new();
    let window_id = source.hwnd as u64;

    let before_invalid_batch = leopardwm_platform_win32::get_window_chrome_rect(window_id)
        .expect("initial source geometry");
    let invalid_batch = apply_placements_with_regions(
        &[
            WindowPlacement {
                window_id,
                rect: Rect::new(700, 300, 320, 240),
                visibility: Visibility::Visible,
                column_index: usize::MAX,
            },
            WindowPlacement {
                window_id: 0,
                rect: Rect::new(0, 0, 100, 100),
                visibility: Visibility::Visible,
                column_index: 0,
            },
        ],
        &[],
        &PlatformConfig::default(),
        None,
        false,
    );
    assert!(invalid_batch.is_err());
    assert_eq!(
        leopardwm_platform_win32::get_window_chrome_rect(window_id),
        Some(before_invalid_batch),
        "invalid sibling must be rejected before mutating a valid HWND"
    );

    let mut stale_identity =
        leopardwm_platform_win32::current_window_event_identity(window_id).unwrap();
    stale_identity.token ^= 1;
    let stale_batch = apply_placements_with_regions_fenced(
        &[WindowPlacement {
            window_id,
            rect: Rect::new(900, 350, 300, 220),
            visibility: Visibility::Visible,
            column_index: usize::MAX,
        }],
        &[],
        &HashMap::from([(window_id, stale_identity)]),
        &PlatformConfig::default(),
        None,
        false,
    );
    assert!(stale_batch.is_err());
    assert_eq!(
        leopardwm_platform_win32::get_window_chrome_rect(window_id),
        Some(before_invalid_batch),
        "stale placement identity must not mutate a replacement HWND"
    );

    leopardwm_platform_win32::move_window_offscreen(window_id)
        .expect("return-rejecting source must accept sentinel park");
    assert!(leopardwm_platform_win32::has_move_offscreen_ownership(
        window_id
    ));
    let requested = Rect::new(
        monitor.work_area.x + 700,
        monitor.work_area.y + 300,
        320,
        240,
    );
    assert!(leopardwm_platform_win32::position_window(window_id, requested).is_err());
    assert!(
        leopardwm_platform_win32::has_move_offscreen_ownership(window_id),
        "failed visible readback must retain recovery ownership"
    );

    let result = apply_placements_with_regions(
        &[WindowPlacement {
            window_id,
            rect: requested,
            visibility: Visibility::Visible,
            column_index: usize::MAX,
        }],
        &[],
        &PlatformConfig::default(),
        None,
        false,
    )
    .expect("float mismatch is returned as a receipt, not a false landing");
    assert_eq!(result.geometry_mismatches, vec![window_id]);
    assert!(leopardwm_platform_win32::has_move_offscreen_ownership(
        window_id
    ));
    FORCE_STAY_AT_SENTINEL.store(true, Ordering::Release);
    assert!(leopardwm_platform_win32::restore_window_moved_offscreen(window_id).is_err());
    assert!(
        leopardwm_platform_win32::has_move_offscreen_ownership(window_id),
        "a rejected emergency restore must retain ownership"
    );
    FORCE_STAY_AT_SENTINEL.store(false, Ordering::Release);
    assert!(
        leopardwm_platform_win32::restore_window_moved_offscreen(window_id)
            .expect("explicit recovery consumes retained ownership")
    );
    assert!(!leopardwm_platform_win32::has_move_offscreen_ownership(
        window_id
    ));
}
