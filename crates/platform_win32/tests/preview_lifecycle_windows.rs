#![cfg(windows)]

use leopardwm_core_layout::{Rect, Visibility, WindowPlacement};
use leopardwm_platform_win32::tab_strip::{
    TabCloseAction, TabLabel, TabStripColors, TabStripOverlay,
};
use leopardwm_platform_win32::thumbnail::integration_probe;
use leopardwm_platform_win32::{
    apply_placements_with_regions, MonitorInfo, PlatformConfig, PreviewClickEvent,
    PreviewClickTarget, PreviewGesture, WindowRegionClip,
};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, InvalidateRect, UpdateWindow,
    PAINTSTRUCT,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, FindWindowW,
    GetForegroundWindow, GetMessageW, IsWindow, PostThreadMessageW, RegisterClassW, SendMessageW,
    UnregisterClassW, CW_USEDEFAULT, MSG, WINDOWPOS, WM_PAINT, WM_QUIT, WM_RBUTTONUP,
    WM_WINDOWPOSCHANGING, WNDCLASSW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

fn dual_gate_destination(monitor: &MonitorInfo, monitors: &[MonitorInfo]) -> Option<Rect> {
    monitors.iter().find_map(|other| {
        let top = monitor.work_area.y.max(other.work_area.y);
        let bottom = monitor.work_area.bottom().min(other.work_area.bottom());
        let destination_y = top + 20;
        let sample_y = destination_y + 60;
        if other.id == monitor.id || bottom - top < 160 || monitor.work_area.width < 160 {
            return None;
        }
        [
            (
                monitor.work_area.right() - 160,
                monitor.work_area.right().saturating_add(2),
            ),
            (monitor.work_area.x, monitor.work_area.x.saturating_sub(2)),
        ]
        .into_iter()
        .find_map(|(destination_x, outside_x)| {
            (!monitor.contains_point(outside_x, sample_y)
                && other.contains_point(outside_x, sample_y))
            .then_some(Rect::new(destination_x, destination_y, 160, 120))
        })
    })
}

#[test]
fn dual_gate_requires_the_outside_sample_on_a_distinct_monitor() {
    let make_monitor = |id, rect, work_area| MonitorInfo {
        id,
        rect,
        work_area,
        is_primary: id == 1,
        device_name: format!("DISPLAY{id}"),
        scale_factor: 1.0,
    };
    let primary = make_monitor(1, Rect::new(0, 0, 1000, 800), Rect::new(0, 0, 1000, 760));
    let adjacent = make_monitor(
        2,
        Rect::new(1000, 100, 800, 600),
        Rect::new(1000, 100, 800, 560),
    );
    assert_eq!(
        dual_gate_destination(&primary, std::slice::from_ref(&adjacent)),
        Some(Rect::new(840, 120, 160, 120))
    );
    assert_eq!(
        dual_gate_destination(&adjacent, std::slice::from_ref(&primary)),
        Some(Rect::new(1000, 120, 160, 120))
    );

    let gap = make_monitor(
        2,
        Rect::new(1010, 100, 800, 600),
        Rect::new(1010, 100, 800, 560),
    );
    assert_eq!(dual_gate_destination(&primary, &[gap]), None);

    let inset_primary = make_monitor(1, primary.rect, Rect::new(0, 0, 960, 760));
    let adjacent = make_monitor(
        2,
        Rect::new(1000, 100, 800, 600),
        Rect::new(1000, 100, 800, 560),
    );
    assert_eq!(dual_gate_destination(&inset_primary, &[adjacent]), None);
}

#[test]
fn physical_click_receipt_requires_exact_publication_identity() {
    let target = PreviewClickTarget {
        window_id: 42,
        source_process_id: 7,
        publication_generation: 11,
        rect: Rect::new(100, 200, 40, 80),
    };
    let event = PreviewClickEvent {
        window_id: target.window_id,
        source_process_id: target.source_process_id,
        publication_generation: target.publication_generation,
        preview_rect: target.rect,
        gesture: PreviewGesture::Click,
    };
    assert!(integration_probe::click_receipt_matches_target(
        event, target
    ));
    assert!(!integration_probe::click_receipt_matches_target(
        PreviewClickEvent {
            publication_generation: 10,
            ..event
        },
        target
    ));
    assert!(!integration_probe::click_receipt_matches_target(
        PreviewClickEvent {
            preview_rect: Rect::new(101, 200, 40, 80),
            ..event
        },
        target
    ));
    assert!(!integration_probe::click_receipt_matches_target(
        PreviewClickEvent {
            window_id: 43,
            ..event
        },
        target
    ));
    assert!(!integration_probe::click_receipt_matches_target(
        PreviewClickEvent {
            source_process_id: 8,
            ..event
        },
        target
    ));
    assert!(!integration_probe::click_receipt_matches_target(
        PreviewClickEvent {
            gesture: PreviewGesture::Drag,
            ..event
        },
        target
    ));
}

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

unsafe extern "system" fn return_rejecting_source_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_WINDOWPOSCHANGING {
        let position = &mut *(lparam.0 as *mut WINDOWPOS);
        let is_sentinel = position.x <= -10_000 && position.y <= -10_000;
        if !is_sentinel {
            position.x = 100;
            position.y = 100;
        }
    }
    DefWindowProcW(hwnd, message, wparam, lparam)
}

static COLORED_SOURCE_PAINTED: AtomicBool = AtomicBool::new(false);

/// A controlled paint source lets the integration probe distinguish DWM's
/// successful property API from a real sampled thumbnail pixel.
unsafe extern "system" fn colored_source_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_PAINT {
        let mut paint = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut paint);
        let brush = CreateSolidBrush(COLORREF(0x000000ff));
        if !brush.is_invalid() {
            let client = RECT {
                left: 0,
                top: 0,
                right: 640,
                bottom: 480,
            };
            let _ = FillRect(hdc, &client, brush);
            let _ = DeleteObject(brush.into());
        }
        let _ = EndPaint(hwnd, &paint);
        COLORED_SOURCE_PAINTED.store(true, Ordering::Release);
        return LRESULT(0);
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

    fn new_with_rejected_return() -> Self {
        Self::new_with_mode(3)
    }

    fn new_colored() -> Self {
        COLORED_SOURCE_PAINTED.store(false, Ordering::Release);
        Self::new_with_mode(4)
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
                        3 => return_rejecting_source_window_proc,
                        4 => colored_source_window_proc,
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
                if constraint_mode == 4 {
                    let _ = InvalidateRect(Some(hwnd), None, true);
                    if !UpdateWindow(hwnd).as_bool()
                        || !COLORED_SOURCE_PAINTED.load(Ordering::Acquire)
                    {
                        let _ = ready_tx.send(Err(
                            "colored source did not complete WM_PAINT before readiness".into(),
                        ));
                        let _ = DestroyWindow(hwnd);
                        let _ = UnregisterClassW(PCWSTR(class.as_ptr()), None);
                        return;
                    }
                }
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

fn assert_daemon_absent() {
    let daemon_class: Vec<u16> = "LeopardWMSysEventClass\0".encode_utf16().collect();
    assert!(
        unsafe { FindWindowW(PCWSTR(daemon_class.as_ptr()), PCWSTR::null()) }.is_err(),
        "stop LeopardWM before running owned-HWND probes; a live daemon would manage and corrupt probe windows"
    );
}

fn verify_rejected_visible_float_return(monitor: &leopardwm_platform_win32::MonitorInfo) {
    let rejecting = SourceWindow::new_with_rejected_return();
    let window_id = rejecting.hwnd as u64;
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
    assert!(
        leopardwm_platform_win32::restore_window_moved_offscreen(window_id)
            .expect("explicit recovery consumes retained ownership")
    );
    assert!(!leopardwm_platform_win32::has_move_offscreen_ownership(
        window_id
    ));
}

#[test]
#[allow(clippy::too_many_lines)] // One ordered HWND lifecycle prevents parallel desktop mutation.
fn ordered_real_preview_lifecycle_contract() {
    ensure_dpi_awareness();
    assert_daemon_absent();
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
    let monitors = leopardwm_platform_win32::enumerate_monitors().expect("monitor enumeration");
    let primary = monitors
        .iter()
        .find(|monitor| monitor.is_primary)
        .cloned()
        .expect("primary monitor");
    let require_physical_click = std::env::var_os("LEOPARDWM_REQUIRE_PHYSICAL_CLICK").is_some();
    let foreground_rect = require_physical_click
        .then(|| unsafe { GetForegroundWindow() })
        .and_then(|foreground| {
            leopardwm_platform_win32::get_window_chrome_rect(foreground.0 as isize as u64)
        });
    let monitor = if require_physical_click {
        monitors
            .iter()
            .find(|monitor| {
                foreground_rect
                    .as_ref()
                    .is_none_or(|foreground| !monitor.rect.intersects(foreground))
            })
            .cloned()
            .unwrap_or(primary)
    } else {
        primary
    };

    verify_rejected_visible_float_return(&monitor);

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
        if !require_physical_click {
            assert!(
                leopardwm_platform_win32::set_foreground_window(source.hwnd as u64)
                    .expect("probe foreground transfer"),
                "probe source must become foreground"
            );
            assert_eq!(GetForegroundWindow(), source_hwnd);
        }
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
        "physical point must resolve to target; owner={:?}, target/host-visible={}/{}",
        report.point_hit_owner, report.target_above_host, report.host_visible
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
    assert!(
        integration_probe::retry_exhaustion_keeps_desired_and_recovers(
            retry_source.hwnd as u64,
            destination,
        )
        .expect("retry exhaustion must retain desired preview state"),
        "a bounded retry burst must self-heal without another placement"
    );
    assert!(
        integration_probe::registration_exhaustion_retains_relayout_obligation(
            retry_source.hwnd as u64,
            destination,
        )
        .expect("registration exhaustion recovery probe"),
        "registration recovery must preserve desire and require a fresh physical relayout"
    );

    let colored_source = SourceWindow::new_colored();
    let dual_destination = dual_gate_destination(&monitor, &monitors);
    if std::env::var_os("LEOPARDWM_REQUIRE_DUAL_MONITOR").is_some() {
        assert!(
            dual_destination.is_some(),
            "dual-monitor gate requires a physically adjacent output at the sampled edge"
        );
    }
    let colored_destination = dual_destination.unwrap_or_else(|| {
        Rect::new(
            monitor.work_area.right() - 160,
            monitor.work_area.y + 20,
            160,
            120,
        )
    });
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::SetWindowPos(
            HWND(colored_source.hwnd as *mut c_void),
            None,
            monitor.work_area.x + 40,
            monitor.work_area.y + 260,
            640,
            480,
            windows::Win32::UI::WindowsAndMessaging::SWP_SHOWWINDOW,
        )
        .expect("colored source placement");
    }
    assert!(
        integration_probe::controlled_colored_source_pixel_proof(
            colored_source.hwnd as u64,
            colored_destination,
            COLORREF(0x000000ff),
        )
        .expect("controlled DWM pixel proof"),
        "DWM API publication must produce the controlled source pixels"
    );

    let cloak_source = SourceWindow::new();
    assert!(
        integration_probe::placement_cloak_failure_is_not_cached(cloak_source.hwnd as u64),
        "a failed cloak must remain retryable and use a verified sentinel fallback"
    );
    let ownership_source = SourceWindow::new();
    assert!(
        integration_probe::host_restart_claims_are_generation_safe(ownership_source.hwnd as u64)
            .expect("host restart ownership probe"),
        "old host-generation drops must not mutate replacement z-order claims"
    );
    assert!(
        integration_probe::unregister_failure_retains_ownership(ownership_source.hwnd as u64)
            .expect("thumbnail unregister retry probe"),
        "failed unregister must retain and later release its ownership receipt"
    );

    assert!(
        leopardwm_platform_win32::remove_maximizebox(ownership_source.hwnd as u64)
            .expect("snap style removal"),
        "controlled source must create a durable snap-style receipt"
    );
    assert!(
        leopardwm_platform_win32::restore_marked_maximizeboxes_best_effort() >= 1,
        "hard-crash snap recovery must consume the durable HWND marker"
    );
    let style = unsafe {
        windows::Win32::UI::WindowsAndMessaging::GetWindowLongW(
            HWND(ownership_source.hwnd as *mut c_void),
            windows::Win32::UI::WindowsAndMessaging::GWL_STYLE,
        )
    };
    assert_ne!(
        style & windows::Win32::UI::WindowsAndMessaging::WS_MAXIMIZEBOX.0 as i32,
        0,
        "hard-crash recovery must restore WS_MAXIMIZEBOX physically"
    );
    let _ = leopardwm_platform_win32::restore_maximizebox(ownership_source.hwnd as u64);

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
                window_id: ownership_source.hwnd as u64,
                title: "One".into(),
                icon: None,
            },
            TabLabel {
                window_id: retry_source.hwnd as u64,
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
