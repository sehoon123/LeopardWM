//! DWM thumbnail bindings for ghost-animation of swap-chain-sensitive windows.
//!
//! Chromium/Electron/Mozilla/Cascadia renderers can't keep up with per-frame
//! `SetWindowPos` cadence — their swap chains desync. This module composites
//! a DWM thumbnail of a cloaked source HWND onto a hidden host window and
//! animates the thumbnail's destination rect instead of moving the live HWND.
//!
//! See `crates/daemon/src/animation_worker.rs` for the per-frame update path
//! and `crates/daemon/src/helpers.rs::start_layout_transition` for the
//! registration site.

use crate::{window_id_to_hwnd, Win32Error};
use leopardwm_core_layout::{Rect, WindowId};
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use tracing::warn;
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmQueryThumbnailSourceSize, DwmRegisterThumbnail, DwmUnregisterThumbnail,
    DwmUpdateThumbnailProperties, DWM_THUMBNAIL_PROPERTIES, DWM_TNP_OPACITY,
    DWM_TNP_RECTDESTINATION, DWM_TNP_RECTSOURCE, DWM_TNP_VISIBLE,
};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject,
    AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS,
    HBITMAP, HDC,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, GetSystemMetrics,
    RegisterClassW, SetWindowPos, UpdateLayeredWindow, CW_USEDEFAULT, HWND_NOTOPMOST, HWND_TOPMOST,
    MSG, SET_WINDOW_POS_FLAGS, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, ULW_ALPHA, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP, WS_VISIBLE,
};

/// Class name for the singleton thumbnail host window. Listed in
/// `enumeration::should_skip_window_by_class` so we don't try to manage
/// our own overlay.
pub(crate) const THUMBNAIL_HOST_CLASS: &str = "LeopardWMThumbnailHost";

/// Process-global counter of currently-registered DWM thumbnails. Used
/// by tests and the `lwm health` IPC field to assert no leaks. Mirrors
/// `Z_ORDER_STATE.balance` for lock-free reads.
static REGISTER_BALANCE: AtomicI64 = AtomicI64::new(0);

/// Serializes register/unregister z-order side effects so concurrent
/// register/unregister can't interleave between the atomic balance update
/// and the `set_topmost` side effect.
///
/// Without this, the bad interleaving is:
///   T1 unregister: balance=1→0, about to call set_topmost(false)
///   T2 register:   balance=0→1, calls set_topmost(true) first
///   T1 unregister: calls set_topmost(false)  ← host left non-topmost with a live thumbnail
struct ZOrderState {
    /// Every handle we own, wherever it is composited (health metric).
    balance: i64,
    /// Host-bound handles that require the topmost band (transition ghosts).
    topmost_balance: i64,
    /// Handles composited on the singleton HOST only — drives the host's
    /// topmost promotion/demotion. Overview-overlay registrations are
    /// excluded: shuffling the host's z-order around the (itself topmost)
    /// overview window caused visible z churn at overview open/close.
    host_balance: i64,
}
static Z_ORDER_STATE: Mutex<ZOrderState> = Mutex::new(ZOrderState {
    balance: 0,
    topmost_balance: 0,
    host_balance: 0,
});

/// Return the current outstanding-registration count. Should converge to 0
/// after any animation cycle completes.
pub fn current_register_balance() -> i64 {
    REGISTER_BALANCE.load(Ordering::Relaxed)
}

/// RAII wrapper around an `HTHUMBNAIL`. Unregisters on drop unless the
/// handle has been transferred out via [`ThumbnailHandle::into_isize`].
///
/// `Send` + `Sync` safety: `HTHUMBNAIL` is a kernel-level handle managed
/// by `dwm.exe`; it has no thread affinity post-registration. Cross-thread
/// `DwmUpdateThumbnailProperties` is supported by design (Aero Flip 3D
/// used the same pattern from worker threads).
pub struct ThumbnailHandle {
    /// Raw `HTHUMBNAIL` value. Set to 0 by `into_isize` to suppress Drop.
    handle: isize,
    /// Whether this registration participates in the HOST z-order
    /// accounting (true only for host-destined thumbnails).
    host_z: bool,
    /// Which band this registration claimed, so unregistering releases the
    /// same claim it made.
    band: HostBand,
}

// SAFETY: HTHUMBNAIL is a process-wide DWM handle, not bound to any HWND
// owner thread for updates. Codex's Microsoft-Learn check confirmed no
// apartment-affinity requirement post-registration.
unsafe impl Send for ThumbnailHandle {}
unsafe impl Sync for ThumbnailHandle {}

impl Drop for ThumbnailHandle {
    fn drop(&mut self) {
        if self.handle != 0 {
            unregister_impl(self.handle, self.host_z, self.band);
        }
    }
}

impl ThumbnailHandle {
    /// Consume this handle without firing Drop, returning the raw `isize`.
    /// The caller takes responsibility for eventually calling
    /// [`unregister_raw`] on the returned value (or wrapping it in a new
    /// owning type that does).
    ///
    /// Used at landing to transfer handle ownership from the daemon's
    /// `AppState.ghost_handles` into `WorkerCommand::Crossfade` entries
    /// owned by the worker thread.
    pub fn into_isize(mut self) -> isize {
        // unregister_raw assumes host z-order accounting; only
        // host-destined handles may be transferred raw.
        debug_assert!(self.host_z, "into_isize on a non-host thumbnail handle");
        let raw = self.handle;
        self.handle = 0;
        std::mem::forget(self);
        raw
    }

    /// Raw `HTHUMBNAIL` for cross-thread `update` calls. Does NOT transfer
    /// ownership — Drop still fires when `self` is dropped.
    pub fn as_isize(&self) -> isize {
        self.handle
    }

    /// Test-only stand-in (handle 0): Drop's unregister no-ops.
    #[cfg(test)]
    pub(crate) fn fake() -> Self {
        Self {
            handle: 0,
            host_z: false,
            band: HostBand::Normal,
        }
    }
}

/// Register a DWM thumbnail of `source` against the singleton host window.
/// On success, returns an owning handle whose `Drop` unregisters.
pub fn register(wid: WindowId) -> Result<ThumbnailHandle, Win32Error> {
    register_on_host(wid, HostBand::Topmost)
}

/// Where a host-bound thumbnail wants the shared host in the z-order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostBand {
    /// Transition ghosts composite over live windows that may be cloaked
    /// underneath, so they need the topmost band.
    Topmost,
    /// A monitor-edge preview stands in for a tiled window, so it belongs in the
    /// normal band: in the topmost band it would cover floating windows, which
    /// sit above the tiled layer by design.
    Normal,
}

fn register_on_host(wid: WindowId, band: HostBand) -> Result<ThumbnailHandle, Win32Error> {
    let host_hwnd = host().hwnd();
    if host_hwnd.0.is_null() {
        return Err(Win32Error::SetPositionFailed(
            "thumbnail host unavailable".into(),
        ));
    }
    register_to(host_hwnd, wid, true, band)
}

/// Register a DWM thumbnail of `source_wid` against an arbitrary top-level
/// window of THIS process (e.g. the overview overlay). Shares the
/// `REGISTER_BALANCE` accounting with [`register`] (the balance counts
/// every handle we own, wherever it is composited) but does NOT touch the
/// host's z-order: the destination window manages its own z, and shuffling
/// the host around it caused visible z churn (overview open/close flash).
pub fn register_for_window(
    dest_hwnd_raw: isize,
    source_wid: WindowId,
) -> Result<ThumbnailHandle, Win32Error> {
    if dest_hwnd_raw == 0 {
        return Err(Win32Error::SetPositionFailed(
            "thumbnail destination hwnd is null".into(),
        ));
    }
    register_to(
        HWND(dest_hwnd_raw as *mut c_void),
        source_wid,
        false,
        HostBand::Normal,
    )
}

fn register_to(
    dest: HWND,
    wid: WindowId,
    host_z: bool,
    band: HostBand,
) -> Result<ThumbnailHandle, Win32Error> {
    let source = window_id_to_hwnd(wid)?;
    let raw = unsafe { DwmRegisterThumbnail(dest, source) }.map_err(|e| {
        Win32Error::SetPositionFailed(format!("DwmRegisterThumbnail({:?}): {}", source.0, e))
    })?;
    if raw == 0 {
        return Err(Win32Error::SetPositionFailed(
            "DwmRegisterThumbnail returned null handle".into(),
        ));
    }
    // Serialize the balance update with the z-order side effect so a
    // concurrent unregister can't sneak its set_topmost(false) in
    // between our increment and our set_topmost(true).
    {
        let mut z = Z_ORDER_STATE
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex);
        z.balance += 1;
        REGISTER_BALANCE.store(z.balance, Ordering::Relaxed);
        // First active HOST thumbnail: promote the host to HWND_TOPMOST so
        // the composition sits above ordinary windows. While idle the host
        // stays non-topmost so the Windows taskbar (also topmost) can
        // animate without z-order interference. Non-host registrations
        // (overview overlay) never move the host.
        if host_z {
            z.host_balance += 1;
            if band == HostBand::Topmost {
                z.topmost_balance += 1;
            }
            // Only a ghost demands the topmost band. A host that carries nothing
            // but previews stays in the normal band so floating windows keep
            // their place above the tiled layer.
            if z.topmost_balance == 1 && band == HostBand::Topmost {
                host().set_topmost(true);
            }
        }
    }
    Ok(ThumbnailHandle {
        handle: raw,
        host_z,
        band,
    })
}

/// Update the destination rect, opacity, and visibility of a registered
/// thumbnail. Safe to call from any thread (the worker thread does this
/// per animation frame). Destination-agnostic: only the `HTHUMBNAIL` is
/// needed, whatever window the registration targeted.
///
/// `dest_client_rect` is in CLIENT coordinates of the thumbnail's
/// DESTINATION window, NOT screen coordinates. For host-bound thumbnails
/// convert via [`screen_to_host_client`] first; overview thumbnails pass
/// overlay client coordinates directly.
pub fn update(
    handle: isize,
    dest_client_rect: Rect,
    opacity: u8,
    visible: bool,
) -> Result<(), Win32Error> {
    update_properties(handle, None, dest_client_rect, opacity, visible)
}

#[cfg_attr(test, allow(dead_code))]
pub(crate) fn update_cropped(
    handle: isize,
    source_rect: Rect,
    dest_client_rect: Rect,
    opacity: u8,
    visible: bool,
) -> Result<(), Win32Error> {
    update_properties(
        handle,
        Some(source_rect),
        dest_client_rect,
        opacity,
        visible,
    )
}

fn update_properties(
    handle: isize,
    source_rect: Option<Rect>,
    dest_client_rect: Rect,
    opacity: u8,
    visible: bool,
) -> Result<(), Win32Error> {
    if handle == 0 {
        return Err(Win32Error::SetPositionFailed(
            "thumbnail::update called with null handle".into(),
        ));
    }
    let mut flags = DWM_TNP_RECTDESTINATION | DWM_TNP_OPACITY | DWM_TNP_VISIBLE;
    let mut props = DWM_THUMBNAIL_PROPERTIES {
        dwFlags: flags,
        rcDestination: RECT {
            left: dest_client_rect.x,
            top: dest_client_rect.y,
            right: dest_client_rect.x + dest_client_rect.width,
            bottom: dest_client_rect.y + dest_client_rect.height,
        },
        rcSource: RECT::default(),
        opacity,
        fVisible: BOOL::from(visible),
        fSourceClientAreaOnly: BOOL::from(false),
    };
    if let Some(source) = source_rect {
        flags |= DWM_TNP_RECTSOURCE;
        props.dwFlags = flags;
        props.rcSource = RECT {
            left: source.x,
            top: source.y,
            right: source.x + source.width,
            bottom: source.y + source.height,
        };
    }
    unsafe { DwmUpdateThumbnailProperties(handle, &props) }.map_err(|error| {
        Win32Error::SetPositionFailed(format!("DwmUpdateThumbnailProperties: {error}"))
    })
}

/// Source window's true size for a registered thumbnail, used for
/// aspect-fit destination rects. `None` on null handles or DWM failure.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PersistentPreviewRequest {
    pub window_id: WindowId,
    pub source_rect: Rect,
    pub expected_source_size: (i32, i32),
    pub destination_screen_rect: Rect,
}

#[cfg_attr(test, allow(dead_code))]
struct PersistentPreview {
    handle: ThumbnailHandle,
    source_size: Option<(i32, i32)>,
    expected_source_size: Option<(i32, i32)>,
}

static PERSISTENT_PREVIEWS: OnceLock<Mutex<HashMap<WindowId, PersistentPreview>>> = OnceLock::new();
static PERSISTENT_PREVIEW_TRANSACTION: Mutex<()> = Mutex::new(());

fn persistent_previews() -> &'static Mutex<HashMap<WindowId, PersistentPreview>> {
    PERSISTENT_PREVIEWS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_persistent_previews() -> std::sync::MutexGuard<'static, HashMap<WindowId, PersistentPreview>>
{
    persistent_previews()
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
}

pub(crate) fn lock_persistent_preview_transaction() -> std::sync::MutexGuard<'static, ()> {
    PERSISTENT_PREVIEW_TRANSACTION
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
}

pub(crate) fn prepare_persistent_preview(window_id: WindowId) -> bool {
    #[cfg(test)]
    {
        let _ = window_id;
        false
    }
    #[cfg(not(test))]
    {
        let mut previews = lock_persistent_previews();
        if previews.contains_key(&window_id) {
            return true;
        }
        let Ok(handle) = register_on_host(window_id, HostBand::Normal) else {
            return false;
        };
        let initial_size = source_size(handle.as_isize());
        previews.insert(
            window_id,
            PersistentPreview {
                handle,
                source_size: initial_size,
                expected_source_size: None,
            },
        );
        true
    }
}

pub(crate) fn has_persistent_preview(window_id: WindowId) -> bool {
    #[cfg(test)]
    {
        let _ = window_id;
        false
    }
    #[cfg(not(test))]
    {
        lock_persistent_previews().contains_key(&window_id)
    }
}

fn scale_edge(value: i32, actual: i32, expected: i32) -> i32 {
    if actual <= 0 || expected <= 0 {
        return 0;
    }
    ((i64::from(value.max(0)) * i64::from(actual) + i64::from(expected) / 2) / i64::from(expected))
        .clamp(0, i64::from(actual)) as i32
}

fn normalized_preview_geometry(
    request: PersistentPreviewRequest,
    actual_source_size: (i32, i32),
) -> Option<(Rect, Rect)> {
    let (actual_w, actual_h) = actual_source_size;
    let (expected_w, expected_h) = request.expected_source_size;
    if actual_w <= 0 || actual_h <= 0 || expected_w <= 0 || expected_h <= 0 {
        return None;
    }
    let left = scale_edge(request.source_rect.x, actual_w, expected_w);
    let top = scale_edge(request.source_rect.y, actual_h, expected_h);
    let right = scale_edge(request.source_rect.right(), actual_w, expected_w).max(left);
    let bottom = scale_edge(request.source_rect.bottom(), actual_h, expected_h).max(top);
    if right <= left || bottom <= top {
        return None;
    }
    Some((
        Rect::new(left, top, right - left, bottom - top),
        request.destination_screen_rect,
    ))
}

/// Publish each request and report the windows whose thumbnail is actually on
/// screen afterwards. Callers need the identities, not just a count: an overlay
/// must never be placed over a preview that failed to publish, or it would
/// absorb clicks where no pixels of ours are drawn.
fn publish_preview_requests(
    requests: &[PersistentPreviewRequest],
    refresh_size: bool,
) -> Vec<WindowId> {
    #[cfg(test)]
    {
        let _ = (requests, refresh_size);
        Vec::new()
    }
    #[cfg(not(test))]
    {
        let origin = host().origin();
        let mut failed = Vec::new();
        let mut published = Vec::new();
        let mut previews = lock_persistent_previews();
        for request in requests {
            let Some(preview) = previews.get_mut(&request.window_id) else {
                continue;
            };
            // A missing source size is always re-queried, even on an
            // intermediate frame: DWM can refuse the query transiently (a UWP
            // host still settling, for example) and without this the preview
            // would stay unpublished until some later event happened to force a
            // size-refreshing pass.
            if preview.source_size.is_none()
                || (refresh_size
                    && preview.expected_source_size != Some(request.expected_source_size))
            {
                preview.source_size = source_size(preview.handle.as_isize());
                preview.expected_source_size = Some(request.expected_source_size);
            }
            let Some(source_size) = preview.source_size else {
                if refresh_size {
                    failed.push(request.window_id);
                }
                continue;
            };
            let Some((source, destination_screen)) =
                normalized_preview_geometry(*request, source_size)
            else {
                if refresh_size {
                    failed.push(request.window_id);
                }
                continue;
            };
            let destination = screen_to_host_client(destination_screen, origin);
            if update_cropped(preview.handle.as_isize(), source, destination, 255, true).is_ok() {
                published.push(request.window_id);
            } else {
                failed.push(request.window_id);
            }
        }
        if refresh_size {
            for window_id in failed {
                previews.remove(&window_id);
            }
        }
        published
    }
}

pub(crate) fn commit_persistent_previews(
    requests: &[PersistentPreviewRequest],
    refresh_source_size: bool,
) -> usize {
    let published = publish_preview_requests(requests, refresh_source_size);
    let mut previews = lock_persistent_previews();
    previews.retain(|window_id, _| {
        requests
            .iter()
            .any(|request| request.window_id == *window_id)
    });
    let live = published.len().min(previews.len());
    let registered: Vec<WindowId> = previews.keys().copied().collect();
    drop(previews);
    // A thumbnail is pixels only, so the strip needs its own click target for
    // the scroll-first gesture of clicking a partially visible column.
    crate::preview_input::sync_preview_click_targets(&preview_click_targets(requests, &registered));
    live
}

/// Click targets for the previews this frame owns: one per request whose
/// thumbnail registration is alive, covering exactly the rectangle the thumbnail
/// is drawn into.
///
/// Keyed on the registration rather than on this frame's publish result. A
/// publish can fail transiently while the source stays parked off-monitor, and
/// gating the overlay on it left the edge strip dead until some later event
/// forced another pass — the user had to click the opposite edge and come back.
/// The registration is the honest signal that this column is represented at the
/// edge; `clear_persistent_previews` and the reconcile below drop the overlay as
/// soon as it is not. Duplicate ids collapse to their first request so a single
/// overlay is never created twice and orphaned.
pub(crate) fn preview_click_targets(
    requests: &[PersistentPreviewRequest],
    registered: &[WindowId],
) -> Vec<crate::preview_input::PreviewClickTarget> {
    let mut targets: Vec<crate::preview_input::PreviewClickTarget> = Vec::new();
    for request in requests {
        if request.destination_screen_rect.width <= 0 || request.destination_screen_rect.height <= 0
        {
            continue;
        }
        if !registered.contains(&request.window_id) {
            continue;
        }
        if targets
            .iter()
            .any(|target| target.window_id == request.window_id)
        {
            continue;
        }
        targets.push(crate::preview_input::PreviewClickTarget {
            window_id: request.window_id,
            rect: request.destination_screen_rect,
        });
    }
    targets
}

pub(crate) fn clear_persistent_previews() {
    lock_persistent_previews().clear();
    crate::preview_input::clear_preview_click_targets();
}

pub(crate) fn forget_persistent_preview(window_id: WindowId) {
    lock_persistent_previews().remove(&window_id);
    // Drop every overlay rather than leave a stale one swallowing clicks for a
    // window that no longer has a preview; the next applied frame republishes
    // the survivors.
    crate::preview_input::clear_preview_click_targets();
}

pub fn source_size(handle: isize) -> Option<(i32, i32)> {
    if handle == 0 {
        return None;
    }
    let size = unsafe { DwmQueryThumbnailSourceSize(handle) }.ok()?;
    if size.cx <= 0 || size.cy <= 0 {
        return None;
    }
    Some((size.cx, size.cy))
}

/// Unregister a thumbnail by raw `HTHUMBNAIL` value. Used by the worker
/// thread when consuming `CrossfadeEntry` values (whose Drop calls this).
/// Raw transfers exist only on the HOST path, so this keeps the host
/// z-order accounting; non-host handles unregister through their Drop.
///
/// Idempotent on null/zero handles — does nothing.
pub fn unregister_raw(handle: isize) {
    // Only ghost handles are transferred raw (see `into_isize`), and a ghost
    // always claims the topmost band.
    unregister_impl(handle, true, HostBand::Topmost);
}

fn unregister_impl(handle: isize, host_z: bool, band: HostBand) {
    if handle == 0 {
        return;
    }
    // A failed DwmUnregisterThumbnail leaks the DWM handle (the caller
    // already gave up its owning reference, so we can't retry). Decrement
    // anyway: the balance tracks handles WE account for, and pinning it
    // above zero on a transient failure would strand the host topmost for
    // the rest of the session and make the health metric lie. Clamp at
    // zero so a double-unregister or failure run can't go negative.
    if let Err(e) = unsafe { DwmUnregisterThumbnail(handle) } {
        warn!(
            "DwmUnregisterThumbnail({}) failed (handle leaked): {}",
            handle, e
        );
    }
    // Serialize the balance update with the z-order side effect.
    let mut z = Z_ORDER_STATE
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex);
    z.balance = (z.balance - 1).max(0);
    REGISTER_BALANCE.store(z.balance, Ordering::Relaxed);
    // Last accounted HOST thumbnail just went away: drop the host back to
    // non-topmost so it stops interfering with the taskbar's auto-hide
    // z-order. Guard on prev >= 1 so a clamped underflow can't skip it.
    if host_z {
        z.host_balance = (z.host_balance - 1).max(0);
        if band == HostBand::Topmost {
            let prev = z.topmost_balance;
            z.topmost_balance = (z.topmost_balance - 1).max(0);
            if prev >= 1 && z.topmost_balance == 0 {
                host().set_topmost(false);
            }
        }
    }
}

/// Convert a screen-coordinate rect to client coordinates of the host
/// window. The host is positioned at the virtual-screen origin
/// (`SM_XVIRTUALSCREEN`, `SM_YVIRTUALSCREEN`), so the conversion is a
/// simple subtraction.
///
/// CRITICAL: `SM_XVIRTUALSCREEN` can be negative when a secondary monitor
/// extends to the left of the primary. The single most likely bug class
/// in this whole module is a sign error here. Unit-tested.
pub fn screen_to_host_client(screen: Rect, host_origin: (i32, i32)) -> Rect {
    Rect {
        x: screen.x - host_origin.0,
        y: screen.y - host_origin.1,
        width: screen.width,
        height: screen.height,
    }
}

/// Predicate: does this top-level window belong to a renderer family whose
/// client surface can desynchronize from the outer HWND after repeated moves?
///
/// This list drives adaptive synchronous movement, targeted landing refreshes,
/// and the optional DWM ghost path. Size-changing safe-mode transitions remain
/// class-agnostic: an unprotected resize snaps even for an unknown renderer.
pub fn is_compositor_sensitive_class(wid: WindowId) -> bool {
    is_compositor_sensitive_class_str(&class_name(wid))
}

/// String variant of [`is_compositor_sensitive_class`] for callers that have
/// already read the class name (avoids a redundant `GetClassNameW` call).
pub fn is_compositor_sensitive_class_str(class: &str) -> bool {
    class.starts_with("Chrome_WidgetWin_")
        || class == "MozillaWindowClass"
        || class == "CASCADIA_HOSTING_WINDOW_CLASS"
        || class.starts_with("WindowsForms10.")
        || class.starts_with("HwndWrapper")
        || class == "CabinetWClass"
        || class == "ExploreWClass"
        || class == "ApplicationFrameWindow"
        || class == "WinUIDesktopWin32WindowClass"
        || class == "Notepad"
        || class == "CEF-OSC-WIDGET"
        || class.starts_with("Qt5QWindow")
        || class.starts_with("Qt6QWindow")
}

/// Backward-compatible name for the experimental thumbnail eligibility check.
pub fn is_ghost_animation_class(wid: WindowId) -> bool {
    is_compositor_sensitive_class(wid)
}

/// Backward-compatible string variant used by existing daemon call sites.
pub fn is_ghost_animation_class_str(class: &str) -> bool {
    is_compositor_sensitive_class_str(class)
}

/// Read the class name of a window. Returns empty string on failure
/// (unknown class, dead HWND, or invalid window ID).
pub fn class_name(wid: WindowId) -> String {
    let Ok(hwnd) = window_id_to_hwnd(wid) else {
        return String::new();
    };
    class_name_hwnd(hwnd)
}

fn class_name_hwnd(hwnd: HWND) -> String {
    use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
    let mut buf: [u16; 256] = [0; 256];
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    if len <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..len as usize])
}

// ----------------------------------------------------------------------
// ThumbnailHost — singleton invisible host window covering the virtual screen.
// ----------------------------------------------------------------------

/// Singleton invisible host window used as the destination for all
/// thumbnails. Created lazily on first `host()` call. Lives until process
/// exit.
///
/// Style choice: `WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE
/// | WS_EX_TRANSPARENT` with a 1×1 fully-transparent UpdateLayeredWindow
/// backing, mirroring `border.rs`. This is the proven-working pattern for
/// click-through composite overlays in our codebase.
pub struct ThumbnailHost {
    hwnd_raw: isize,
    /// Virtual-screen origin captured at host creation, updated by
    /// `resize_to_virtual_screen` on display change. Wrapped in a Mutex
    /// for cross-thread reads (animation worker reads on every frame).
    origin: std::sync::Mutex<(i32, i32)>,
    _thread: Option<std::thread::JoinHandle<()>>,
}

static HOST: OnceLock<ThumbnailHost> = OnceLock::new();

/// Get (or lazily create) the global thumbnail host.
pub fn host() -> &'static ThumbnailHost {
    HOST.get_or_init(|| match ThumbnailHost::new() {
        Ok(h) => h,
        Err(e) => {
            // Construct-failure path: panic in dev, but in production
            // surface a recoverable host with a null HWND so callers can
            // detect and fall back to legacy animation.
            warn!(
                "ThumbnailHost::new failed: {} — ghost animation disabled",
                e
            );
            ThumbnailHost {
                hwnd_raw: 0,
                origin: std::sync::Mutex::new(virtual_screen_origin()),
                _thread: None,
            }
        }
    })
}

impl ThumbnailHost {
    fn new() -> Result<Self, Win32Error> {
        #[cfg(test)]
        panic!("ThumbnailHost::new spawns a DWM host window; gate the call behind cfg(test)");
        #[allow(unreachable_code)]
        {
            let origin = virtual_screen_origin();
            let (vw, vh) = virtual_screen_size();
            let (tx, rx) = mpsc::channel::<Result<isize, Win32Error>>();

            let thread = std::thread::Builder::new()
                .name("leopardwm-thumbnail-host".into())
                .spawn(move || unsafe {
                    let class_name: Vec<u16> = format!("{}\0", THUMBNAIL_HOST_CLASS)
                        .encode_utf16()
                        .collect();
                    let wc = WNDCLASSW {
                        lpfnWndProc: Some(thumbnail_host_proc),
                        lpszClassName: windows::core::PCWSTR(class_name.as_ptr()),
                        ..Default::default()
                    };
                    RegisterClassW(&wc);

                    let ex_style =
                        WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT;

                    match CreateWindowExW(
                        ex_style,
                        windows::core::PCWSTR(class_name.as_ptr()),
                        None,
                        WS_POPUP | WS_VISIBLE,
                        origin.0,
                        origin.1,
                        vw,
                        vh,
                        None,
                        None,
                        None,
                        None,
                    ) {
                        Ok(h) => {
                            // Initialize layered window with a 1×1 fully-transparent
                            // bitmap so DWM treats it as a valid layered surface.
                            // Without this, the host has no backing and DWM
                            // composition of the thumbnail behaves inconsistently.
                            init_layered_transparent(h);
                            // Idle z-order: non-topmost so the Windows taskbar
                            // (also topmost) can show in front during auto-hide
                            // animation. `register` promotes us to topmost when
                            // at least one thumbnail is alive.
                            let _ = SetWindowPos(
                                h,
                                Some(HWND_NOTOPMOST),
                                origin.0,
                                origin.1,
                                vw,
                                vh,
                                SWP_NOACTIVATE,
                            );
                            let _ = tx.send(Ok(h.0 as isize));
                            let mut msg = MSG::default();
                            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                                let _ = DispatchMessageW(&msg);
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(Err(Win32Error::HookInstallFailed(format!(
                                "ThumbnailHost: {}",
                                e
                            ))));
                        }
                    }
                })
                .map_err(|e| {
                    Win32Error::HookInstallFailed(format!("ThumbnailHost thread: {}", e))
                })?;

            let hwnd_raw = match rx.recv() {
                Ok(Ok(raw)) => raw,
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    return Err(Win32Error::HookInstallFailed(
                        "ThumbnailHost init failed".into(),
                    ))
                }
            };

            Ok(Self {
                hwnd_raw,
                origin: std::sync::Mutex::new(origin),
                _thread: Some(thread),
            })
        }
    }

    /// HWND of the host window. `HWND(0)` if construction failed (e.g.
    /// under cfg(test) or genuine init failure).
    pub fn hwnd(&self) -> HWND {
        HWND(self.hwnd_raw as *mut c_void)
    }

    /// Origin of the host's client area in screen coordinates (matches
    /// `(SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN)` at the most recent
    /// `resize_to_virtual_screen` call, or host creation if no resize
    /// has happened).
    pub fn origin(&self) -> (i32, i32) {
        *self
            .origin
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex)
    }

    /// `true` if the host construction succeeded. Callers should check
    /// this before attempting registration.
    pub fn is_available(&self) -> bool {
        self.hwnd_raw != 0
    }

    /// Resize and reposition the host to the current virtual-screen
    /// geometry. Called from the daemon's display-change recovery so
    /// thumbnail destination rects use post-change coordinates. Subsequent
    /// `register` calls see the new origin via `origin()`.
    pub fn resize_to_virtual_screen(&self) {
        if self.hwnd_raw == 0 {
            return;
        }
        let new_origin = virtual_screen_origin();
        let (vw, vh) = virtual_screen_size();
        {
            let mut g = self
                .origin
                .lock()
                .unwrap_or_else(crate::recover_poisoned_mutex);
            *g = new_origin;
        }
        let hwnd = self.hwnd();
        // Preserve current z-order: if thumbnails are live we're topmost,
        // otherwise non-topmost. Pass SWP_NOZORDER to leave it untouched.
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                None,
                new_origin.0,
                new_origin.1,
                vw,
                vh,
                SWP_NOACTIVATE | SWP_NOZORDER,
            );
        }
    }

    /// Toggle the host's z-order between topmost (while thumbnails are
    /// active) and non-topmost (idle). Idle non-topmost lets the taskbar
    /// auto-hide animation appear correctly in front of windows; topmost
    /// during animation ensures the thumbnail composites above the live
    /// HWNDs that may be cloaked underneath.
    fn set_topmost(&self, topmost: bool) {
        if self.hwnd_raw == 0 {
            return;
        }
        let hwnd = self.hwnd();
        let z = if topmost {
            HWND_TOPMOST
        } else {
            HWND_NOTOPMOST
        };
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                Some(z),
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
            );
        }
    }
}

extern "system" fn thumbnail_host_proc(
    hwnd: HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Set up the layered host with a 1×1 fully-transparent backing so DWM
/// composes its layered surface correctly. Without this step,
/// `WS_EX_LAYERED` windows that never call `UpdateLayeredWindow` may not
/// composite thumbnails reliably on all GPUs.
unsafe fn init_layered_transparent(hwnd: HWND) {
    let screen_dc: HDC = GetDC(None);
    let mem_dc = CreateCompatibleDC(Some(screen_dc));

    // 1×1 BGRA bitmap, alpha = 0 (fully transparent).
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: 1,
            biHeight: 1,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let bmp_result = CreateDIBSection(Some(mem_dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0);
    let bmp: HBITMAP = match bmp_result {
        Ok(h) => h,
        Err(_) => {
            let _ = DeleteDC(mem_dc);
            ReleaseDC(None, screen_dc);
            return;
        }
    };
    // Zero the pixel (alpha = 0).
    if !bits.is_null() {
        std::ptr::write_bytes(bits as *mut u8, 0, 4);
    }
    let old = SelectObject(mem_dc, bmp.into());

    let src_pt = windows::Win32::Foundation::POINT { x: 0, y: 0 };
    let size = windows::Win32::Foundation::SIZE { cx: 1, cy: 1 };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    let _ = UpdateLayeredWindow(
        hwnd,
        None,
        None,
        Some(&size),
        Some(mem_dc),
        Some(&src_pt),
        windows::Win32::Foundation::COLORREF(0),
        Some(&blend),
        ULW_ALPHA,
    );

    SelectObject(mem_dc, old);
    let _ = DeleteObject(bmp.into());
    let _ = DeleteDC(mem_dc);
    ReleaseDC(None, screen_dc);
}

fn virtual_screen_origin() -> (i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
        )
    }
}

fn virtual_screen_size() -> (i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

// Silence unused-warning hint: SWP_NOZORDER, SET_WINDOW_POS_FLAGS,
// CW_USEDEFAULT, WS_POPUP are kept for completeness even when not all
// are used directly.
#[allow(dead_code)]
const _UNUSED_IMPORTS: (SET_WINDOW_POS_FLAGS, i32) = (SWP_NOZORDER, CW_USEDEFAULT);

#[cfg(test)]
mod preview_click_target_tests {
    use super::{preview_click_targets, PersistentPreviewRequest};
    use leopardwm_core_layout::Rect;

    fn request(window_id: u64, x: i32, width: i32) -> PersistentPreviewRequest {
        PersistentPreviewRequest {
            window_id,
            source_rect: Rect::new(0, 0, width, 800),
            expected_source_size: (width, 800),
            destination_screen_rect: Rect::new(x, 100, width, 800),
        }
    }

    #[test]
    fn only_registered_previews_get_a_click_target() {
        let requests = [request(1, 0, 250), request(2, 1670, 250)];
        // Window 2 has no live registration — nothing represents it at the edge,
        // so an overlay there would swallow clicks over nothing of ours.
        let targets = preview_click_targets(&requests, &[1]);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].window_id, 1);
        assert_eq!(targets[0].rect, Rect::new(0, 100, 250, 800));
    }

    #[test]
    fn a_registered_preview_stays_clickable_across_a_failed_publish() {
        // The registration is the contract: a transient DWM publish failure must
        // not leave the edge strip dead until another event forces a pass.
        let requests = [request(1, 0, 250)];
        assert_eq!(preview_click_targets(&requests, &[1]).len(), 1);
    }

    #[test]
    fn nothing_registered_means_no_click_targets() {
        let requests = [request(1, 0, 250)];
        assert!(preview_click_targets(&requests, &[]).is_empty());
    }

    #[test]
    fn a_duplicate_request_yields_one_target() {
        // Two overlays for one id would orphan the first HWND in the pump's map.
        let requests = [request(7, 0, 250), request(7, 1670, 250)];
        let targets = preview_click_targets(&requests, &[7]);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].rect, Rect::new(0, 100, 250, 800));
    }

    #[test]
    fn an_empty_destination_is_never_clickable() {
        let mut empty = request(3, 0, 0);
        empty.destination_screen_rect = Rect::new(0, 100, 0, 800);
        assert!(preview_click_targets(&[empty], &[3]).is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_screen_to_host_client_zero_origin() {
        let screen = Rect {
            x: 100,
            y: 200,
            width: 800,
            height: 600,
        };
        let client = screen_to_host_client(screen, (0, 0));
        assert_eq!(client.x, 100);
        assert_eq!(client.y, 200);
        assert_eq!(client.width, 800);
        assert_eq!(client.height, 600);
    }

    #[test]
    fn test_screen_to_host_client_negative_origin() {
        // Secondary monitor LEFT of primary: SM_XVIRTUALSCREEN is negative.
        // A window at screen x=-1000 should map to client x=0 when the host
        // origin is also -1000.
        let screen = Rect {
            x: -1000,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let client = screen_to_host_client(screen, (-1000, 0));
        assert_eq!(client.x, 0);
        assert_eq!(client.y, 0);
        assert_eq!(client.width, 1920);
        assert_eq!(client.height, 1080);

        // A window on the primary (at screen x=0) with the host at x=-1000
        // should map to client x=1000.
        let primary = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let client2 = screen_to_host_client(primary, (-1000, 0));
        assert_eq!(client2.x, 1000);
        assert_eq!(client2.y, 0);
    }

    #[test]
    fn test_is_ghost_animation_class_str() {
        assert!(is_ghost_animation_class_str("Chrome_WidgetWin_1"));
        assert!(is_ghost_animation_class_str("Chrome_WidgetWin_2"));
        assert!(is_ghost_animation_class_str("Chrome_WidgetWin_100"));
        assert!(is_ghost_animation_class_str("MozillaWindowClass"));
        assert!(is_ghost_animation_class_str(
            "CASCADIA_HOSTING_WINDOW_CLASS"
        ));
        // .NET Framework: WinForms and WPF top-level windows.
        assert!(is_ghost_animation_class_str(
            "WindowsForms10.Window.8.app.0.1a2b3c"
        ));
        assert!(is_ghost_animation_class_str(
            "HwndWrapper[MyApp.exe;;abc-123]"
        ));
        assert!(is_compositor_sensitive_class_str("CabinetWClass"));
        assert!(is_compositor_sensitive_class_str("ExploreWClass"));
        assert!(is_compositor_sensitive_class_str("ApplicationFrameWindow"));
        assert!(is_compositor_sensitive_class_str(
            "WinUIDesktopWin32WindowClass"
        ));
        assert!(is_compositor_sensitive_class_str("CEF-OSC-WIDGET"));
        assert!(is_compositor_sensitive_class_str("Qt6QWindowIcon"));

        assert!(is_ghost_animation_class_str("Notepad"));
        assert!(!is_ghost_animation_class_str(""));
        assert!(!is_ghost_animation_class_str("Chrome_RenderWidgetHostHWND")); // internal widget; skipped earlier
        assert!(!is_ghost_animation_class_str("Chrome_Widget")); // prefix-only match avoided
        assert!(!is_ghost_animation_class_str("CASCADIA")); // partial match avoided
        assert!(!is_ghost_animation_class_str("WindowsForms")); // needs the version + dot
    }

    #[test]
    fn test_register_balance_starts_at_zero() {
        // Process-global; may have non-zero state from other tests in the
        // same binary, but we can at least observe the read API.
        let _initial = current_register_balance();
    }
}

#[cfg(test)]
mod persistent_preview_geometry_tests {
    use super::{normalized_preview_geometry, PersistentPreviewRequest};
    use leopardwm_core_layout::Rect;

    #[test]
    fn matching_source_size_preserves_crop_and_destination() {
        let request = PersistentPreviewRequest {
            window_id: 1,
            source_rect: Rect::new(500, 0, 250, 800),
            expected_source_size: (750, 800),
            destination_screen_rect: Rect::new(1000, 0, 250, 800),
        };
        assert_eq!(
            normalized_preview_geometry(request, (750, 800)),
            Some((request.source_rect, request.destination_screen_rect))
        );
    }

    #[test]
    fn crop_scales_to_the_actual_dwm_source_size() {
        let request = PersistentPreviewRequest {
            window_id: 1,
            source_rect: Rect::new(500, 80, 250, 640),
            expected_source_size: (750, 800),
            destination_screen_rect: Rect::new(1000, 80, 250, 640),
        };
        assert_eq!(
            normalized_preview_geometry(request, (1500, 1600)),
            Some((
                Rect::new(1000, 160, 500, 1280),
                request.destination_screen_rect,
            ))
        );
    }

    #[test]
    fn invalid_or_empty_source_geometry_is_rejected() {
        let request = PersistentPreviewRequest {
            window_id: 1,
            source_rect: Rect::new(750, 0, 0, 800),
            expected_source_size: (750, 800),
            destination_screen_rect: Rect::new(1000, 0, 0, 800),
        };
        assert_eq!(normalized_preview_geometry(request, (750, 800)), None);
    }
}
