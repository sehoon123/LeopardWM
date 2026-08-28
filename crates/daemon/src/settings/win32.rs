//! Win32 window shell + WebView2 settings panel.
//!
//! Creates a Win32 window with DWM theming (Mica, dark title bar, rounded
//! corners), then fills the client area with a WebView2 instance via `wry`.
//! All settings UI lives in the embedded HTML/CSS/JS (see `html.rs`).
//! Communication is via IPC: Rust → JS with `evaluate_script`, JS → Rust
//! with `window.ipc.postMessage`.

use std::collections::VecDeque;
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};

use anyhow::Result;
use tracing::{info, warn};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{
    DwmExtendFrameIntoClientArea, DwmSetWindowAttribute, DWMWINDOWATTRIBUTE,
};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForSystem, GetDpiForWindow};
use windows::Win32::UI::WindowsAndMessaging::*;

use raw_window_handle::{
    HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle, Win32WindowHandle,
    WindowsDisplayHandle,
};
use wry::WebContext;

use crate::config::{ConditionalConfigSave, Config};

use super::html::SETTINGS_HTML;
use super::SettingsEvent;

// DWM attributes for Windows 11 theming (not yet in windows crate enum)
const DWMWA_USE_IMMERSIVE_DARK_MODE_VAL: i32 = 20;
const DWMWA_WINDOW_CORNER_PREFERENCE_VAL: i32 = 33;
const DWMWA_SYSTEMBACKDROP_TYPE_VAL: i32 = 38;
const DWMWCP_ROUND: u32 = 2;
const DWMSBT_MAINWINDOW: u32 = 2; // Mica

const BASE_DPI: u32 = 96;
const WINDOW_CLIENT_WIDTH: i32 = 780;
const WINDOW_CLIENT_HEIGHT: i32 = 560;
const WINDOW_MIN_CLIENT_WIDTH: i32 = 640;
const WINDOW_MIN_CLIENT_HEIGHT: i32 = 420;

// Dark mode background (COLORREF = 0x00BBGGRR)
const DARK_BG: u32 = 0x00202020;

/// Custom message: ask the open settings window to refresh its rejected-hotkey
/// warning. Carries no payload; the new list is read from `PENDING_FAILED_BINDS`
/// on the window's own thread (the only thread that may touch the webview).
const WM_SETTINGS_PUSH_BINDS: u32 = WM_APP + 1;
/// Custom message: restore and foreground the existing settings window.
/// Sent to the settings thread so all window activation work stays on the
/// owning GUI thread.
const WM_SETTINGS_ACTIVATE: u32 = WM_APP + 2;
/// Custom message: deliver a save acknowledgement or optimistic-concurrency
/// conflict to the WebView on its owning thread.
const WM_SETTINGS_SAVE_RESULT: u32 = WM_APP + 3;

/// Thread id of the open settings window's message loop, or `None` when closed.
/// We target the thread queue (not the HWND) so a destroyed or recycled window
/// can never receive a stray push.
static SETTINGS_THREAD: Mutex<Option<u32>> = Mutex::new(None);
/// Latest rejected-bind list as a JSON array, staged for the next push.
static PENDING_FAILED_BINDS: Mutex<Option<String>> = Mutex::new(None);
/// FIFO save results staged by WebView IPC callbacks for the owning UI thread.
static PENDING_SAVE_RESULTS: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());
/// Rendered static settings resources are shared across opens.
static SETTINGS_HTML_RENDERED: OnceLock<String> = OnceLock::new();
static SETTINGS_HOTKEY_CATALOG_JSON: OnceLock<String> = OnceLock::new();

/// Push an updated rejected-hotkey list to the open settings window, if one is
/// open. Safe to call from any thread and no-ops when no window is open; the
/// `evaluate_script` itself runs on the window's own thread via its message loop.
pub fn push_failed_binds(failed_binds: &[String]) {
    let thread_id = match SETTINGS_THREAD.lock() {
        Ok(guard) => *guard,
        Err(_) => return,
    };
    let Some(thread_id) = thread_id else { return };
    let json = serde_json::to_string(failed_binds).unwrap_or_else(|_| "[]".to_string());
    let Ok(mut pending) = PENDING_FAILED_BINDS.lock() else {
        return;
    };

    // Keep the staging mutex held until the thread message has been accepted.
    // The window thread takes the same mutex before consuming the payload, so it
    // can never observe a message without its matching data. A failed post does
    // not leave stale data for a later settings-window lifetime.
    *pending = Some(json);
    let posted = unsafe {
        PostThreadMessageW(thread_id, WM_SETTINGS_PUSH_BINDS, WPARAM(0), LPARAM(0)).is_ok()
    };
    if !posted {
        *pending = None;
    }
}

/// Bring the already-open settings window back to the foreground.
///
/// Activation is posted to the owning message loop rather than manipulating
/// the HWND from the daemon thread. Returns false only when the window is not
/// ready yet, has already closed, or its thread queue rejected the message.
pub fn request_close() -> bool {
    let thread_id = match SETTINGS_THREAD.lock() {
        Ok(guard) => *guard,
        Err(_) => return false,
    };
    let Some(thread_id) = thread_id else {
        return false;
    };
    unsafe { PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0)).is_ok() }
}

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
struct Win32Handle(isize);

impl HasWindowHandle for Win32Handle {
    fn window_handle(
        &self,
    ) -> std::result::Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError>
    {
        let mut handle =
            Win32WindowHandle::new(unsafe { std::num::NonZero::new_unchecked(self.0) });
        handle.hinstance = None;
        let raw = RawWindowHandle::Win32(handle);
        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(raw) })
    }
}

impl HasDisplayHandle for Win32Handle {
    fn display_handle(
        &self,
    ) -> std::result::Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError>
    {
        let raw = RawDisplayHandle::Windows(WindowsDisplayHandle::new());
        Ok(unsafe { raw_window_handle::DisplayHandle::borrow_raw(raw) })
    }
}

fn settings_window_style() -> WINDOW_STYLE {
    // WS_OVERLAPPEDWINDOW supplies the resize frame and both caption buttons.
    // WS_CLIPCHILDREN prevents the Mica/background erase pass from painting over
    // WebView2 while its child controller follows an interactive resize.
    WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN
}

fn normalized_dpi(dpi: u32) -> u32 {
    if dpi == 0 {
        BASE_DPI
    } else {
        dpi
    }
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

fn initial_section_script(section: &str) -> String {
    let section_json = serde_json::to_string(section).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        "(function(section){{var items=document.querySelectorAll('.nav-item[data-section]');for(var i=0;i<items.length;i++){{if(items[i].dataset.section===section){{items[i].click();break;}}}}}})({section_json});"
    )
}

/// Build and run the settings window. Blocks until the window is closed.
pub fn run_settings_window(
    config: Config,
    event_tx: mpsc::Sender<SettingsEvent>,
    initial_section: Option<&str>,
    high_contrast: bool,
    failed_binds: Vec<String>,
) -> Result<()> {
    let config_revision = config.revision()?;
    unsafe {
        let hinstance = GetModuleHandleW(None)?;
        let dark = is_dark_mode();

        let bg_brush = if dark {
            CreateSolidBrush(COLORREF(DARK_BG))
        } else {
            HBRUSH((COLOR_BTNFACE.0 + 1) as _)
        };

        // Load the embedded application icon (set by winresource build script)
        let icon = LoadIconW(Some(hinstance.into()), PCWSTR(1 as _)).ok();
        let icon_sm = LoadImageW(
            Some(hinstance.into()),
            PCWSTR(1 as _),
            IMAGE_ICON,
            16,
            16,
            LR_DEFAULTCOLOR,
        )
        .ok()
        .map(|h| HICON(h.0));

        // Register window class
        let class_name = w!("LeopardWMSettings");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: bg_brush,
            lpszClassName: class_name,
            hIcon: icon.unwrap_or_default(),
            hIconSm: icon_sm.unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassExW(&wc);

        // Create a DPI-correct, normally resizable top-level window. Wry's
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

        // Apply Windows 11 DWM theming (Mica backdrop, dark title bar, rounded corners)
        apply_win11_theming(hwnd, dark);

        // Extend the DWM frame into the entire client area so Mica renders behind content
        let margins = MARGINS {
            cxLeftWidth: -1,
            cxRightWidth: -1,
            cyTopHeight: -1,
            cyBottomHeight: -1,
        };
        let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);

        // Persistent data directory so WebView2 reuses its browser profile
        // across settings opens (avoids cold-start each time).
        let data_dir = directories::ProjectDirs::from("", "", "leopardwm")
            .map(|d| d.cache_dir().join("webview2"))
            .unwrap_or_else(|| std::env::temp_dir().join("leopardwm-webview2"));
        let mut web_context = WebContext::new(Some(data_dir));

        // Create the WebView2 instance via wry
        let win_handle = Win32Handle(hwnd.0 as isize);
        let auto_start = leopardwm_platform_win32::autostart::get_autostart().unwrap_or(false);
        let config_json = {
            let mut val = serde_json::to_value(&config)
                .unwrap_or(serde_json::Value::Object(Default::default()));
            if let serde_json::Value::Object(ref mut map) = val {
                map.insert(
                    "high_contrast".to_string(),
                    serde_json::Value::Bool(high_contrast),
                );
                map.insert(
                    "auto_start".to_string(),
                    serde_json::Value::Bool(auto_start),
                );
                map.insert(
                    "config_revision".to_string(),
                    serde_json::Value::String(config_revision),
                );
            }
            serde_json::to_string(&val).unwrap_or_else(|_| "{}".to_string())
        };
        // The settings UI derives its hotkey-list labels, order, and
        // reset-to-defaults from this single catalog (see ipc::hotkeys).
        let catalog_json = SETTINGS_HOTKEY_CATALOG_JSON.get_or_init(|| {
            serde_json::to_string(&leopardwm_ipc::hotkeys::hotkey_catalog())
                .unwrap_or_else(|_| "[]".to_string())
        });
        let failed_binds_json =
            serde_json::to_string(&failed_binds).unwrap_or_else(|_| "[]".to_string());

        // Kept for the post-loop "window closed" notification; the original is
        // moved into the IPC handler closure below.
        let close_tx = event_tx.clone();

        let settings_html = SETTINGS_HTML_RENDERED
            .get_or_init(|| SETTINGS_HTML.replace("{VERSION}", env!("CARGO_PKG_VERSION")));
        let webview_result = wry::WebViewBuilder::new_with_web_context(&mut web_context)
            .with_html(settings_html.as_str())
            .with_initialization_script(format!(
                "window._initConfig = {}; window._hotkeyCatalog = {}; window._failedHotkeys = {};",
                config_json, catalog_json, failed_binds_json
            ))
            .with_ipc_handler(move |req| {
                handle_ipc(req.body(), &event_tx, hwnd);
            })
            .with_transparent(true)
            .with_background_color((0, 0, 0, 0))
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
        let init_js = "init(window._initConfig)".to_string();
        let _ = webview.evaluate_script(&init_js);

        // Navigate without interpolating the section into a CSS selector. The
        // current callers use internal constants, but JSON quoting keeps this
        // boundary safe and a missing section now degrades to a no-op.
        if let Some(section) = initial_section {
            let _ = webview.evaluate_script(&initial_section_script(section));
        }

        // Show the window
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);

        // Message loop. GetMessageW returns >0 for a message, 0 for WM_QUIT,
        // and -1 on error; break on anything <= 0 (matches the hotkey loop).
        let mut msg_buf = MSG::default();
        loop {
            let rc = GetMessageW(&mut msg_buf, None, 0, 0).0;
            if rc <= 0 {
                break;
            }
            if msg_buf.message == WM_SETTINGS_ACTIVATE {
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
                let json = PENDING_FAILED_BINDS.lock().ok().and_then(|mut p| p.take());
                if let Some(json) = json {
                    let js = format!(
                        "window._failedHotkeys = {}; if (typeof renderFailedHotkeys === 'function') renderFailedHotkeys();",
                        json
                    );
                    let _ = webview.evaluate_script(&js);
                }
                continue;
            }
            if msg_buf.message == WM_SETTINGS_SAVE_RESULT {
                let json = PENDING_SAVE_RESULTS
                    .lock()
                    .ok()
                    .and_then(|mut pending| pending.pop_front());
                if let Some(json) = json {
                    let js = format!(
                        "if (typeof handleSaveResult === 'function') handleSaveResult({});",
                        json
                    );
                    let _ = webview.evaluate_script(&js);
                }
                continue;
            }
            let _ = TranslateMessage(&msg_buf);
            DispatchMessageW(&msg_buf);
        }

        // Stop the daemon from posting to this thread before tearing down
        // WebView2 and its parent HWND.
        if let Ok(mut guard) = SETTINGS_THREAD.lock() {
            *guard = None;
        }
        if let Ok(mut pending) = PENDING_FAILED_BINDS.lock() {
            *pending = None;
        }
        if let Ok(mut pending) = PENDING_SAVE_RESULTS.lock() {
            pending.clear();
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
    }

    Ok(())
}

fn is_allowed_url(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap();
    (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
        && !authority.is_empty()
        && !authority.chars().any(char::is_whitespace)
}

/// Handle IPC messages from the WebView (JS → Rust).
fn handle_ipc(body: &str, event_tx: &mpsc::Sender<SettingsEvent>, _hwnd: HWND) {
    let msg: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => {
            warn!("Settings IPC: invalid JSON: {}", e);
            return;
        }
    };

    let action = msg.get("action").and_then(|v| v.as_str()).unwrap_or("");

    match action {
        "save" => {
            let result = match (
                msg.get("config"),
                msg.get("revision").and_then(|value| value.as_str()),
            ) {
                (Some(cfg_val), Some(revision)) => do_save(cfg_val, revision, event_tx),
                (_, None) => {
                    warn!("Settings IPC: save missing config revision; rejecting stale snapshot");
                    SettingsSaveResult::Failed
                }
                (None, _) => {
                    warn!("Settings IPC: save missing config");
                    SettingsSaveResult::Failed
                }
            };
            push_save_result(&result);
        }
        "set_recording" => {
            let recording = msg
                .get("recording")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let _ = event_tx.send(SettingsEvent::SetRecording(recording));
        }
        "set_auto_start" => {
            let Some(enabled) = msg.get("enabled").and_then(|v| v.as_bool()) else {
                warn!("Settings IPC: set_auto_start missing or non-bool 'enabled' field; ignoring");
                return;
            };
            use leopardwm_platform_win32::autostart;
            let result = if enabled {
                match std::env::current_exe() {
                    Ok(exe) => {
                        let target = autostart::preferred_autostart_executable(&exe);
                        autostart::enable_autostart(&target).map(|()| Some(target))
                    }
                    Err(e) => Err(anyhow::anyhow!("resolve daemon executable: {}", e)),
                }
            } else {
                autostart::disable_autostart().map(|()| None)
            };
            match result {
                Ok(Some(exe)) => info!("Auto-start enabled via Settings (path: {})", exe.display()),
                Ok(None) => info!("Auto-start disabled via Settings"),
                Err(e) => warn!("Settings: failed to update auto-start: {}", e),
            }
        }
        "open_url" => {
            let Some(url) = msg.get("url").and_then(|v| v.as_str()) else {
                warn!("Settings IPC: rejected open_url with missing or non-string 'url'");
                return;
            };
            if !is_allowed_url(url) {
                warn!("Settings IPC: rejected open_url: {:?}", url);
                return;
            }
            leopardwm_platform_win32::shell::open(url);
        }
        other => {
            warn!("Settings IPC: unknown action: {}", other);
        }
    }
}

#[derive(Debug)]
enum SettingsSaveResult {
    Saved {
        revision: String,
    },
    Conflict {
        current: Box<Config>,
        revision: String,
    },
    Failed,
}

/// Stage a result for delivery on the window thread; `WebView` itself never
/// crosses the IPC callback boundary.
fn push_save_result(result: &SettingsSaveResult) {
    let thread_id = match SETTINGS_THREAD.lock() {
        Ok(guard) => *guard,
        Err(_) => return,
    };
    let Some(thread_id) = thread_id else { return };

    let json = match result {
        SettingsSaveResult::Saved { revision } => {
            serde_json::json!({ "status": "saved", "revision": revision }).to_string()
        }
        SettingsSaveResult::Conflict { current, revision } => serde_json::json!({
            "status": "conflict",
            "revision": revision,
            "config": current,
        })
        .to_string(),
        SettingsSaveResult::Failed => serde_json::json!({ "status": "failed" }).to_string(),
    };

    let Ok(mut pending) = PENDING_SAVE_RESULTS.lock() else {
        return;
    };
    pending.push_back(json);
    let posted = unsafe {
        PostThreadMessageW(thread_id, WM_SETTINGS_SAVE_RESULT, WPARAM(0), LPARAM(0)).is_ok()
    };
    if !posted {
        pending.pop_back();
    }
}

/// Deserialize config JSON, validate, compare its snapshot revision, save to
/// disk only when current, and notify the daemon after a successful write.
fn do_save(
    cfg_val: &serde_json::Value,
    expected_revision: &str,
    event_tx: &mpsc::Sender<SettingsEvent>,
) -> SettingsSaveResult {
    let mut cfg: Config = match serde_json::from_value(cfg_val.clone()) {
        Ok(c) => c,
        Err(e) => {
            warn!("Settings: failed to parse config JSON: {}", e);
            return SettingsSaveResult::Failed;
        }
    };

    let warnings = cfg.validate();
    for w in &warnings {
        warn!("Config validation: {}: {}", w.field, w.message);
    }

    match cfg.save_if_current_revision(expected_revision) {
        Ok(ConditionalConfigSave::Saved { revision }) => {
            info!("Settings saved successfully");
            let _ = event_tx.send(SettingsEvent::Saved);
            SettingsSaveResult::Saved { revision }
        }
        Ok(ConditionalConfigSave::Conflict { current, revision }) => {
            warn!("Settings save rejected because the config changed outside this window");
            SettingsSaveResult::Conflict { current, revision }
        }
        Err(e) => {
            warn!("Failed to save settings: {}", e);
            SettingsSaveResult::Failed
        }
    }
}

// ── Window Procedure ─────────────────────────────────────────────────

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_ERASEBKGND => {
            // Paint black — DWM treats black in the extended frame as transparent,
            // letting the Mica backdrop show through.
            let hdc = HDC(wparam.0 as *mut _);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            FillRect(hdc, &rc, HBRUSH(GetStockObject(BLACK_BRUSH).0));
            LRESULT(1)
        }
        WM_GETMINMAXINFO => {
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
            // Cheap and idempotent — avoids unsafe lparam string parsing.
            apply_win11_theming(hwnd, is_dark_mode());
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        WM_CLOSE => {
            // Hide first, then let the owning message loop drop WebView2 before
            // it destroys the parent HWND. This avoids a white teardown flash and
            // keeps COM/controller destruction ordered.
            let _ = ShowWindow(hwnd, SW_HIDE);
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

// ── Windows 11 Theming ──────────────────────────────────────────────

/// Detect whether the system is using dark mode via the registry.
fn is_dark_mode() -> bool {
    unsafe {
        use windows::Win32::System::Registry::*;

        let subkey = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, subkey, Some(0), KEY_READ, &mut key).is_err() {
            return false;
        }

        let value_name = w!("AppsUseLightTheme");
        let mut data: u32 = 1;
        let mut data_size = std::mem::size_of::<u32>() as u32;
        let ok = RegQueryValueExW(
            key,
            value_name,
            None,
            None,
            Some(&mut data as *mut u32 as *mut u8),
            Some(&mut data_size),
        )
        .is_ok();
        let _ = RegCloseKey(key);

        ok && data == 0
    }
}

/// Apply Windows 11 DWM attributes: dark title bar, rounded corners, Mica backdrop.
unsafe fn apply_win11_theming(hwnd: HWND, dark: bool) {
    let val: i32 = if dark { 1 } else { 0 };
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWINDOWATTRIBUTE(DWMWA_USE_IMMERSIVE_DARK_MODE_VAL),
        &val as *const i32 as *const std::ffi::c_void,
        std::mem::size_of::<i32>() as u32,
    );

    let corner: u32 = DWMWCP_ROUND;
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWINDOWATTRIBUTE(DWMWA_WINDOW_CORNER_PREFERENCE_VAL),
        &corner as *const u32 as *const std::ffi::c_void,
        std::mem::size_of::<u32>() as u32,
    );

    let backdrop: u32 = DWMSBT_MAINWINDOW;
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWINDOWATTRIBUTE(DWMWA_SYSTEMBACKDROP_TYPE_VAL),
        &backdrop as *const u32 as *const std::ffi::c_void,
        std::mem::size_of::<u32>() as u32,
    );
}

#[cfg(test)]
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
    fn initial_section_script_json_quotes_the_section() {
        let section = "layout\");window.__leopardwm_injected=true;//";
        let encoded = serde_json::to_string(section).unwrap();
        let script = initial_section_script(section);

        assert!(script.ends_with(&format!("({encoded});")));
        assert!(script.contains("dataset.section===section"));
        assert!(!script.contains("data-section=\\\"{}\\\""));
    }

    #[test]
    fn allowed_urls_include_settings_links_and_mixed_case_schemes() {
        for url in [
            "https://github.com/sehoon123/LeopardWM/graphs/contributors",
            "https://github.com/sehoon123/LeopardWM",
            "https://github.com/sehoon123/LeopardWM",
            "hTtPs://example.com",
            "HtTp://example.com",
        ] {
            assert!(is_allowed_url(url), "expected URL to be allowed: {url}");
        }
    }

    #[test]
    fn disallowed_urls_reject_invalid_schemes_and_authorities() {
        for url in [
            "file:///C:/config.toml",
            "custom://example.com",
            "example.com",
            "://example.com",
            "",
            "https://",
            "https:// ",
            "https://exam ple.com",
            "https://example.com extra",
            "https:///x",
            "https:/x",
            " https://example.com",
        ] {
            assert!(!is_allowed_url(url), "expected URL to be rejected: {url:?}");
        }
    }
}
