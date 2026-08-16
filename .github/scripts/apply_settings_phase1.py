from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(
            f"{path}: expected exactly one match, found {count}\n--- needle ---\n{old}"
        )
    p.write_text(text.replace(old, new, 1), encoding="utf-8", newline="\n")


win32 = "crates/daemon/src/settings/win32.rs"
modrs = "crates/daemon/src/settings/mod.rs"

replace_once(
    win32,
    "use windows::Win32::UI::Controls::MARGINS;\nuse windows::Win32::UI::WindowsAndMessaging::*;",
    "use windows::Win32::UI::Controls::MARGINS;\nuse windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForSystem, GetDpiForWindow};\nuse windows::Win32::UI::WindowsAndMessaging::*;",
)

replace_once(
    win32,
    "const WINDOW_WIDTH: i32 = 780;\nconst WINDOW_HEIGHT: i32 = 560;",
    "const BASE_DPI: u32 = 96;\nconst WINDOW_CLIENT_WIDTH: i32 = 780;\nconst WINDOW_CLIENT_HEIGHT: i32 = 560;\nconst WINDOW_MIN_CLIENT_WIDTH: i32 = 640;\nconst WINDOW_MIN_CLIENT_HEIGHT: i32 = 420;",
)

replace_once(
    win32,
    "const WM_SETTINGS_PUSH_BINDS: u32 = WM_APP + 1;",
    "const WM_SETTINGS_PUSH_BINDS: u32 = WM_APP + 1;\n/// Custom message: restore and foreground the existing settings window.\n/// Sent to the settings thread so all window activation work stays on the\n/// owning GUI thread.\nconst WM_SETTINGS_ACTIVATE: u32 = WM_APP + 2;",
)

replace_once(
    win32,
    '''    unsafe {
        let _ = PostThreadMessageW(thread_id, WM_SETTINGS_PUSH_BINDS, WPARAM(0), LPARAM(0));
    }
}

/// Wrapper that implements `HasWindowHandle` + `HasDisplayHandle` for a raw HWND.
''',
    '''    unsafe {
        let _ = PostThreadMessageW(thread_id, WM_SETTINGS_PUSH_BINDS, WPARAM(0), LPARAM(0));
    }
}

/// Bring the already-open settings window back to the foreground.
///
/// Activation is posted to the owning message loop rather than manipulating
/// the HWND from the daemon thread. Returns false only when the window is not
/// ready yet, has already closed, or its thread queue rejected the message.
pub fn focus_existing_window() -> bool {
    let thread_id = match SETTINGS_THREAD.lock() {
        Ok(guard) => *guard,
        Err(_) => return false,
    };
    let Some(thread_id) = thread_id else {
        return false;
    };
    unsafe { PostThreadMessageW(thread_id, WM_SETTINGS_ACTIVATE, WPARAM(0), LPARAM(0)).is_ok() }
}

/// Wrapper that implements `HasWindowHandle` + `HasDisplayHandle` for a raw HWND.
''',
)

replace_once(
    win32,
    '''/// Build and run the settings window. Blocks until the window is closed.
pub fn run_settings_window(
''',
    '''fn settings_window_style() -> WINDOW_STYLE {
    // WS_OVERLAPPEDWINDOW supplies the resize frame and both caption buttons.
    // WS_CLIPCHILDREN prevents the Mica/background erase pass from painting over
    // WebView2 while its child controller follows an interactive resize.
    WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN
}

fn normalized_dpi(dpi: u32) -> u32 {
    if dpi == 0 { BASE_DPI } else { dpi }
}

fn scale_logical_px(value: i32, dpi: u32) -> i32 {
    let value = i64::from(value.max(1));
    let dpi = i64::from(normalized_dpi(dpi));
    let scaled = (value * dpi + i64::from(BASE_DPI / 2)) / i64::from(BASE_DPI);
    scaled.clamp(1, i64::from(i32::MAX)) as i32
}

/// Convert a desired logical client size into the top-level window size needed
/// for the active DPI, including caption and resize-frame metrics.
unsafe fn outer_size_for_client(client_width: i32, client_height: i32, dpi: u32) -> (i32, i32) {
    let dpi = normalized_dpi(dpi);
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: scale_logical_px(client_width, dpi),
        bottom: scale_logical_px(client_height, dpi),
    };
    if AdjustWindowRectExForDpi(
        &mut rect,
        settings_window_style(),
        false,
        WINDOW_EX_STYLE::default(),
        dpi,
    )
    .is_err()
    {
        return (rect.right.max(1), rect.bottom.max(1));
    }
    (
        rect.right.saturating_sub(rect.left).max(1),
        rect.bottom.saturating_sub(rect.top).max(1),
    )
}

/// Build and run the settings window. Blocks until the window is closed.
pub fn run_settings_window(
''',
)

replace_once(
    win32,
    '''        // Create the window
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            w!("LeopardWM Settings"),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            WINDOW_WIDTH,
            WINDOW_HEIGHT,
            None,
            None,
            Some(hinstance.into()),
            None,
        )?;

        // Expose this thread's id so the daemon can push live updates to the
        // window's message queue (see push_failed_binds).
        if let Ok(mut g) = SETTINGS_THREAD.lock() {
            *g = Some(GetCurrentThreadId());
        }
''',
    '''        // Create a DPI-correct, normally resizable top-level window. Wry's
        // Windows backend automatically resizes the WebView2 controller when its
        // parent HWND changes size.
        let ex_style = WINDOW_EX_STYLE::default();
        let style = settings_window_style();
        let dpi = normalized_dpi(GetDpiForSystem());
        let (window_width, window_height) =
            outer_size_for_client(WINDOW_CLIENT_WIDTH, WINDOW_CLIENT_HEIGHT, dpi);
        let hwnd = match CreateWindowExW(
            ex_style,
            class_name,
            w!("LeopardWM Settings"),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            window_width,
            window_height,
            None,
            None,
            Some(hinstance.into()),
            None,
        ) {
            Ok(hwnd) => hwnd,
            Err(error) => {
                let _ = UnregisterClassW(class_name, Some(hinstance.into()));
                if dark {
                    let _ = DeleteObject(HGDIOBJ(bg_brush.0));
                }
                if let Some(icon_sm) = icon_sm {
                    let _ = DestroyIcon(icon_sm);
                }
                return Err(error.into());
            }
        };
''',
)

replace_once(
    win32,
    '''        let webview = wry::WebViewBuilder::new_with_web_context(&mut web_context)
            .with_html(&settings_html)
            .with_initialization_script(format!(
                "window._initConfig = {}; window._hotkeyCatalog = {}; window._failedHotkeys = {};",
                config_json, catalog_json, failed_binds_json
            ))
            .with_ipc_handler(move |req| {
                handle_ipc(req.body(), &event_tx, hwnd);
            })
            .with_transparent(true)
            .with_background_color((0, 0, 0, 0))
            .with_additional_browser_args("--disable-features=msSmartScreenProtection")
            .build(&win_handle)?;

        // Populate the form with the current config
''',
    '''        let webview_result = wry::WebViewBuilder::new_with_web_context(&mut web_context)
            .with_html(&settings_html)
            .with_initialization_script(format!(
                "window._initConfig = {}; window._hotkeyCatalog = {}; window._failedHotkeys = {};",
                config_json, catalog_json, failed_binds_json
            ))
            .with_ipc_handler(move |req| {
                handle_ipc(req.body(), &event_tx, hwnd);
            })
            .with_transparent(true)
            .with_background_color((0, 0, 0, 0))
            .with_additional_browser_args("--disable-features=msSmartScreenProtection")
            .build(&win_handle);
        let webview = match webview_result {
            Ok(webview) => webview,
            Err(error) => {
                let _ = DestroyWindow(hwnd);
                let _ = UnregisterClassW(class_name, Some(hinstance.into()));
                if dark {
                    let _ = DeleteObject(HGDIOBJ(bg_brush.0));
                }
                if let Some(icon_sm) = icon_sm {
                    let _ = DestroyIcon(icon_sm);
                }
                return Err(error.into());
            }
        };

        // Publish the thread id only after the window and WebView are fully
        // initialized. This prevents live-update or focus messages from racing
        // a half-constructed settings surface.
        if let Ok(mut guard) = SETTINGS_THREAD.lock() {
            *guard = Some(GetCurrentThreadId());
        }

        // Populate the form with the current config
''',
)

replace_once(
    win32,
    '''            // Daemon-initiated live refresh of the rejected-hotkey warning. The
            // webview is only valid on this thread, so we apply it here rather
            // than in the (static) window proc.
            if msg_buf.message == WM_SETTINGS_PUSH_BINDS {
''',
    '''            if msg_buf.message == WM_SETTINGS_ACTIVATE {
                if IsIconic(hwnd).as_bool() {
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                } else {
                    let _ = ShowWindow(hwnd, SW_SHOW);
                }
                let _ = SetForegroundWindow(hwnd);
                continue;
            }
            // Daemon-initiated live refresh of the rejected-hotkey warning. The
            // webview is only valid on this thread, so we apply it here rather
            // than in the (static) window proc.
            if msg_buf.message == WM_SETTINGS_PUSH_BINDS {
''',
)

replace_once(
    win32,
    '''        // The window is gone; stop the daemon from posting to a dead thread.
        if let Ok(mut g) = SETTINGS_THREAD.lock() {
            *g = None;
        }
        // Let the daemon resume hotkeys if the window closed mid-recording.
        let _ = close_tx.send(SettingsEvent::Closed);

        // Hide window before tearing down WebView2 to prevent white flash.
        let _ = ShowWindow(hwnd, SW_HIDE);
        drop(webview);
        drop(web_context);
        if dark {
            let _ = DeleteObject(HGDIOBJ(bg_brush.0));
        }
        let _ = UnregisterClassW(class_name, Some(hinstance.into()));
''',
    '''        // Stop the daemon from posting to this thread before tearing down
        // WebView2 and its parent HWND.
        if let Ok(mut guard) = SETTINGS_THREAD.lock() {
            *guard = None;
        }
        if let Ok(mut pending) = PENDING_FAILED_BINDS.lock() {
            *pending = None;
        }
        // Let the daemon resume hotkeys if the window closed mid-recording.
        let _ = close_tx.send(SettingsEvent::Closed);

        // Keep the parent alive until the WebView2 controller has been dropped;
        // destroying the HWND first can leave COM teardown racing a dead host.
        let _ = ShowWindow(hwnd, SW_HIDE);
        drop(webview);
        drop(web_context);
        if IsWindow(Some(hwnd)).as_bool() {
            let _ = DestroyWindow(hwnd);
        }
        let _ = UnregisterClassW(class_name, Some(hinstance.into()));
        if dark {
            let _ = DeleteObject(HGDIOBJ(bg_brush.0));
        }
        if let Some(icon_sm) = icon_sm {
            let _ = DestroyIcon(icon_sm);
        }
''',
)

replace_once(
    win32,
    '''        WM_SETTINGCHANGE => {
            // Re-apply DWM theming on any system setting change (theme toggle, etc.).
''',
    '''        WM_GETMINMAXINFO => {
            let dpi = normalized_dpi(GetDpiForWindow(hwnd));
            let (min_width, min_height) =
                outer_size_for_client(WINDOW_MIN_CLIENT_WIDTH, WINDOW_MIN_CLIENT_HEIGHT, dpi);
            let info = &mut *(lparam.0 as *mut MINMAXINFO);
            info.ptMinTrackSize.x = min_width;
            info.ptMinTrackSize.y = min_height;
            LRESULT(0)
        }
        WM_DPICHANGED => {
            // Per-monitor DPI awareness requires accepting Windows' suggested
            // outer rectangle when the settings window crosses monitor scales.
            let suggested = lparam.0 as *const RECT;
            if !suggested.is_null() {
                let suggested = &*suggested;
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    suggested.left,
                    suggested.top,
                    suggested.right.saturating_sub(suggested.left),
                    suggested.bottom.saturating_sub(suggested.top),
                    SWP_NOACTIVATE | SWP_NOZORDER,
                );
            }
            LRESULT(0)
        }
        WM_SETTINGCHANGE => {
            // Re-apply DWM theming on any system setting change (theme toggle, etc.).
''',
)

replace_once(
    win32,
    '''        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
''',
    '''        WM_CLOSE => {
            // Hide first, then let the owning message loop drop WebView2 before
            // it destroys the parent HWND. This avoids a white teardown flash and
            // keeps COM/controller destruction ordered.
            let _ = ShowWindow(hwnd, SW_HIDE);
            PostQuitMessage(0);
            LRESULT(0)
        }
''',
)

replace_once(
    win32,
    '''#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_urls_include_settings_links_and_mixed_case_schemes() {
''',
    '''#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_window_style_is_resizable_and_child_flicker_safe() {
        let style = settings_window_style();
        for required in [
            WS_THICKFRAME,
            WS_MAXIMIZEBOX,
            WS_MINIMIZEBOX,
            WS_CLIPCHILDREN,
        ] {
            assert_ne!(style.0 & required.0, 0, "missing style bit: {required:?}");
        }
    }

    #[test]
    fn logical_pixel_scaling_handles_common_dpi_and_zero_fallback() {
        assert_eq!(scale_logical_px(100, 0), 100);
        assert_eq!(scale_logical_px(100, 96), 100);
        assert_eq!(scale_logical_px(100, 120), 125);
        assert_eq!(scale_logical_px(100, 144), 150);
        assert_eq!(scale_logical_px(100, 192), 200);
    }

    #[test]
    fn allowed_urls_include_settings_links_and_mixed_case_schemes() {
''',
)

replace_once(
    modrs,
    '''/// Singleton guard — only one settings window at a time.
static SETTINGS_OPEN: AtomicBool = AtomicBool::new(false);

/// Handle to the settings window thread.
''',
    '''/// Singleton guard — only one settings window at a time.
static SETTINGS_OPEN: AtomicBool = AtomicBool::new(false);

/// Resets the singleton flag even if the settings thread unwinds unexpectedly.
struct SettingsOpenGuard;

impl Drop for SettingsOpenGuard {
    fn drop(&mut self) {
        SETTINGS_OPEN.store(false, Ordering::SeqCst);
    }
}

/// Handle to the settings window thread.
''',
)

replace_once(
    modrs,
    '''        {
            info!("Settings window already open — focusing existing");
            return None;
        }
''',
    '''        {
            if win32::focus_existing_window() {
                info!("Settings window already open — focused existing window");
            } else {
                info!("Settings window already open — it is still initializing");
            }
            return None;
        }
''',
)

replace_once(
    modrs,
    '''        let handle = std::thread::Builder::new()
            .name("settings-window".into())
            .spawn(move || {
                if let Err(e) = win32::run_settings_window(
                    config,
                    event_tx,
                    section.as_deref(),
                    high_contrast,
                    failed_binds,
                ) {
                    warn!("Settings window error: {}", e);
                }
                SETTINGS_OPEN.store(false, Ordering::SeqCst);
            })
            .ok()?;

        Some(SettingsWindowHandle { _thread: handle })
''',
    '''        let handle = match std::thread::Builder::new()
            .name("settings-window".into())
            .spawn(move || {
                let _open_guard = SettingsOpenGuard;
                if let Err(e) = win32::run_settings_window(
                    config,
                    event_tx,
                    section.as_deref(),
                    high_contrast,
                    failed_binds,
                ) {
                    warn!("Settings window error: {}", e);
                }
            })
        {
            Ok(handle) => handle,
            Err(error) => {
                SETTINGS_OPEN.store(false, Ordering::SeqCst);
                warn!("Failed to spawn settings window thread: {}", error);
                return None;
            }
        };

        Some(SettingsWindowHandle { _thread: handle })
''',
)
