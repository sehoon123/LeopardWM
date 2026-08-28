#![cfg(windows)]

use leopardwm_core_layout::{Rect, Visibility, WindowPlacement};
use leopardwm_platform_win32::tab_strip::{
    TabCloseAction, TabLabel, TabStripColors, TabStripOverlay,
};
use leopardwm_platform_win32::thumbnail::integration_probe;
use leopardwm_platform_win32::{apply_placements_with_regions, PlatformConfig, WindowRegionClip};
use std::ffi::c_void;
use std::sync::mpsc;
use std::time::Duration;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetForegroundWindow,
    GetMessageW, IsWindow, PostThreadMessageW, RegisterClassW, SendMessageW, UnregisterClassW,
    CW_USEDEFAULT, MSG, WINDOWPOS, WM_QUIT, WM_RBUTTONUP, WM_WINDOWPOSCHANGING, WNDCLASSW,
    WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

unsafe extern "system" fn source_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    DefWindowProcW(hwnd, message, wparam, lparam)
}

unsafe extern "system" fn stubborn_source_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_WINDOWPOSCHANGING {
        let position = &mut *(lparam.0 as *mut WINDOWPOS);
        if position.x.unsigned_abs() > 1000 || position.y.unsigned_abs() > 1000 {
            position.x = 100;
            position.y = 100;
        }
    }
    DefWindowProcW(hwnd, message, wparam, lparam)
}

unsafe extern "system" fn emergency_only_source_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_WINDOWPOSCHANGING {
        let position = &mut *(lparam.0 as *mut WINDOWPOS);
        let is_emergency = position.x <= -10_000 && position.y <= -10_000;
        if !is_emergency && (position.x.unsigned_abs() > 1000 || position.y.unsigned_abs() > 1000) {
            position.x = 100;
            position.y = 100;
        }
    }
    DefWindowProcW(hwnd, message, wparam, lparam)
}

fn ensure_dpi_awareness() {
    static SET_DPI: std::sync::Once = std::sync::Once::new();
    SET_DPI.call_once(|| {
        assert!(leopardwm_platform_win32::set_dpi_awareness());
    });
}

struct SourceWindow {
    hwnd: isize,
    thread_id: u32,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SourceWindow {
    fn new() -> Self {
        Self::new_with_mode(0)
    }

    fn new_with_constraints(stubborn: bool) -> Self {
        Self::new_with_mode(u8::from(stubborn))
    }

    fn new_with_emergency_fallback() -> Self {
        Self::new_with_mode(2)
    }

    fn new_with_mode(constraint_mode: u8) -> Self {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(isize, u32), String>>();
        let thread = std::thread::Builder::new()
            .name("preview-integration-source".into())
            .spawn(move || unsafe {
                let class: Vec<u16> = format!(
                    "LeopardWMIntegrationSource-{}-{}\0",
                    std::process::id(),
                    GetCurrentThreadId()
                )
                .encode_utf16()
                .collect();
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(match constraint_mode {
                        1 => stubborn_source_window_proc,
                        2 => emergency_only_source_window_proc,
                        _ => source_window_proc,
                    }),
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
                let title: Vec<u16> = "LeopardWM preview integration source\0"
                    .encode_utf16()
                    .collect();
                let hwnd = match CreateWindowExW(
                    Default::default(),
                    PCWSTR(class.as_ptr()),
                    PCWSTR(title.as_ptr()),
                    WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                    CW_USEDEFAULT,
                    CW_USEDEFAULT,
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

fn preview_park_case(source: &SourceWindow) -> (WindowPlacement, WindowRegionClip) {
    (
        WindowPlacement {
            window_id: source.hwnd as u64,
            rect: Rect::new(-400, 20, 640, 480),
            visibility: Visibility::OffScreenLeft,
            column_index: 0,
        },
        WindowRegionClip {
            window_id: source.hwnd as u64,
            clip_bounds: Rect::new(0, 20, 240, 480),
            fallback_rect: Rect::new(-5000, 20, 640, 480),
            fallback_visibility: Visibility::OffScreenLeft,
        },
    )
}

#[test]
fn ordered_real_preview_lifecycle_contract() {
    ensure_dpi_awareness();
    assert!(
        integration_probe::host_initial_spawn_failure_recovers(),
        "a transient first host failure must recover lazily"
    );

    {
        let source = SourceWindow::new();
        leopardwm_platform_win32::move_window_offscreen(source.hwnd as u64)
            .expect("verified MoveOffScreen park");
        let actual = leopardwm_platform_win32::get_window_chrome_rect(source.hwnd as u64)
            .expect("parked source rect");
        assert!(leopardwm_platform_win32::is_move_offscreen_sentinel_rect(
            &actual
        ));
        assert!(
            leopardwm_platform_win32::restore_window_moved_offscreen(source.hwnd as u64)
                .expect("marker-based MoveOffScreen restore")
        );
    }

    {
        let source = SourceWindow::new_with_emergency_fallback();
        let (placement, clip) = preview_park_case(&source);
        apply_placements_with_regions(
            &[placement],
            &[clip],
            &PlatformConfig::default(),
            None,
            false,
        )
        .unwrap_or_else(|error| panic!("verified emergency park should publish: {error}"));
        assert_eq!(
            leopardwm_platform_win32::thumbnail::current_register_balance(),
            1
        );
        leopardwm_platform_win32::thumbnail::invalidate_persistent_preview_surface();
        leopardwm_platform_win32::thumbnail::clear_persistent_previews()
            .expect("emergency preview cleanup");
    }

    {
        let source = SourceWindow::new_with_constraints(true);
        let (placement, clip) = preview_park_case(&source);
        let error = match apply_placements_with_regions(
            &[placement],
            &[clip],
            &PlatformConfig::default(),
            None,
            false,
        ) {
            Ok(_) => panic!("a source that rejects both parks must fail closed"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("layout did not safely commit"));
        assert_eq!(
            leopardwm_platform_win32::thumbnail::current_register_balance(),
            0,
            "failed parking must leave no registration before cleanup"
        );
        assert!(
            !unsafe {
                windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(
                    leopardwm_platform_win32::thumbnail::host().hwnd(),
                )
            }
            .as_bool(),
            "failed parking must leave the host hidden before cleanup"
        );
        leopardwm_platform_win32::thumbnail::invalidate_persistent_preview_surface();
        let _ = leopardwm_platform_win32::thumbnail::clear_persistent_previews_best_effort();
    }

    assert!(
        leopardwm_platform_win32::preview_input::integration_probe_restart_input_pump(),
        "dead input pump must be replaced"
    );
    assert!(
        integration_probe::two_target_z_order_is_valid(),
        "every target must precede the host"
    );

    assert!(
        leopardwm_platform_win32::integration_probe_incomplete_monitor_snapshot_fails_closed(),
        "one failed monitor callback must reject the entire safety snapshot"
    );
    let monitor = leopardwm_platform_win32::enumerate_monitors()
        .expect("monitor enumeration")
        .into_iter()
        .find(|monitor| monitor.is_primary)
        .expect("primary monitor");
    let source = SourceWindow::new();
    let source_hwnd = HWND(source.hwnd as *mut c_void);
    let destination_width = 40.min(monitor.work_area.width.max(1));
    let destination_height = 180.min(monitor.work_area.height.max(1));
    let destination = Rect::new(
        monitor.work_area.right() - destination_width,
        monitor.work_area.y,
        destination_width,
        destination_height,
    );
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::SetWindowPos(
            source_hwnd,
            Some(windows::Win32::UI::WindowsAndMessaging::HWND_TOP),
            monitor.work_area.x,
            monitor.work_area.y + destination_height + 20,
            300.min(monitor.work_area.width.max(1)),
            200.min(monitor.work_area.height.max(1)),
            windows::Win32::UI::WindowsAndMessaging::SWP_SHOWWINDOW,
        )
        .expect("probe source placement");
        assert!(
            leopardwm_platform_win32::set_foreground_window(source.hwnd as u64)
                .expect("probe foreground transfer"),
            "probe source must become foreground"
        );
        assert_eq!(GetForegroundWindow(), source_hwnd);
    }
    let actual_source = leopardwm_platform_win32::get_window_chrome_rect(source.hwnd as u64)
        .expect("positioned probe source rect");
    assert!(
        !actual_source.intersects(&destination),
        "foreground source must not cover the routed preview point"
    );
    assert!(destination.x >= monitor.work_area.x);
    assert!(destination.y >= monitor.work_area.y);
    assert!(destination.right() <= monitor.work_area.right());
    assert!(destination.bottom() <= monitor.work_area.bottom());
    let report = integration_probe::run(source.hwnd as u64, destination)
        .expect("real preview lifecycle probe");
    assert_eq!(report.initial_live_previews, 1);
    assert!(report.host_visible && report.target_above_host);
    assert!(
        report.point_hits_target,
        "physical point must resolve to target"
    );
    assert!(report.armed_hit_test && report.click_event_delivered);
    assert!(report.source_destroy_target_inert);
    assert!(report.concurrent_registration_rejected);
    assert!(report.stale_target_inert);
    assert_eq!(report.stale_commit_live_previews, 0);
    assert!(report.host_survived_close && report.host_restarted);
    assert_eq!(report.registration_balance_after_clear, 0);

    let captured_source = SourceWindow::new();
    let surviving_source = SourceWindow::new();
    let left_width = (destination.width / 2).max(1);
    let right_width = (destination.width - left_width).max(1);
    let captured_destination =
        Rect::new(destination.x, destination.y, left_width, destination.height);
    let surviving_destination = Rect::new(
        destination.x + left_width,
        destination.y,
        right_width,
        destination.height,
    );
    assert!(
        integration_probe::retained_capture_destroy_fence_survives_reanchor(
            captured_source.hwnd as u64,
            surviving_source.hwnd as u64,
            captured_destination,
            surviving_destination,
        )
        .expect("retained capture destroy-fence probe")
    );

    let retry_source = SourceWindow::new();
    assert!(
        integration_probe::retry_spawn_failure_recovers(retry_source.hwnd as u64, destination,)
            .expect("real retry publication probe"),
        "spawn failure obligation must publish the real desired request"
    );

    let (action_tx, _action_rx) = mpsc::channel();
    let strip = TabStripOverlay::new(action_tx).expect("tab strip probe creation");
    strip.show(
        Rect::new(
            monitor.work_area.x + 100,
            monitor.work_area.y + 200,
            500,
            400,
        ),
        vec![
            TabLabel {
                title: "One".into(),
                icon: None,
            },
            TabLabel {
                title: "Two".into(),
                icon: None,
            },
        ],
        0,
        TabStripColors::default(),
        28,
        0,
        monitor.id,
        0,
        0,
        monitor.scale_factor,
        TabCloseAction::CloseWindow,
    );
    std::thread::sleep(Duration::from_millis(100));
    let strip_raw = strip.hwnd_for_integration_probe();
    let menu_thread = std::thread::spawn(move || unsafe {
        let packed = (10u32 << 16) | 10u32;
        let _ = SendMessageW(
            HWND(strip_raw as *mut c_void),
            WM_RBUTTONUP,
            Some(WPARAM(0)),
            Some(LPARAM(packed as isize)),
        );
    });
    let menu_deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !TabStripOverlay::context_menu_open_for_integration_probe()
        && std::time::Instant::now() < menu_deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        TabStripOverlay::context_menu_open_for_integration_probe(),
        "context menu must enter its nested modal loop"
    );
    let shutdown_started = std::time::Instant::now();
    drop(strip);
    assert!(
        shutdown_started.elapsed() < Duration::from_secs(2),
        "tab strip Drop must terminate an open modal menu"
    );
    menu_thread.join().expect("context menu sender thread");
}
