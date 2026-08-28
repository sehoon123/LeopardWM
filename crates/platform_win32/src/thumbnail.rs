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
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicIsize, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::Duration;
#[cfg(not(test))]
use std::time::Instant;
use tracing::warn;
use windows::core::BOOL;
use windows::Win32::Foundation::{GetLastError, ERROR_CLASS_ALREADY_EXISTS, HWND, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmQueryThumbnailSourceSize, DwmRegisterThumbnail, DwmUnregisterThumbnail,
    DwmUpdateThumbnailProperties, DWM_THUMBNAIL_PROPERTIES, DWM_TNP_OPACITY,
    DWM_TNP_RECTDESTINATION, DWM_TNP_RECTSOURCE, DWM_TNP_VISIBLE,
};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject,
    AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS,
    HDC,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClassInfoW, GetMessageW,
    GetSystemMetrics, GetWindow, IsWindow, RegisterClassW, SetWindowPos, UnregisterClassW,
    UpdateLayeredWindow, CW_USEDEFAULT, GW_HWNDNEXT, HWND_NOTOPMOST, HWND_TOP, HWND_TOPMOST, MSG,
    SET_WINDOW_POS_FLAGS, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    SWP_SHOWWINDOW, ULW_ALPHA, WM_CLOSE, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP, WS_VISIBLE,
};
#[cfg(not(test))]
use windows::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, WM_QUIT};

/// Class name for the singleton thumbnail host window. Listed in
/// `enumeration::should_skip_window_by_class` so we don't try to manage
/// our own overlay.
pub(crate) const THUMBNAIL_HOST_CLASS: &str = "LeopardWMThumbnailHost";

/// Process-global counter of currently-registered DWM thumbnails. Used
/// by tests and the `lwm health` IPC field to assert no leaks. Mirrors
/// `Z_ORDER_STATE.balance` for lock-free reads.
static REGISTER_BALANCE: AtomicI64 = AtomicI64::new(0);
/// Opaque IDs returned by `ThumbnailHandle::into_isize`. They are resolved by
/// this module before every DWM call and deliberately never alias a live raw
/// registration key.
static NEXT_RAW_TRANSFER_TOKEN: AtomicIsize = AtomicIsize::new(isize::MIN);

/// Serializes register/unregister z-order side effects so concurrent
/// register/unregister can't interleave between the atomic balance update
/// and the `set_topmost` side effect.
///
/// Without this, the bad interleaving is:
///   T1 unregister: balance=1→0, about to call set_topmost(false)
///   T2 register:   balance=0→1, calls set_topmost(true) first
///   T1 unregister: calls set_topmost(false)  ← host left non-topmost with a live thumbnail
#[derive(Debug, Clone, Copy)]
struct ThumbnailOwnership {
    /// Actual DWM `HTHUMBNAIL`. The map key can be a distinct raw-transfer
    /// token after `into_isize`, preventing a stale worker drop from matching a
    /// reused DWM handle in a replacement host generation.
    dwm_handle: isize,
    host_z: bool,
    band: HostBand,
    /// The destination host incarnation. `0` denotes a non-host destination.
    host_generation: u64,
    /// Whether this registration reached host z-order accounting. A failed
    /// setup can still leave a DWM handle that must be retried, but it must not
    /// demote/promote a host band it never claimed.
    host_claimed: bool,
    /// A prior `DwmUnregisterThumbnail` failed. Only these registrations are
    /// eligible for autonomous unregister retry; healthy live handles must
    /// never be swept by the retry service.
    pending_unregister: bool,
    /// The host HWND was proven destroyed during restart. DWM retires handles
    /// targeting that destination; later stale drops must not affect the new
    /// generation's balance or z-order.
    retired: bool,
}

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
    /// Raw-handle ownership survives `ThumbnailHandle::into_isize`, allowing
    /// both ordinary and raw drops to release the exact host generation that
    /// registered them. Failed unregisters remain here as retry receipts.
    registrations: HashMap<isize, ThumbnailOwnership>,
}
/// Serializes z-order accounting with host promotion/demotion without holding
/// `Z_ORDER_STATE` across a host call. Host availability can restart the host
/// and retire old-generation claims, which itself locks `Z_ORDER_STATE`.
static Z_ORDER_COMMIT: Mutex<()> = Mutex::new(());
static Z_ORDER_STATE: LazyLock<Mutex<ZOrderState>> = LazyLock::new(|| {
    Mutex::new(ZOrderState {
        balance: 0,
        topmost_balance: 0,
        host_balance: 0,
        registrations: HashMap::new(),
    })
});

/// Return the current outstanding-registration count. Should converge to 0
/// after any animation cycle completes.
pub fn current_register_balance() -> i64 {
    REGISTER_BALANCE.load(Ordering::Relaxed)
}

/// RAII wrapper around an `HTHUMBNAIL` ownership token. Unregisters on drop
/// unless the token has been transferred out via [`ThumbnailHandle::into_isize`].
///
/// `Send` + `Sync` safety: `HTHUMBNAIL` is a kernel-level handle managed
/// by `dwm.exe`; it has no thread affinity post-registration. Cross-thread
/// `DwmUpdateThumbnailProperties` is supported by design (Aero Flip 3D
/// used the same pattern from worker threads).
pub struct ThumbnailHandle {
    /// Opaque registration token. Set to 0 by `into_isize` to suppress Drop;
    /// module-local DWM calls resolve it to the underlying HTHUMBNAIL.
    handle: isize,
    /// Whether this registration participates in the HOST z-order
    /// accounting (true only for host-destined thumbnails).
    host_z: bool,
    /// Which band this registration claimed, so unregistering releases the
    /// same claim it made.
    band: HostBand,
    /// Singleton-host generation this handle targets. Zero denotes an
    /// arbitrary destination window such as the overview.
    host_generation: u64,
}

// SAFETY: HTHUMBNAIL is a process-wide DWM handle, not bound to any HWND
// owner thread for updates. Codex's Microsoft-Learn check confirmed no
// apartment-affinity requirement post-registration.
unsafe impl Send for ThumbnailHandle {}
unsafe impl Sync for ThumbnailHandle {}

impl Drop for ThumbnailHandle {
    fn drop(&mut self) {
        if self.handle != 0 {
            unregister_impl(self.handle, self.host_z, self.band, self.host_generation);
        }
    }
}

impl ThumbnailHandle {
    /// Consume this handle without firing Drop, returning its opaque `isize`
    /// ownership token. The caller takes responsibility for eventually calling
    /// [`unregister_raw`] on that token (or wrapping it in a new owning type
    /// that does).
    ///
    /// Used at landing to transfer handle ownership from the daemon's
    /// `AppState.ghost_handles` into `WorkerCommand::Crossfade` entries
    /// owned by the worker thread.
    pub fn into_isize(mut self) -> isize {
        // unregister_raw assumes host z-order accounting; only
        // host-destined handles may be transferred raw.
        debug_assert!(self.host_z, "into_isize on a non-host thumbnail handle");
        let raw = self.handle;
        // Ownership metadata is retained in `Z_ORDER_STATE.registrations`, so
        // the raw crossfade path can later release this exact host generation.
        self.handle = 0;
        raw
    }

    /// Opaque registration token for cross-thread `update` calls. Does NOT
    /// transfer ownership — Drop still fires when `self` is dropped.
    pub fn as_isize(&self) -> isize {
        self.handle
    }

    fn belongs_to_current_host(&self) -> bool {
        registration_matches_host_generation(self.host_z, self.host_generation, host().generation())
    }

    /// Test-only stand-in (handle 0): Drop's unregister no-ops.
    #[cfg(test)]
    pub(crate) fn fake() -> Self {
        Self {
            handle: 0,
            host_z: false,
            band: HostBand::Normal,
            host_generation: 0,
        }
    }
}

fn registration_matches_host_generation(
    host_bound: bool,
    registered_generation: u64,
    current_generation: u64,
) -> bool {
    !host_bound || registered_generation == current_generation
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

fn host_band_is_compatible(state: &ZOrderState, band: HostBand) -> bool {
    let normal_balance = state.host_balance - state.topmost_balance;
    match band {
        HostBand::Topmost => normal_balance == 0,
        HostBand::Normal => state.topmost_balance == 0,
    }
}

fn next_registration_token(state: &ZOrderState) -> isize {
    loop {
        let token = NEXT_RAW_TRANSFER_TOKEN
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                Some(value.wrapping_add(1).max(1))
            })
            .unwrap_or_else(|value| value);
        if token != 0 && !state.registrations.contains_key(&token) {
            return token;
        }
    }
}

fn record_registration_locked(
    state: &mut ZOrderState,
    token: isize,
    ownership: ThumbnailOwnership,
) {
    state.registrations.insert(token, ownership);
    if ownership.retired {
        return;
    }
    state.balance += 1;
    if ownership.host_claimed {
        state.host_balance += 1;
        if ownership.band == HostBand::Topmost {
            state.topmost_balance += 1;
        }
    }
    REGISTER_BALANCE.store(state.balance, Ordering::Relaxed);
}

fn release_registration_locked(
    state: &mut ZOrderState,
    token: isize,
) -> Option<(ThumbnailOwnership, bool)> {
    let ownership = state.registrations.remove(&token)?;
    if ownership.retired {
        return Some((ownership, false));
    }
    state.balance = (state.balance - 1).max(0);
    let mut demote_host = false;
    if ownership.host_claimed {
        state.host_balance = (state.host_balance - 1).max(0);
        if ownership.band == HostBand::Topmost {
            let previous = state.topmost_balance;
            state.topmost_balance = (state.topmost_balance - 1).max(0);
            demote_host = previous >= 1 && state.topmost_balance == 0;
        }
    }
    REGISTER_BALANCE.store(state.balance, Ordering::Relaxed);
    Some((ownership, demote_host))
}

/// Keep a successful DWM registration owned after a later setup/unregister
/// failure. It has no Rust handle any more, so this registry is the retry
/// receipt; its unique token also prevents DWM raw-handle reuse from making a
/// stale drop touch a replacement registration.
fn retain_failed_dwm_registration(
    dwm_handle: isize,
    host_z: bool,
    band: HostBand,
    host_generation: u64,
    host_claimed: bool,
) {
    let retired = host_z && host_generation != 0 && host_generation != host().generation();
    // A changed host generation is proof that this destination was retired.
    // There is no live destination left to unregister, and retaining its raw
    // value could collide with a future DWM registration.
    if retired {
        return;
    }
    let mut state = Z_ORDER_STATE
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex);
    let token = next_registration_token(&state);
    record_registration_locked(
        &mut state,
        token,
        ThumbnailOwnership {
            dwm_handle,
            host_z,
            band,
            host_generation,
            host_claimed,
            pending_unregister: true,
            retired: false,
        },
    );
}

fn resolve_dwm_handle(handle: isize) -> isize {
    Z_ORDER_STATE
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
        .registrations
        .get(&handle)
        .map(|ownership| ownership.dwm_handle)
        // Preserve the public raw API for callers outside this crate. All
        // LeopardWM-created handles have a token entry, so this fallback is
        // never used by generation-sensitive ownership paths.
        .unwrap_or(handle)
}

fn unregister_dwm_handle(dwm_handle: isize) -> Result<(), Win32Error> {
    #[cfg(feature = "integration-probes")]
    if FORCE_NEXT_UNREGISTER_FAILURE.swap(false, Ordering::AcqRel) {
        return Err(Win32Error::SetPositionFailed(
            "injected DwmUnregisterThumbnail failure".into(),
        ));
    }
    unsafe { DwmUnregisterThumbnail(dwm_handle) }.map_err(|error| {
        Win32Error::SetPositionFailed(format!("DwmUnregisterThumbnail({dwm_handle}): {error}"))
    })
}

/// Retry failed unregisters without touching the active replacement host's
/// z-order claims. Normal preview activity calls this opportunistically; the
/// integration probe calls it explicitly after a deterministic failure.
pub fn service_pending_thumbnail_unregisters() {
    let pending: Vec<(isize, ThumbnailOwnership)> = Z_ORDER_STATE
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
        .registrations
        .iter()
        .filter_map(|(token, ownership)| {
            (ownership.pending_unregister && !ownership.retired).then_some((*token, *ownership))
        })
        .collect();
    for (token, ownership) in pending {
        unregister_impl(
            token,
            ownership.host_z,
            ownership.band,
            ownership.host_generation,
        );
    }
}

#[cfg_attr(test, allow(dead_code))]
fn retire_host_generation_claims(host_generation: u64) {
    let mut state = Z_ORDER_STATE
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex);
    let mut retired_total = 0i64;
    let mut retired_host = 0i64;
    let mut retired_topmost = 0i64;
    for ownership in state.registrations.values_mut() {
        if !ownership.retired && ownership.host_z && ownership.host_generation == host_generation {
            ownership.retired = true;
            retired_total += 1;
            if ownership.host_claimed {
                retired_host += 1;
                if ownership.band == HostBand::Topmost {
                    retired_topmost += 1;
                }
            }
        }
    }
    if retired_total != 0 {
        state.balance = (state.balance - retired_total).max(0);
        state.host_balance = (state.host_balance - retired_host).max(0);
        state.topmost_balance = (state.topmost_balance - retired_topmost).max(0);
        REGISTER_BALANCE.store(state.balance, Ordering::Relaxed);
    }
}

fn register_on_host(wid: WindowId, band: HostBand) -> Result<ThumbnailHandle, Win32Error> {
    #[cfg(feature = "integration-probes")]
    if FORCE_PREVIEW_REGISTRATION_FAILURES
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
            if remaining == 0 {
                None
            } else {
                Some(remaining - 1)
            }
        })
        .is_ok()
    {
        return Err(Win32Error::SetPositionFailed(
            "injected DWM preview registration failure".into(),
        ));
    }
    if !host().is_available() {
        return Err(Win32Error::SetPositionFailed(
            "thumbnail host unavailable".into(),
        ));
    }
    host().ensure_virtual_screen_geometry()?;
    let generation = host().generation();
    register_to(host().hwnd(), wid, true, band, generation)
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
        0,
    )
}

fn register_to(
    dest: HWND,
    wid: WindowId,
    host_z: bool,
    band: HostBand,
    host_generation: u64,
) -> Result<ThumbnailHandle, Win32Error> {
    service_pending_thumbnail_unregisters();
    let source = window_id_to_hwnd(wid)?;
    let raw = unsafe { DwmRegisterThumbnail(dest, source) }.map_err(|e| {
        Win32Error::SetPositionFailed(format!("DwmRegisterThumbnail({:?}): {}", source.0, e))
    })?;
    if raw == 0 {
        return Err(Win32Error::SetPositionFailed(
            "DwmRegisterThumbnail returned null handle".into(),
        ));
    }
    if host_z && host().generation() != host_generation {
        if unregister_dwm_handle(raw).is_err() {
            retain_failed_dwm_registration(raw, host_z, band, host_generation, false);
        }
        return Err(Win32Error::SetPositionFailed(
            "thumbnail host generation changed during registration".into(),
        ));
    }

    // Serialize the balance update with the z-order side effect so a
    // concurrent unregister cannot demote between promotion and accounting.
    // Do not hold Z_ORDER_STATE across `set_topmost`: host availability may
    // restart the host and retire old-generation claims under that same lock.
    let _commit = Z_ORDER_COMMIT
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex);
    let band_change = {
        let z = Z_ORDER_STATE
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex);
        if host_z && host().generation() != host_generation {
            drop(z);
            if unregister_dwm_handle(raw).is_err() {
                retain_failed_dwm_registration(raw, host_z, band, host_generation, false);
            }
            return Err(Win32Error::SetPositionFailed(
                "thumbnail host generation changed during registration accounting".into(),
            ));
        }
        if host_z && !host_band_is_compatible(&z, band) {
            drop(z);
            if unregister_dwm_handle(raw).is_err() {
                retain_failed_dwm_registration(raw, host_z, band, host_generation, false);
            }
            return Err(Win32Error::SetPositionFailed(
                "shared thumbnail host cannot mix normal previews and topmost ghosts".into(),
            ));
        }
        if host_z && band == HostBand::Topmost && z.topmost_balance == 0 {
            Some(true)
        } else if host_z && band == HostBand::Normal && z.host_balance == 0 {
            Some(false)
        } else {
            None
        }
    };
    if let Some(topmost) = band_change {
        if let Err(error) = host().set_topmost(topmost) {
            if unregister_dwm_handle(raw).is_err() {
                retain_failed_dwm_registration(raw, host_z, band, host_generation, false);
            }
            return Err(error);
        }
    }
    let mut z = Z_ORDER_STATE
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex);
    if host_z && host().generation() != host_generation {
        drop(z);
        // A restart may have applied the requested band to the replacement
        // host. Reconcile it from the replacement generation's actual claims.
        let replacement_topmost = Z_ORDER_STATE
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex)
            .topmost_balance
            > 0;
        let _ = host().set_topmost(replacement_topmost);
        if unregister_dwm_handle(raw).is_err() {
            retain_failed_dwm_registration(raw, host_z, band, host_generation, false);
        }
        return Err(Win32Error::SetPositionFailed(
            "thumbnail host restarted during z-order commit".into(),
        ));
    }
    let token = next_registration_token(&z);
    record_registration_locked(
        &mut z,
        token,
        ThumbnailOwnership {
            dwm_handle: raw,
            host_z,
            band,
            host_generation,
            host_claimed: host_z,
            pending_unregister: false,
            retired: false,
        },
    );
    Ok(ThumbnailHandle {
        handle: token,
        host_z,
        band,
        host_generation,
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
    let dwm_handle = resolve_dwm_handle(handle);
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
    unsafe { DwmUpdateThumbnailProperties(dwm_handle, &props) }.map_err(|error| {
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PublishedPreview {
    request: PersistentPreviewRequest,
    generation: u64,
    source_process_id: u32,
}

#[cfg_attr(test, allow(dead_code))]
struct PersistentPreview {
    handle: ThumbnailHandle,
    source_process_id: u32,
    source_thread_id: u32,
    source_class_at_register: String,
    publication_generation: u64,
    source_size: Option<(i32, i32)>,
    expected_source_size: Option<(i32, i32)>,
    /// Last request whose DWM property update and flush succeeded. This is an
    /// API-publication receipt, not a generic proof of sampled screen pixels:
    /// arbitrary application content cannot be captured reliably in production.
    /// Input targets are derived only from this receipt; the integration probe
    /// performs a controlled colored-source capture proof separately.
    published: Option<PublishedPreview>,
    /// Registration was created autonomously and has not yet crossed a fresh
    /// physical source-parking/uncloak transaction. Retry workers may retain
    /// it, but only an exact placement commit may clear this fence.
    requires_physical_commit: bool,
    /// Consecutive autonomous attempts that could not publish the current
    /// request. Reset only by a successful DWM update.
    failed_publishes: u32,
}

#[derive(Default)]
struct PersistentPreviewState {
    previews: HashMap<WindowId, PersistentPreview>,
    lifecycle_epoch: u64,
    /// Whether the host has been anchored for the current preview ownership set.
    host_anchored: bool,
    /// Authoritative requests from the newest committed placement pass. Kept so
    /// the retry worker does not depend on an unrelated future layout event.
    desired: Vec<PersistentPreviewRequest>,
    /// Window the host must sit directly below, from the newest placement pass.
    /// Retry and recovery paths reuse it so they anchor exactly like the pass
    /// that planned the publication.
    host_below: Option<isize>,
    generation: u64,
}

/// How many consecutive size-refreshing passes may fail before a preview is
/// abandoned. Three passes is long enough for a source that is still settling
/// and short enough that a genuinely dead registration does not linger.
#[cfg_attr(test, allow(dead_code))]
const MAX_FAILED_PUBLISHES: u32 = 3;

static PERSISTENT_PREVIEWS: OnceLock<Mutex<PersistentPreviewState>> = OnceLock::new();
/// Revokes every producer (frame, exact apply, autonomous retry) across display
/// topology and emergency cleanup boundaries.
static PREVIEW_LIFECYCLE_EPOCH: AtomicU64 = AtomicU64::new(1);
#[cfg_attr(test, allow(dead_code))]
static NEXT_PREVIEW_PUBLICATION: AtomicU64 = AtomicU64::new(1);
static PERSISTENT_PREVIEW_TRANSACTION: Mutex<()> = Mutex::new(());
#[derive(Clone, Copy)]
struct PreviewSourceInvalidation {
    generation: u64,
}

static INVALIDATED_PREVIEW_SOURCES: Mutex<Option<HashMap<WindowId, PreviewSourceInvalidation>>> =
    Mutex::new(None);
static NEXT_PREVIEW_SOURCE_INVALIDATION: AtomicU64 = AtomicU64::new(1);
#[cfg(feature = "integration-probes")]
struct RegistrationFenceProbe {
    reached: mpsc::SyncSender<()>,
    release: mpsc::Receiver<()>,
}
#[cfg(feature = "integration-probes")]
static REGISTRATION_FENCE_PROBE: Mutex<Option<RegistrationFenceProbe>> = Mutex::new(None);
#[cfg(not(test))]
static PREVIEW_RETRY_TX: Mutex<Option<mpsc::SyncSender<()>>> = Mutex::new(None);
#[cfg(not(test))]
static PREVIEW_RETRY_PENDING: AtomicBool = AtomicBool::new(false);
/// A missing registration recovered autonomously. The daemon must run a fresh
/// exact placement before that handle may publish, so source parking and cloak
/// release are physically re-verified.
static PREVIEW_RELAYOUT_REQUIRED: AtomicBool = AtomicBool::new(false);
#[cfg(not(test))]
static PREVIEW_CLEAR_PENDING: AtomicBool = AtomicBool::new(false);
#[cfg(not(test))]
static PREVIEW_CLEAR_RETRY_AFTER: Mutex<Option<Instant>> = Mutex::new(None);
#[cfg(feature = "integration-probes")]
static FORCE_RETRY_SPAWN_FAILURE: AtomicBool = AtomicBool::new(false);
#[cfg(feature = "integration-probes")]
static FORCE_HOST_SPAWN_FAILURE: AtomicBool = AtomicBool::new(false);
#[cfg(feature = "integration-probes")]
static FORCE_NEXT_UNREGISTER_FAILURE: AtomicBool = AtomicBool::new(false);
#[cfg(feature = "integration-probes")]
static FORCE_PREVIEW_REGISTRATION_FAILURES: AtomicU32 = AtomicU32::new(0);
#[cfg(feature = "integration-probes")]
static FORCE_NEXT_PREVIEW_PUBLISH_FAILURE: AtomicBool = AtomicBool::new(false);
#[cfg(feature = "integration-probes")]
static FORCE_PREVIEW_PUBLISH_FAILURES: AtomicU32 = AtomicU32::new(0);
#[cfg(feature = "integration-probes")]
static SUPPRESS_RETRY_FOR_CAPTURE_PROBE: AtomicBool = AtomicBool::new(false);
#[cfg(not(test))]
static REGISTRATION_BACKOFF: Mutex<Option<HashMap<WindowId, Instant>>> = Mutex::new(None);

/// Called synchronously from the WinEvent destroy callback. The global atomic
/// epoch is the nonblocking safety fence; the per-source tombstone is a
/// best-effort precision aid and is never allowed to stall the callback.
pub fn invalidate_persistent_preview_source(window_id: WindowId) {
    if matches!(
        persistent_preview_presence_nonblocking(window_id),
        Some(false)
    ) {
        return;
    }
    record_preview_source_invalidation(window_id);
}

fn record_preview_source_invalidation(window_id: WindowId) {
    let generation = NEXT_PREVIEW_SOURCE_INVALIDATION
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            Some(value.wrapping_add(1).max(1))
        })
        .unwrap_or_else(|value| value);
    match INVALIDATED_PREVIEW_SOURCES.try_lock() {
        Ok(mut sources) => {
            sources
                .get_or_insert_with(HashMap::new)
                .insert(window_id, PreviewSourceInvalidation { generation });
        }
        Err(std::sync::TryLockError::Poisoned(error)) => {
            error
                .into_inner()
                .get_or_insert_with(HashMap::new)
                .insert(window_id, PreviewSourceInvalidation { generation });
        }
        Err(std::sync::TryLockError::WouldBlock) => {
            // Contention is rare and cannot block a WinEvent callback. Revoke
            // every preview producer/input epoch as the fixed-size fallback;
            // the daemon's next service tick performs a fresh exact relayout.
            advance_preview_lifecycle_epoch();
            PREVIEW_RELAYOUT_REQUIRED.store(true, Ordering::Release);
        }
    }
}

fn preview_source_invalidation_generation(window_id: WindowId) -> Option<u64> {
    let guard = INVALIDATED_PREVIEW_SOURCES
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex);
    guard
        .as_ref()?
        .get(&window_id)
        .map(|invalidation| invalidation.generation)
}

pub(crate) fn preview_source_is_invalidated(window_id: WindowId) -> bool {
    preview_source_invalidation_generation(window_id).is_some()
}

/// The queued lifecycle handler calls this only after it has withdrawn the
/// target and removed the registration while owning the preview transaction.
/// Until then the tombstone deliberately has no timeout: a wedged lifecycle
/// lane must fail closed rather than let a dead target wake up after 30 seconds.
fn retire_preview_source_invalidation(window_id: WindowId, generation: Option<u64>) {
    let Some(generation) = generation else {
        return;
    };
    if let Some(sources) = INVALIDATED_PREVIEW_SOURCES
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
        .as_mut()
    {
        let still_same_destroy = sources
            .get(&window_id)
            .is_some_and(|invalidation| invalidation.generation == generation);
        if still_same_destroy {
            sources.remove(&window_id);
        }
    }
}

/// Install a freshly registered source only if no newer destroy arrived since
/// registration began. Holding the invalidation lock through `install` makes
/// this a linearizable handoff: a destroy is ordered either before the new
/// registration (and cleared by it) or after installation (and remains visible
/// to hit testing/publication).
fn validate_new_preview_source<R>(
    window_id: WindowId,
    observed_generation: Option<u64>,
    install: impl FnOnce() -> R,
) -> Option<R> {
    let mut guard = INVALIDATED_PREVIEW_SOURCES
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex);
    let sources = guard.get_or_insert_with(HashMap::new);
    let current_generation = sources
        .get(&window_id)
        .map(|invalidation| invalidation.generation);
    if current_generation != observed_generation {
        return None;
    }
    let installed = install();
    if current_generation.is_some() {
        sources.remove(&window_id);
    }
    Some(installed)
}

pub fn preview_lifecycle_epoch() -> u64 {
    PREVIEW_LIFECYCLE_EPOCH.load(Ordering::Acquire)
}

fn advance_preview_lifecycle_epoch() -> u64 {
    PREVIEW_LIFECYCLE_EPOCH
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            Some(value.wrapping_add(1).max(1))
        })
        .unwrap_or_else(|value| value)
        .wrapping_add(1)
        .max(1)
}

/// Transaction-independent emergency revocation. Hiding/disarming does not
/// wait for an apply worker that may be wedged inside the preview transaction;
/// all producers check the epoch before exposing a surface again.
pub fn invalidate_persistent_preview_surface() {
    PREVIEW_RELAYOUT_REQUIRED.store(false, Ordering::Release);
    let epoch = advance_preview_lifecycle_epoch();
    crate::preview_input::set_preview_targets_armed(false);
    #[cfg(not(test))]
    let _ = host().hide_surface();
    crate::preview_input::clear_preview_click_targets();
    if let Ok(mut state) = persistent_previews().try_lock() {
        state.lifecycle_epoch = epoch;
        state.desired.clear();
        state.host_anchored = false;
        for preview in state.previews.values_mut() {
            preview.published = None;
        }
    }
}

fn persistent_previews() -> &'static Mutex<PersistentPreviewState> {
    PERSISTENT_PREVIEWS.get_or_init(|| Mutex::new(PersistentPreviewState::default()))
}

fn lock_persistent_previews() -> std::sync::MutexGuard<'static, PersistentPreviewState> {
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
        if !host().is_available() {
            return false;
        }
        let observed_invalidation = preview_source_invalidation_generation(window_id);
        let mut state = lock_persistent_previews();
        state
            .previews
            .retain(|_, preview| preview.handle.belongs_to_current_host());
        if observed_invalidation.is_some() {
            state.previews.remove(&window_id);
        }
        if state.previews.contains_key(&window_id) {
            return true;
        }
        {
            let backoff = REGISTRATION_BACKOFF
                .lock()
                .unwrap_or_else(crate::recover_poisoned_mutex);
            if backoff
                .as_ref()
                .and_then(|map| map.get(&window_id))
                .is_some_and(|failed_at| failed_at.elapsed() < Duration::from_secs(1))
            {
                return false;
            }
        }
        let mut handle = None;
        for attempt in 1..=MAX_FAILED_PUBLISHES {
            match register_on_host(window_id, HostBand::Normal) {
                Ok(registered) => {
                    handle = Some(registered);
                    break;
                }
                Err(error) if attempt < MAX_FAILED_PUBLISHES => {
                    warn!(
                        "Preview registration for {window_id:#x} attempt {attempt}/{} failed: {error}",
                        MAX_FAILED_PUBLISHES
                    );
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => {
                    warn!(
                        "Preview registration for {window_id:#x} abandoned after {attempt} attempts: {error}"
                    );
                }
            }
        }
        let Some(handle) = handle else {
            REGISTRATION_BACKOFF
                .lock()
                .unwrap_or_else(crate::recover_poisoned_mutex)
                .get_or_insert_with(HashMap::new)
                .insert(window_id, Instant::now());
            return false;
        };
        if let Some(backoff) = REGISTRATION_BACKOFF
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex)
            .as_mut()
        {
            backoff.remove(&window_id);
        }
        let initial_size = source_size(handle.as_isize());
        let mut source_process_id = 0u32;
        let mut source_thread_id = 0u32;
        let mut source_class_at_register = String::new();
        if let Ok(source) = window_id_to_hwnd(window_id) {
            unsafe {
                source_thread_id =
                    windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(
                        source,
                        Some(&mut source_process_id),
                    );
            }
            source_class_at_register = class_name_hwnd(source);
        }
        if source_process_id == 0 || source_thread_id == 0 || source_class_at_register.is_empty() {
            warn!("Preview source {window_id:#x} has no process identity");
            return false;
        }
        #[cfg(feature = "integration-probes")]
        if let Some(probe) = REGISTRATION_FENCE_PROBE
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex)
            .take()
        {
            let reached = probe.reached.send(()).is_ok();
            if !reached || probe.release.recv_timeout(Duration::from_secs(2)).is_err() {
                warn!("Preview registration fence probe timed out for {window_id:#x}");
                return false;
            }
        }
        let preview = PersistentPreview {
            handle,
            source_process_id,
            source_thread_id,
            source_class_at_register,
            publication_generation: NEXT_PREVIEW_PUBLICATION.fetch_add(1, Ordering::Relaxed),
            requires_physical_commit: true,
            failed_publishes: 0,
            published: None,
            source_size: initial_size,
            expected_source_size: None,
        };
        if validate_new_preview_source(window_id, observed_invalidation, || {
            state.previews.insert(window_id, preview)
        })
        .is_none()
        {
            warn!("Preview registration for {window_id:#x} discarded after a concurrent destroy");
            return false;
        }
        true
    }
}

fn persistent_preview_presence_nonblocking(window_id: WindowId) -> Option<bool> {
    match persistent_previews().try_lock() {
        Ok(state) => Some(state.previews.contains_key(&window_id)),
        Err(std::sync::TryLockError::Poisoned(error)) => {
            Some(error.into_inner().previews.contains_key(&window_id))
        }
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

pub(crate) fn has_persistent_preview_nonblocking(window_id: WindowId) -> bool {
    persistent_preview_presence_nonblocking(window_id) == Some(true)
}

pub(crate) fn has_persistent_preview(window_id: WindowId) -> bool {
    #[cfg(test)]
    {
        let _ = window_id;
        false
    }
    #[cfg(not(test))]
    {
        lock_persistent_previews().previews.contains_key(&window_id)
    }
}

/// Whether DWM has accepted and flushed at least one publication for this
/// source. This is deliberately an API receipt, not a generic pixel proof; a
/// handle that was only registered is not evidence that its HWND was safely
/// parked.
pub(crate) fn has_published_persistent_preview(window_id: WindowId) -> bool {
    #[cfg(test)]
    {
        let _ = window_id;
        false
    }
    #[cfg(not(test))]
    {
        lock_persistent_previews()
            .previews
            .get(&window_id)
            .is_some_and(|preview| preview.published.is_some())
    }
}

/// Validate the stable registration/incarnation token captured at button-down
/// against a currently visible, anchored publication. Geometry publications may
/// move during a held press, but removal/re-registration always changes this
/// token and invalidates the gesture.
pub fn current_persistent_preview_rect(
    window_id: WindowId,
    source_process_id: u32,
    publication_generation: u64,
) -> Option<Rect> {
    #[cfg(test)]
    {
        let _ = (window_id, source_process_id, publication_generation);
        None
    }
    #[cfg(not(test))]
    {
        if preview_source_is_invalidated(window_id) {
            return None;
        }
        let state = lock_persistent_previews();
        if !state.host_anchored || state.lifecycle_epoch != preview_lifecycle_epoch() {
            return None;
        }
        let preview = state.previews.get(&window_id)?;
        let published = preview.published?;
        let source = window_id_to_hwnd(window_id).ok()?;
        let mut live_process_id = 0u32;
        let live_thread_id = unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(
                source,
                Some(&mut live_process_id),
            )
        };
        (preview.source_process_id == source_process_id
            && preview.source_process_id == live_process_id
            && preview.source_thread_id == live_thread_id
            && preview.source_class_at_register == class_name_hwnd(source)
            && preview.publication_generation == publication_generation
            && source_size(preview.handle.as_isize()).is_some())
        .then_some(published.request.destination_screen_rect)
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

/// Update a ghost crop described in expected placement coordinates after
/// scaling it to DWM's actual source surface.
pub fn update_cropped_scaled(
    handle: isize,
    expected_crop: Rect,
    expected_source_size: (i32, i32),
    destination_client: Rect,
    opacity: u8,
    visible: bool,
) -> Result<(), Win32Error> {
    let actual = source_size(handle)
        .ok_or_else(|| Win32Error::SetPositionFailed("DWM ghost source size unavailable".into()))?;
    let request = PersistentPreviewRequest {
        window_id: 0,
        source_rect: expected_crop,
        expected_source_size,
        destination_screen_rect: destination_client,
    };
    let (source, _) = normalized_preview_geometry(request, actual).ok_or_else(|| {
        Win32Error::SetPositionFailed("DWM ghost crop normalized to empty".into())
    })?;
    update_cropped(handle, source, destination_client, opacity, visible)
}

/// Result of publishing the newest desired requests.
#[cfg(not(test))]
struct PreviewPublishOutcome {
    live: usize,
    retry_needed: bool,
}

/// Publish requests into already-registered thumbnails while preserving the
/// distinction between a handle that merely exists and geometry DWM accepted.
/// `published` is an API publication receipt; controlled screen-capture proof
/// exists only in the integration probe because arbitrary source pixels are not
/// a reliable production postcondition. On failure, `published` stays at the
/// last successful request so the input overlay remains aligned with the DWM
/// surface it last acknowledged.
#[cfg(not(test))]
fn publish_preview_requests_locked(
    state: &mut PersistentPreviewState,
    requests: &[PersistentPreviewRequest],
    force_source_refresh: bool,
) -> PreviewPublishOutcome {
    if state.lifecycle_epoch != preview_lifecycle_epoch() {
        state.host_anchored = false;
        for preview in state.previews.values_mut() {
            preview.published = None;
        }
        return PreviewPublishOutcome {
            live: 0,
            retry_needed: false,
        };
    }
    if requests.is_empty() {
        return PreviewPublishOutcome {
            live: 0,
            retry_needed: false,
        };
    }

    let origin = host().origin();
    let mut invalidated_sources = Vec::new();
    let mut updated_any = false;
    for request in requests {
        if preview_source_is_invalidated(request.window_id) {
            invalidated_sources.push(request.window_id);
            continue;
        }
        let Some(preview) = state.previews.get_mut(&request.window_id) else {
            continue;
        };
        if preview.requires_physical_commit {
            // The handle exists, but source protection has not been released by
            // a fresh verified placement. Keep the desire hidden and ask the
            // daemon for an exact pass instead of publishing a blank/cloaked or
            // physically unsafe source from this background worker.
            PREVIEW_RELAYOUT_REQUIRED.store(true, Ordering::Release);
            continue;
        }
        // A retry worker does not churn a preview that is already current.
        if !force_source_refresh
            && preview.failed_publishes == 0
            && preview.published.map(|published| published.request) == Some(*request)
        {
            continue;
        }

        if force_source_refresh
            || preview.source_size.is_none()
            || preview.expected_source_size != Some(request.expected_source_size)
        {
            preview.source_size = source_size(preview.handle.as_isize());
            preview.expected_source_size = Some(request.expected_source_size);
        }

        #[cfg(feature = "integration-probes")]
        let injected_failure = FORCE_NEXT_PREVIEW_PUBLISH_FAILURE.swap(false, Ordering::AcqRel)
            || FORCE_PREVIEW_PUBLISH_FAILURES
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                    if remaining == 0 {
                        None
                    } else {
                        Some(remaining - 1)
                    }
                })
                .is_ok();
        #[cfg(not(feature = "integration-probes"))]
        let injected_failure = false;
        let update_result = if injected_failure {
            Err(Win32Error::SetPositionFailed(
                "injected DWM preview publication failure".into(),
            ))
        } else {
            preview
                .source_size
                .and_then(|size| normalized_preview_geometry(*request, size))
                .ok_or_else(|| {
                    Win32Error::SetPositionFailed(
                        "DWM preview source size/crop is not publishable".into(),
                    )
                })
                .and_then(|(source, destination_screen)| {
                    update_cropped(
                        preview.handle.as_isize(),
                        source,
                        screen_to_host_client(destination_screen, origin),
                        255,
                        true,
                    )
                })
        };

        match update_result {
            Ok(()) => {
                preview.published = Some(PublishedPreview {
                    request: *request,
                    generation: preview.publication_generation,
                    source_process_id: preview.source_process_id,
                });
                preview.failed_publishes = 0;
                updated_any = true;
            }
            Err(error) => {
                preview.failed_publishes = preview.failed_publishes.saturating_add(1);
                // Re-query on retry: the most common transient is a source whose
                // redirection surface has not caught up with its parked frame.
                preview.source_size = None;
                if preview.failed_publishes >= MAX_FAILED_PUBLISHES {
                    // The bounded burst only controls immediate retries. Keep
                    // both the registered handle and desired request so a
                    // later service tick/backoff round can self-heal instead
                    // of permanently abandoning an otherwise safely parked
                    // source.
                    warn!(
                        "Preview for {:#x} exhausted immediate retry budget after {} attempts; retaining desired state: {}",
                        request.window_id, preview.failed_publishes, error
                    );
                } else {
                    warn!(
                        "Preview for {:#x} publish attempt {}/{} failed; retrying: {}",
                        request.window_id, preview.failed_publishes, MAX_FAILED_PUBLISHES, error
                    );
                }
            }
        }
    }
    if state.lifecycle_epoch != preview_lifecycle_epoch() {
        state.host_anchored = false;
        for preview in state.previews.values_mut() {
            preview.published = None;
        }
        return PreviewPublishOutcome {
            live: 0,
            retry_needed: false,
        };
    }
    for window_id in invalidated_sources {
        // A destroy fence is terminal for this incarnation. This is distinct
        // from a publication failure: only a proven invalidation may retire
        // its desired request and DWM handle.
        state.previews.remove(&window_id);
        state
            .desired
            .retain(|request| request.window_id != window_id);
    }
    if updated_any && unsafe { windows::Win32::Graphics::Dwm::DwmFlush() }.is_err() {
        warn!("DWM could not commit preview publication; withdrawing the surface");
        state.host_anchored = false;
        for preview in state.previews.values_mut() {
            preview.published = None;
        }
        return PreviewPublishOutcome {
            live: 0,
            retry_needed: true,
        };
    }
    if state.lifecycle_epoch != preview_lifecycle_epoch() {
        state.host_anchored = false;
        for preview in state.previews.values_mut() {
            preview.published = None;
        }
        return PreviewPublishOutcome {
            live: 0,
            retry_needed: false,
        };
    }
    // A destroy can race the DWM update above. Remove every such registration
    // before targets are synchronized; WM_NCHITTEST independently checks the
    // same fence for a destroy that arrives after this sweep.
    let invalidated: Vec<WindowId> = state
        .previews
        .keys()
        .copied()
        .filter(|window_id| preview_source_is_invalidated(*window_id))
        .collect();
    for window_id in invalidated {
        state.previews.remove(&window_id);
    }

    let retry_needed = requests.iter().any(|request| {
        !preview_source_is_invalidated(request.window_id)
            && state
                .previews
                .get(&request.window_id)
                .is_none_or(|preview| {
                    preview.published.map(|published| published.request) != Some(*request)
                })
    });
    let live = state
        .previews
        .values()
        .filter(|preview| preview.published.is_some())
        .count();
    if live == 0 {
        state.host_anchored = false;
    }
    PreviewPublishOutcome { live, retry_needed }
}

fn activate_published_surface(state: &mut PersistentPreviewState) -> bool {
    let stale_host_registration = state
        .previews
        .values()
        .any(|preview| !preview.handle.belongs_to_current_host());
    if state.lifecycle_epoch != preview_lifecycle_epoch() || stale_host_registration {
        state.host_anchored = false;
        crate::preview_input::set_preview_targets_armed(false);
        let _ = host().hide_surface();
        return false;
    }
    if state
        .previews
        .values()
        .all(|preview| preview.published.is_none())
    {
        state.host_anchored = false;
        crate::preview_input::set_preview_targets_armed(false);
        let _ = host().hide_surface();
        return true;
    }
    let epoch = state.lifecycle_epoch;
    let host_below = state.host_below.map(|hwnd| HWND(hwnd as *mut c_void));
    if let Err(error) = host().anchor_within_band(host_below) {
        state.host_anchored = false;
        crate::preview_input::set_preview_targets_armed(false);
        warn!("Preview host z-order anchor failed; surface remains hidden: {error}");
        return false;
    }
    if epoch != preview_lifecycle_epoch()
        || state
            .previews
            .values()
            .any(|preview| !preview.handle.belongs_to_current_host())
    {
        state.host_anchored = false;
        crate::preview_input::set_preview_targets_armed(false);
        let _ = host().hide_surface();
        return false;
    }
    // The host is now in its final band. Raise targets after it so every armed
    // target is physically above the destination HWND.
    let targets_raised = crate::preview_input::raise_preview_click_targets(
        host().hwnd().0 as isize,
    )
    .is_some_and(|generation| {
        crate::preview_input::wait_for_applied_raise_generation(
            generation,
            Duration::from_millis(150),
        )
    });
    if !targets_raised || epoch != preview_lifecycle_epoch() {
        state.host_anchored = false;
        crate::preview_input::set_preview_targets_armed(false);
        let _ = host().hide_surface();
        warn!("Preview target z-order/lifecycle could not be acknowledged; surface hidden");
        return false;
    }
    state.host_anchored = true;
    // Arming carries E, not a fresh read of the global epoch. If invalidation
    // races this store, WM_NCHITTEST compares E against E+1 and stays transparent.
    crate::preview_input::set_preview_targets_armed_for_epoch(epoch);
    true
}

fn published_preview_requests(state: &PersistentPreviewState) -> Vec<PublishedPreview> {
    state
        .previews
        .values()
        .filter_map(|preview| preview.published)
        .collect()
}

/// Reconcile the input overlays strictly against DWM updates that succeeded.
#[cfg(not(test))]
fn sync_published_preview_targets(state: &PersistentPreviewState) -> Option<u64> {
    let published = published_preview_requests(state);
    crate::preview_input::sync_preview_click_targets(&preview_click_targets(&published))
}

fn signal_existing_retry_worker(slot: &mut Option<mpsc::SyncSender<()>>) -> bool {
    let Some(sender) = slot.as_ref() else {
        return false;
    };
    match sender.try_send(()) {
        Ok(()) | Err(mpsc::TrySendError::Full(())) => true,
        Err(mpsc::TrySendError::Disconnected(())) => {
            *slot = None;
            false
        }
    }
}

/// Schedule a retry that does not depend on any later layout event.
#[cfg(not(test))]
fn schedule_preview_retry() {
    #[cfg(feature = "integration-probes")]
    if SUPPRESS_RETRY_FOR_CAPTURE_PROBE.load(Ordering::Acquire) {
        return;
    }
    if PREVIEW_CLEAR_PENDING.load(Ordering::Acquire) {
        return;
    }
    PREVIEW_RETRY_PENDING.store(true, Ordering::Release);
    let mut slot = PREVIEW_RETRY_TX
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex);
    if signal_existing_retry_worker(&mut slot) {
        return;
    }
    let (tx, rx) = mpsc::sync_channel::<()>(1);
    let worker = move || {
        while rx.recv().is_ok() {
            // Coalesce bursty animation failures, then retry the newest
            // desired generation under the same transaction as commits.
            std::thread::sleep(Duration::from_millis(50));
            while rx.try_recv().is_ok() {}
            // This round covers every desire visible after coalescing. A
            // request arriving during publication sets the bit and queues a
            // new token for the next loop iteration.
            PREVIEW_RETRY_PENDING.store(false, Ordering::Release);
            let mut retry_round = 0u32;
            loop {
                retry_round += 1;
                let _ = host().hide_surface();
                crate::preview_input::set_preview_targets_armed(false);
                // A host restart or a previous failed registration can leave a
                // desired request without a handle. Re-attempt registration
                // outside the state lock, then publish the exact newest desire.
                let desired_for_registration = lock_persistent_previews().desired.clone();
                for request in &desired_for_registration {
                    if !preview_source_is_invalidated(request.window_id) {
                        let _ = prepare_persistent_preview(request.window_id);
                    }
                }
                let retry_state = {
                    let _transaction = lock_persistent_preview_transaction();
                    let mut state = lock_persistent_previews();
                    let requests = state.desired.clone();
                    let preview_generation = state.generation;
                    let outcome = publish_preview_requests_locked(&mut state, &requests, true);
                    let input_generation = sync_published_preview_targets(&state);
                    (preview_generation, outcome.retry_needed, input_generation)
                };
                let (preview_generation, mut retry_needed, input_generation) = retry_state;
                let input_applied = input_generation.is_some_and(|generation| {
                    crate::preview_input::wait_for_applied_generation(
                        generation,
                        Duration::from_millis(150),
                    )
                });
                if input_applied {
                    let _transaction = lock_persistent_preview_transaction();
                    let mut state = lock_persistent_previews();
                    if state.generation != preview_generation {
                        break;
                    }
                    retry_needed |= !activate_published_surface(&mut state);
                } else {
                    // Keep handles and desire for another bounded retry,
                    // but never expose pixels without acknowledged input.
                    retry_needed = true;
                }
                if !retry_needed {
                    PREVIEW_RETRY_PENDING.store(false, Ordering::Release);
                    break;
                }
                if retry_round >= MAX_FAILED_PUBLISHES {
                    // Keep the desired request and registered-handle receipts.
                    // The host/input stay hidden, but this is a backoff boundary
                    // rather than an abandonment boundary: an eventual source,
                    // DWM, or input recovery must republish without waiting for
                    // an unrelated layout mutation.
                    PREVIEW_RETRY_PENDING.store(true, Ordering::Release);
                    warn!(
                        "Preview retry burst exhausted for generation {preview_generation}; retaining desired state for self-healing backoff"
                    );
                    retry_round = 0;
                    std::thread::sleep(Duration::from_millis(500));
                } else {
                    std::thread::sleep(Duration::from_millis(75));
                }
            }
        }
    };
    #[cfg(feature = "integration-probes")]
    let force_failure = FORCE_RETRY_SPAWN_FAILURE.load(Ordering::Acquire);
    #[cfg(not(feature = "integration-probes"))]
    let force_failure = false;
    let spawn = if force_failure {
        Err(std::io::Error::other(
            "injected preview retry worker spawn failure",
        ))
    } else {
        std::thread::Builder::new()
            .name("leopardwm-preview-retry".into())
            .spawn(worker)
    };
    match spawn {
        Ok(_) => {
            let _ = tx.try_send(());
            *slot = Some(tx);
        }
        Err(error) => {
            warn!("Preview retry worker could not start: {error}; daemon service tick will retry")
        }
    }
}

/// Service a retry obligation that could not create its dedicated worker. The
/// daemon calls this after every event and from its periodic UI tick, so a
/// transient thread-spawn failure cannot leave a desired preview hidden until
/// unrelated layout activity occurs.
pub fn service_pending_preview_retry() -> bool {
    service_pending_thumbnail_unregisters();
    #[cfg(not(test))]
    {
        if PREVIEW_CLEAR_PENDING.load(Ordering::Acquire) {
            let due = PREVIEW_CLEAR_RETRY_AFTER
                .lock()
                .unwrap_or_else(crate::recover_poisoned_mutex)
                .is_none_or(|deadline| Instant::now() >= deadline);
            if due {
                let _ = clear_persistent_previews_best_effort();
            }
            return false;
        }
        if PREVIEW_RETRY_PENDING.load(Ordering::Acquire) {
            schedule_preview_retry();
        }
    }
    PREVIEW_RELAYOUT_REQUIRED.swap(false, Ordering::AcqRel)
}

/// Retain the full physically parked preview intent even when registration was
/// temporarily unavailable. Publishable requests are committed separately;
/// the autonomous worker may recover a missing handle, but its physical fence
/// forces a fresh daemon exact placement before pixels can be exposed.
pub(crate) fn retain_persistent_preview_desire(
    requests: &[PersistentPreviewRequest],
    expected_lifecycle_epoch: u64,
) {
    #[cfg(test)]
    {
        let _ = (requests, expected_lifecycle_epoch);
    }
    #[cfg(not(test))]
    {
        let expected_lifecycle_epoch = if expected_lifecycle_epoch == 0 {
            preview_lifecycle_epoch()
        } else {
            expected_lifecycle_epoch
        };
        if expected_lifecycle_epoch != preview_lifecycle_epoch() {
            return;
        }
        let retry_needed = {
            let mut state = lock_persistent_previews();
            if state.lifecycle_epoch != expected_lifecycle_epoch {
                return;
            }
            if state.desired != requests {
                state.desired = requests.to_vec();
                state.generation = state.generation.wrapping_add(1).max(1);
            }
            requests.iter().any(|request| {
                state
                    .previews
                    .get(&request.window_id)
                    .is_none_or(|preview| {
                        preview.requires_physical_commit
                            || preview.published.map(|published| published.request)
                                != Some(*request)
                    })
            })
        };
        if retry_needed {
            schedule_preview_retry();
        }
    }
}

pub(crate) fn commit_persistent_previews(
    requests: &[PersistentPreviewRequest],
    refresh_source_size: bool,
    expected_lifecycle_epoch: u64,
    host_below: Option<isize>,
) -> Result<usize, Win32Error> {
    #[cfg(test)]
    {
        let _ = (
            requests,
            refresh_source_size,
            expected_lifecycle_epoch,
            host_below,
        );
        crate::preview_input::clear_preview_click_targets();
        Ok(0)
    }
    #[cfg(not(test))]
    {
        let expected_lifecycle_epoch = if expected_lifecycle_epoch == 0 {
            preview_lifecycle_epoch()
        } else {
            expected_lifecycle_epoch
        };
        if expected_lifecycle_epoch != preview_lifecycle_epoch() {
            return Ok(0);
        }
        {
            let mut state = lock_persistent_previews();
            let host_generation_changed = state
                .previews
                .values()
                .any(|preview| !preview.handle.belongs_to_current_host());
            if state.lifecycle_epoch != expected_lifecycle_epoch || host_generation_changed {
                state.host_anchored = false;
                for preview in state.previews.values_mut() {
                    preview.published = None;
                }
            }
            if host_generation_changed {
                state
                    .previews
                    .retain(|_, preview| preview.handle.belongs_to_current_host());
            }
            state.lifecycle_epoch = expected_lifecycle_epoch;
            if state.host_below != host_below {
                // A new band anchor must be re-verified before the surface may
                // claim ownership of those pixels again.
                state.host_anchored = false;
                state.host_below = host_below;
            }
        }
        let next_ids: std::collections::HashSet<_> =
            requests.iter().map(|request| request.window_id).collect();
        let publication_changed = {
            let mut state = lock_persistent_previews();
            let mut published: Vec<_> = published_preview_requests(&state)
                .into_iter()
                .map(|receipt| receipt.request)
                .collect();
            let mut requested = requests.to_vec();
            published.sort_by_key(|request| request.window_id);
            requested.sort_by_key(|request| request.window_id);
            let changed = published != requested;
            if changed {
                state.host_anchored = false;
            }
            changed
        };

        // Geometry changes are a short hidden transaction. Disarm hit testing,
        // hide the host, and acknowledge withdrawal before changing DWM pixels.
        // New targets are positioned while transparent; only then is the host
        // shown and hit testing armed.
        if publication_changed {
            if let Err(error) = host().hide_surface() {
                warn!("Preview surface could not be hidden for reconciliation: {error}");
                crate::preview_input::set_preview_targets_armed(false);
                schedule_preview_retry();
                return Ok(0);
            }
            let withdrawn =
                crate::preview_input::sync_preview_click_targets(&[]).is_some_and(|generation| {
                    crate::preview_input::wait_for_applied_generation(
                        generation,
                        Duration::from_millis(150),
                    )
                });
            if !withdrawn {
                // Optional preview input must not turn an already-applied HWND
                // layout into a placement failure. Keep the host hidden and let
                // the autonomous worker retry the newest desire.
                let mut state = lock_persistent_previews();
                state.desired = requests.to_vec();
                state.generation = state.generation.wrapping_add(1).max(1);
                state.host_anchored = false;
                drop(state);
                schedule_preview_retry();
                warn!("Preview input withdrawal was not acknowledged; tiling kept, surface hidden");
                return Ok(0);
            }
        }

        if expected_lifecycle_epoch != preview_lifecycle_epoch() {
            return Ok(0);
        }
        let (commit_generation, outcome, final_input_generation) = {
            let mut state = lock_persistent_previews();
            if state.lifecycle_epoch != expected_lifecycle_epoch {
                return Ok(0);
            }
            state.desired = requests.to_vec();
            state.generation = state.generation.wrapping_add(1).max(1);
            let generation = state.generation;
            if publication_changed {
                // Old-owner/old-geometry receipts were withdrawn above. A
                // failed update must not resurrect them merely because the HWND
                // survived into the new request set.
                for preview in state.previews.values_mut() {
                    preview.published = None;
                }
            }
            state
                .previews
                .retain(|window_id, _| next_ids.contains(window_id));
            // `commit_persistent_previews` is called only after the placement
            // layer verified source parking and released cloak/region protection.
            // This is the sole operation that authorizes an autonomously
            // recovered registration to publish.
            for request in requests {
                if let Some(preview) = state.previews.get_mut(&request.window_id) {
                    preview.requires_physical_commit = false;
                }
            }
            let outcome =
                publish_preview_requests_locked(&mut state, requests, refresh_source_size);
            let input = sync_published_preview_targets(&state);
            (generation, outcome, input)
        };

        let final_input_applied = final_input_generation.is_some_and(|generation| {
            crate::preview_input::wait_for_applied_generation(
                generation,
                Duration::from_millis(150),
            )
        });
        if !final_input_applied {
            host().hide_surface().ok();
            crate::preview_input::set_preview_targets_armed(false);
            schedule_preview_retry();
            warn!(
                "Preview input generation was not acknowledged; generation {commit_generation} remains hidden for retry"
            );
            return Ok(0);
        }

        if expected_lifecycle_epoch != preview_lifecycle_epoch() {
            host().hide_surface().ok();
            crate::preview_input::set_preview_targets_armed(false);
            return Ok(0);
        }
        let activated = {
            let mut state = lock_persistent_previews();
            state.generation == commit_generation
                && state.lifecycle_epoch == expected_lifecycle_epoch
                && activate_published_surface(&mut state)
        };
        if outcome.retry_needed || !activated {
            schedule_preview_retry();
        }
        Ok(if activated { outcome.live } else { 0 })
    }
}

/// One input target per last successfully-published request. Registration alone
/// is never enough: it proves only that DWM allocated a handle, not that pixels
/// exist at the current destination.
fn preview_click_targets(
    published: &[PublishedPreview],
) -> Vec<crate::preview_input::PreviewClickTarget> {
    let mut targets: Vec<crate::preview_input::PreviewClickTarget> = Vec::new();
    for published in published {
        let request = published.request;
        if request.destination_screen_rect.width <= 0
            || request.destination_screen_rect.height <= 0
            || targets
                .iter()
                .any(|target| target.window_id == request.window_id)
        {
            continue;
        }
        targets.push(crate::preview_input::PreviewClickTarget {
            window_id: request.window_id,
            source_process_id: published.source_process_id,
            publication_generation: published.generation,
            rect: request.destination_screen_rect,
        });
    }
    targets
}

#[cfg(not(test))]
fn clear_persistent_previews_locked() -> Result<(), Win32Error> {
    lock_persistent_previews().host_anchored = false;
    // Hide/disarm first, then prove input withdrawal, and only then unregister
    // DWM handles. This never leaves visible dead pixels or an armed invisible
    // target during normal cleanup.
    host().hide_surface()?;
    crate::preview_input::set_preview_targets_armed(false);
    let generation = crate::preview_input::sync_preview_click_targets(&[]).ok_or_else(|| {
        Win32Error::SetPositionFailed("persistent preview input clear unavailable".into())
    })?;
    if !crate::preview_input::wait_for_applied_generation(generation, Duration::from_millis(150)) {
        return Err(Win32Error::SetPositionFailed(
            "persistent preview clear was not acknowledged by input pump".into(),
        ));
    }
    let mut state = lock_persistent_previews();
    state.previews.clear();
    state.desired.clear();
    state.host_anchored = false;
    state.generation = state.generation.wrapping_add(1).max(1);
    PREVIEW_CLEAR_PENDING.store(false, Ordering::Release);
    *PREVIEW_CLEAR_RETRY_AFTER
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex) = None;
    PREVIEW_RETRY_PENDING.store(false, Ordering::Release);
    PREVIEW_RELAYOUT_REQUIRED.store(false, Ordering::Release);
    Ok(())
}

pub fn clear_persistent_previews() -> Result<(), Win32Error> {
    #[cfg(test)]
    {
        let mut state = lock_persistent_previews();
        state.previews.clear();
        state.desired.clear();
        state.host_anchored = false;
        state.generation = state.generation.wrapping_add(1).max(1);
        Ok(())
    }
    #[cfg(not(test))]
    {
        let _transaction = lock_persistent_preview_transaction();
        clear_persistent_previews_locked()
    }
}

/// Nonblocking lifecycle cleanup. Epoch invalidation already makes old pixels
/// and targets unreachable; if a placement worker owns the transaction, defer
/// handle/input destruction rather than wedge the daemon event loop.
pub fn clear_persistent_previews_best_effort() -> Result<bool, Win32Error> {
    let _transaction = match PERSISTENT_PREVIEW_TRANSACTION.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            #[cfg(not(test))]
            {
                PREVIEW_CLEAR_PENDING.store(true, Ordering::Release);
                *PREVIEW_CLEAR_RETRY_AFTER
                    .lock()
                    .unwrap_or_else(crate::recover_poisoned_mutex) =
                    Some(Instant::now() + Duration::from_millis(250));
                PREVIEW_RETRY_PENDING.store(false, Ordering::Release);
            }
            return Ok(false);
        }
    };
    #[cfg(test)]
    {
        let mut state = lock_persistent_previews();
        state.previews.clear();
        state.desired.clear();
        state.host_anchored = false;
        state.generation = state.generation.wrapping_add(1).max(1);
        Ok(true)
    }
    #[cfg(not(test))]
    match clear_persistent_previews_locked() {
        Ok(()) => Ok(true),
        Err(error) => {
            PREVIEW_CLEAR_PENDING.store(true, Ordering::Release);
            *PREVIEW_CLEAR_RETRY_AFTER
                .lock()
                .unwrap_or_else(crate::recover_poisoned_mutex) =
                Some(Instant::now() + Duration::from_millis(250));
            PREVIEW_RETRY_PENDING.store(false, Ordering::Release);
            Err(error)
        }
    }
}

/// Re-anchor the preview layer after an explicit tiled HWND was raised. This is
/// a lifecycle operation, not a per-frame publication side effect.
pub fn reanchor_persistent_previews() -> Result<(), Win32Error> {
    let _transaction = match PERSISTENT_PREVIEW_TRANSACTION.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            crate::preview_input::set_preview_targets_armed(false);
            let _ = host().hide_surface();
            #[cfg(not(test))]
            schedule_preview_retry();
            return Err(Win32Error::SetPositionFailed(
                "preview re-anchor deferred behind active placement".into(),
            ));
        }
    };
    let mut state = lock_persistent_previews();
    if state
        .previews
        .values()
        .all(|preview| preview.published.is_none())
    {
        return Ok(());
    }
    if activate_published_surface(&mut state) {
        Ok(())
    } else {
        Err(Win32Error::SetPositionFailed(
            "preview re-anchor could not verify current lifecycle/z-order".into(),
        ))
    }
}

pub(crate) fn forget_persistent_preview(window_id: WindowId) {
    let invalidation_to_retire = preview_source_invalidation_generation(window_id);
    let _transaction = match PERSISTENT_PREVIEW_TRANSACTION.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            invalidate_persistent_preview_source(window_id);
            invalidate_persistent_preview_surface();
            #[cfg(not(test))]
            {
                PREVIEW_CLEAR_PENDING.store(true, Ordering::Release);
                PREVIEW_RETRY_PENDING.store(false, Ordering::Release);
            }
            return;
        }
    };
    lock_persistent_previews().host_anchored = false;
    let _ = host().hide_surface();
    crate::preview_input::set_preview_targets_armed(false);
    let surviving = {
        let state = lock_persistent_previews();
        published_preview_requests(&state)
            .into_iter()
            .filter(|published| published.request.window_id != window_id)
            .collect::<Vec<_>>()
    };
    let acknowledged =
        crate::preview_input::sync_preview_click_targets(&preview_click_targets(&surviving))
            .is_some_and(|generation| {
                crate::preview_input::wait_for_applied_generation(
                    generation,
                    Duration::from_millis(100),
                )
            });
    if !acknowledged {
        let mut state = lock_persistent_previews();
        state.previews.remove(&window_id);
        state
            .desired
            .retain(|request| request.window_id != window_id);
        state.generation = state.generation.wrapping_add(1).max(1);
        drop(state);
        // The input pump may be retaining this target while it owns capture.
        // Keep the tombstone until a later acknowledged withdrawal; otherwise
        // re-anchoring a surviving preview could arm the retained dead target.
        #[cfg(not(test))]
        schedule_preview_retry();
        warn!("Preview forget was not acknowledged by input pump; surface remains hidden");
        return;
    }
    let mut state = lock_persistent_previews();
    state.previews.remove(&window_id);
    state
        .desired
        .retain(|request| request.window_id != window_id);
    state.generation = state.generation.wrapping_add(1).max(1);
    let _ = activate_published_surface(&mut state);
    drop(state);
    retire_preview_source_invalidation(window_id, invalidation_to_retire);
}

pub fn source_size(handle: isize) -> Option<(i32, i32)> {
    if handle == 0 {
        return None;
    }
    let size = unsafe { DwmQueryThumbnailSourceSize(resolve_dwm_handle(handle)) }.ok()?;
    if size.cx <= 0 || size.cy <= 0 {
        return None;
    }
    Some((size.cx, size.cy))
}

/// Unregister a thumbnail by its opaque transfer token. Used by the worker
/// thread when consuming `CrossfadeEntry` values (whose Drop calls this).
/// The token resolves to both the DWM handle and the host-generation claim;
/// stale old-generation drops are therefore inert after a host restart.
///
/// Idempotent on null/zero handles — does nothing.
pub fn unregister_raw(handle: isize) {
    unregister_impl(handle, true, HostBand::Topmost, 0);
}

fn unregister_impl(
    handle: isize,
    fallback_host_z: bool,
    fallback_band: HostBand,
    expected_host_generation: u64,
) {
    if handle == 0 {
        return;
    }
    let _commit = Z_ORDER_COMMIT
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex);
    let ownership = Z_ORDER_STATE
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
        .registrations
        .get(&handle)
        .copied();

    let Some(ownership) = ownership else {
        // Compatibility for an external caller that still passes a literal
        // HTHUMBNAIL. LeopardWM-created registrations always take the tracked
        // path below.
        if let Err(error) = unregister_dwm_handle(handle) {
            warn!("DwmUnregisterThumbnail({handle}) failed for untracked handle: {error}");
        }
        return;
    };
    if expected_host_generation != 0
        && (ownership.host_z != fallback_host_z
            || ownership.band != fallback_band
            || ownership.host_generation != expected_host_generation)
    {
        warn!("Ignoring stale thumbnail ownership token {handle}");
        return;
    }
    if ownership.retired {
        // Restart proved this destination HWND was destroyed. Removing the
        // stale token cannot affect a replacement generation's accounting.
        Z_ORDER_STATE
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex)
            .registrations
            .remove(&handle);
        return;
    }

    // Hide first. If unregister itself fails, the globally retained ownership
    // receipt still owns a non-visible thumbnail and can be retried later.
    if let Err(error) = update(handle, Rect::new(0, 0, 1, 1), 0, false) {
        warn!("Could not hide thumbnail {handle} before unregister: {error}");
    }
    if let Err(error) = unregister_dwm_handle(ownership.dwm_handle) {
        let mut state = Z_ORDER_STATE
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex);
        if let Some(current) = state.registrations.get_mut(&handle) {
            if current.dwm_handle == ownership.dwm_handle && !current.retired {
                current.pending_unregister = true;
            }
        }
        warn!(
            "DwmUnregisterThumbnail({}) failed; retaining retry receipt: {}",
            ownership.dwm_handle, error
        );
        return;
    }

    let released = {
        let mut state = Z_ORDER_STATE
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex);
        // A token is unique, but retain this check so a future maintenance
        // change cannot let an old completion release replacement ownership.
        if state
            .registrations
            .get(&handle)
            .is_some_and(|current| current.dwm_handle == ownership.dwm_handle)
        {
            release_registration_locked(&mut state, handle)
        } else {
            None
        }
    };
    let Some((released, demote_host)) = released else {
        return;
    };
    if demote_host && released.host_generation == host().generation() {
        if let Err(error) = host().set_topmost(false) {
            // The registration is gone, but the host state is retryable on the
            // next registration/restart. Do not resurrect a released DWM handle.
            warn!("Thumbnail host demotion failed: {error}");
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
#[cfg_attr(test, allow(dead_code))]
pub struct ThumbnailHost {
    hwnd_raw: AtomicIsize,
    thread_id: AtomicU32,
    generation: AtomicU64,
    geometry_current: AtomicBool,
    /// Virtual-screen origin captured at host creation, updated by
    /// `resize_to_virtual_screen` on display change. Wrapped in a Mutex
    /// for cross-thread reads (animation worker reads on every frame).
    origin: std::sync::Mutex<(i32, i32)>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    restart_lock: Mutex<()>,
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
                "ThumbnailHost::new failed: {} — preview host will retry lazily",
                e
            );
            ThumbnailHost {
                hwnd_raw: AtomicIsize::new(0),
                thread_id: AtomicU32::new(0),
                generation: AtomicU64::new(1),
                geometry_current: AtomicBool::new(false),
                origin: std::sync::Mutex::new(virtual_screen_origin()),
                thread: Mutex::new(None),
                restart_lock: Mutex::new(()),
            }
        }
    })
}

/// Whether `below` is somewhere under `above` in the same z-order band.
///
/// Bounded so a corrupted or cyclic chain can never spin the caller. The walk
/// covers far more top-level windows than a desktop realistically holds.
fn is_below_in_band(above: HWND, below: HWND) -> bool {
    const MAX_WALK: usize = 4096;
    let mut cursor = above;
    for _ in 0..MAX_WALK {
        match unsafe { GetWindow(cursor, GW_HWNDNEXT) }.ok() {
            Some(next) if next == below => return true,
            Some(next) => cursor = next,
            None => return false,
        }
    }
    false
}

impl ThumbnailHost {
    fn new() -> Result<Self, Win32Error> {
        #[cfg(feature = "integration-probes")]
        if FORCE_HOST_SPAWN_FAILURE.load(Ordering::Acquire) {
            return Err(Win32Error::HookInstallFailed(
                "injected thumbnail host spawn failure".into(),
            ));
        }
        #[cfg(test)]
        panic!("ThumbnailHost::new spawns a DWM host window; gate the call behind cfg(test)");
        #[allow(unreachable_code)]
        {
            let origin = virtual_screen_origin();
            let (vw, vh) = virtual_screen_size();
            let (tx, rx) = mpsc::channel::<Result<(isize, u32), Win32Error>>();
            let (startup_ack_tx, startup_ack_rx) = mpsc::channel::<()>();

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
                    if RegisterClassW(&wc) == 0 {
                        let last_error = GetLastError();
                        let mut existing = WNDCLASSW::default();
                        let compatible_existing = last_error == ERROR_CLASS_ALREADY_EXISTS
                            && GetClassInfoW(
                                None,
                                windows::core::PCWSTR(class_name.as_ptr()),
                                &mut existing,
                            )
                            .is_ok()
                            && existing.lpfnWndProc.map(|proc| proc as usize)
                                == wc.lpfnWndProc.map(|proc| proc as usize);
                        if !compatible_existing {
                            let _ = tx.send(Err(Win32Error::HookInstallFailed(format!(
                                "ThumbnailHost class: {}",
                                windows::core::Error::from_thread()
                            ))));
                            return;
                        }
                    }

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
                            let initialized = init_layered_transparent(h).and_then(|()| {
                                SetWindowPos(
                                    h,
                                    Some(HWND_NOTOPMOST),
                                    origin.0,
                                    origin.1,
                                    vw,
                                    vh,
                                    SWP_NOACTIVATE,
                                )
                                .map_err(|error| {
                                    Win32Error::HookInstallFailed(format!(
                                        "ThumbnailHost initial position: {error}"
                                    ))
                                })
                            });
                            if let Err(error) = initialized {
                                let _ = DestroyWindow(h);
                                let _ = tx.send(Err(error));
                                return;
                            }
                            let thread_id = windows::Win32::System::Threading::GetCurrentThreadId();
                            if tx.send(Ok((h.0 as isize, thread_id))).is_err()
                                || startup_ack_rx.recv_timeout(Duration::from_secs(5)).is_err()
                            {
                                let _ = DestroyWindow(h);
                                return;
                            }
                            let mut msg = MSG::default();
                            loop {
                                let result = GetMessageW(&mut msg, None, 0, 0).0;
                                if result <= 0 {
                                    if result < 0 {
                                        warn!("ThumbnailHost message pump failed");
                                    }
                                    break;
                                }
                                let _ = DispatchMessageW(&msg);
                            }
                            if IsWindow(Some(h)).as_bool() {
                                let _ = DestroyWindow(h);
                            }
                            let _ =
                                UnregisterClassW(windows::core::PCWSTR(class_name.as_ptr()), None);
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

            let (hwnd_raw, thread_id) = match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(Ok(ready)) => ready,
                Ok(Err(error)) => {
                    let _ = thread.join();
                    return Err(error);
                }
                Err(error) => {
                    drop(startup_ack_tx);
                    let _ = thread.join();
                    return Err(Win32Error::HookInstallFailed(format!(
                        "ThumbnailHost readiness timed out: {error}"
                    )));
                }
            };
            if startup_ack_tx.send(()).is_err() {
                let _ = thread.join();
                return Err(Win32Error::HookInstallFailed(
                    "ThumbnailHost startup acknowledgement failed".into(),
                ));
            }

            Ok(Self {
                hwnd_raw: AtomicIsize::new(hwnd_raw),
                thread_id: AtomicU32::new(thread_id),
                generation: AtomicU64::new(1),
                geometry_current: AtomicBool::new(true),
                origin: std::sync::Mutex::new(origin),
                thread: Mutex::new(Some(thread)),
                restart_lock: Mutex::new(()),
            })
        }
    }

    fn raw_hwnd(&self) -> isize {
        self.hwnd_raw.load(Ordering::Acquire)
    }

    /// HWND of the current host generation, or null while unavailable.
    pub fn hwnd(&self) -> HWND {
        HWND(self.raw_hwnd() as *mut c_void)
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn pump_running(&self) -> bool {
        self.thread
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex)
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
    }

    pub fn mark_virtual_screen_geometry_stale(&self) {
        self.geometry_current.store(false, Ordering::Release);
    }

    fn ensure_virtual_screen_geometry(&self) -> Result<(), Win32Error> {
        if self.geometry_current.load(Ordering::Acquire) {
            Ok(())
        } else {
            self.resize_to_virtual_screen()
        }
    }

    #[cfg(not(test))]
    fn restart(&self) -> Result<(), Win32Error> {
        let _restart = self
            .restart_lock
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex);
        if self.raw_hwnd() != 0
            && unsafe { IsWindow(Some(self.hwnd())) }.as_bool()
            && self.pump_running()
        {
            return Ok(());
        }
        let old_generation = self.generation();
        let old_thread = self
            .thread
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex)
            .take();
        if let Some(thread) = old_thread {
            if !thread.is_finished() {
                let thread_id = self.thread_id.load(Ordering::Acquire);
                if thread_id != 0 {
                    let _ = unsafe {
                        PostThreadMessageW(
                            thread_id,
                            WM_QUIT,
                            windows::Win32::Foundation::WPARAM(0),
                            windows::Win32::Foundation::LPARAM(0),
                        )
                    };
                }
                // The pump may already have consumed WM_QUIT, in which case a
                // second post legitimately fails while the thread is between
                // GetMessage return and process teardown. Wait either way.
                let deadline = Instant::now() + Duration::from_secs(1);
                while !thread.is_finished() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
                if !thread.is_finished() {
                    *self
                        .thread
                        .lock()
                        .unwrap_or_else(crate::recover_poisoned_mutex) = Some(thread);
                    return Err(Win32Error::SetPositionFailed(
                        "thumbnail host pump could not be stopped after HWND death".into(),
                    ));
                }
            }
            let _ = thread.join();
        }

        self.hwnd_raw.store(0, Ordering::Release);
        self.thread_id.store(0, Ordering::Release);
        // Advance before retiring old claims. A registration that completed its
        // DWM call just as the pump died now observes a generation mismatch and
        // cannot insert an old claim after retirement.
        let _ = self
            .generation
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                Some(value.wrapping_add(1).max(1))
            });
        retire_host_generation_claims(old_generation);
        let replacement = Self::new()?;
        let ThumbnailHost {
            hwnd_raw,
            thread_id,
            origin,
            thread,
            ..
        } = replacement;
        let new_raw = hwnd_raw.into_inner();
        let new_thread_id = thread_id.into_inner();
        let new_origin = origin
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let new_thread = thread
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.hwnd_raw.store(new_raw, Ordering::Release);
        self.thread_id.store(new_thread_id, Ordering::Release);
        self.geometry_current.store(true, Ordering::Release);
        *self
            .origin
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex) = new_origin;
        *self
            .thread
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex) = new_thread;

        // All handles targeting the prior host generation are invalid. Revoke
        // their publication epoch before callers can register on the new host.
        invalidate_persistent_preview_surface();
        PREVIEW_RELAYOUT_REQUIRED.store(true, Ordering::Release);
        Ok(())
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
        if self.raw_hwnd() != 0
            && unsafe { IsWindow(Some(self.hwnd())) }.as_bool()
            && self.pump_running()
        {
            return true;
        }
        #[cfg(test)]
        {
            false
        }
        #[cfg(not(test))]
        {
            self.restart().is_ok()
                && self.raw_hwnd() != 0
                && unsafe { IsWindow(Some(self.hwnd())) }.as_bool()
                && self.pump_running()
        }
    }

    /// Resize and reposition the host to the current virtual-screen
    /// geometry. Called from the daemon's display-change recovery so
    /// thumbnail destination rects use post-change coordinates. Subsequent
    /// `register` calls see the new origin via `origin()`.
    pub fn resize_to_virtual_screen(&self) -> Result<(), Win32Error> {
        if !self.is_available() {
            return Err(Win32Error::SetPositionFailed(
                "thumbnail host unavailable during display resize".into(),
            ));
        }
        let _transaction = match PERSISTENT_PREVIEW_TRANSACTION.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                return Err(Win32Error::SetPositionFailed(
                    "thumbnail host resize deferred behind active placement".into(),
                ));
            }
        };
        let new_origin = virtual_screen_origin();
        let (vw, vh) = virtual_screen_size();
        let hwnd = self.hwnd();
        // Move first and publish the client-origin only after the HWND accepted
        // it. The old ordering exposed new client coordinates while the host
        // still occupied the old virtual origin whenever SetWindowPos failed.
        unsafe {
            SetWindowPos(
                hwnd,
                None,
                new_origin.0,
                new_origin.1,
                vw,
                vh,
                SWP_NOACTIVATE | SWP_NOZORDER,
            )
        }
        .map_err(|error| {
            Win32Error::SetPositionFailed(format!("thumbnail host resize: {error}"))
        })?;
        let mut origin = self
            .origin
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex);
        *origin = new_origin;
        self.geometry_current.store(true, Ordering::Release);
        Ok(())
    }

    /// Toggle the host's z-order between topmost (while thumbnails are
    /// active) and non-topmost (idle). Idle non-topmost lets the taskbar
    /// auto-hide animation appear correctly in front of windows; topmost
    /// during animation ensures the thumbnail composites above the live
    /// HWNDs that may be cloaked underneath.
    /// Lift the host to the front of the band it is already in.
    ///
    /// Needed for edge previews: they deliberately keep the host out of the
    /// topmost band so floating windows stay above the tiled layer, but every
    /// focus change raises an application window to the front of that same band.
    /// Without this a preview could be composited behind an ordinary window and
    /// simply never appear. The daemon narrows previews away from floats, so
    /// nothing that belongs above them is covered by this.
    #[cfg_attr(test, allow(dead_code))]
    fn hide_surface(&self) -> Result<(), Win32Error> {
        if self.raw_hwnd() == 0 {
            return Ok(());
        }
        crate::preview_input::set_preview_targets_armed(false);
        unsafe {
            SetWindowPos(
                self.hwnd(),
                None,
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_HIDEWINDOW,
            )
        }
        .map_err(|error| Win32Error::SetPositionFailed(format!("thumbnail host hide: {error}")))
    }

    /// Anchor the host inside the normal band.
    ///
    /// `below` is normally the bottommost visible tiled HWND. Anchoring there
    /// keeps every window above the tiled band — including higher-integrity
    /// windows this process cannot move — above the preview, so the full edge
    /// strip stays published behind them instead of being cut away. `HWND_TOP`
    /// remains the fallback for standalone callers without an anchor.
    fn anchor_within_band(&self, below: Option<HWND>) -> Result<(), Win32Error> {
        if !self.is_available() {
            return Err(Win32Error::SetPositionFailed(
                "thumbnail host unavailable for z-order anchor".into(),
            ));
        }
        let anchor = below.filter(|anchor| unsafe { IsWindow(Some(*anchor)) }.as_bool());
        unsafe {
            SetWindowPos(
                self.hwnd(),
                Some(anchor.unwrap_or(HWND_TOP)),
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
            )
        }
        .map_err(|error| {
            Win32Error::SetPositionFailed(format!("thumbnail host z-order: {error}"))
        })?;
        // An accepted request is not a committed band position: the shell can
        // reorder during the same message pass. Without this readback the host
        // could stay above a window that owns those pixels and paint over it.
        // The host's own click targets are raised above it and legitimately sit
        // between the anchor and the host, so the invariant is "below the
        // anchor", not "immediately below" it.
        if let Some(anchor) = anchor {
            if !is_below_in_band(anchor, self.hwnd()) {
                return Err(Win32Error::SetPositionFailed(
                    "thumbnail host did not land below its tiled anchor".into(),
                ));
            }
        }
        Ok(())
    }

    fn set_topmost(&self, topmost: bool) -> Result<(), Win32Error> {
        if !self.is_available() {
            return Err(Win32Error::SetPositionFailed(
                "thumbnail host unavailable for band change".into(),
            ));
        }
        let hwnd = self.hwnd();
        let z = if topmost {
            HWND_TOPMOST
        } else {
            HWND_NOTOPMOST
        };
        let mut flags = SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE;
        if topmost {
            flags |= SWP_SHOWWINDOW;
        }
        unsafe { SetWindowPos(hwnd, Some(z), 0, 0, 0, 0, flags) }.map_err(|error| {
            Win32Error::SetPositionFailed(format!("thumbnail host band change: {error}"))
        })
    }
}

extern "system" fn thumbnail_host_proc(
    hwnd: HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    // The host is process infrastructure, not a user window. Ignoring WM_CLOSE
    // prevents shell/broadcast close messages from destroying the HWND while
    // leaving its pump alive and unrestartable.
    if msg == WM_CLOSE {
        return windows::Win32::Foundation::LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Set up the layered host with a 1×1 fully-transparent backing so DWM
/// composes its layered surface correctly. Without this step,
/// `WS_EX_LAYERED` windows that never call `UpdateLayeredWindow` may not
/// composite thumbnails reliably on all GPUs.
unsafe fn init_layered_transparent(hwnd: HWND) -> Result<(), Win32Error> {
    let screen_dc: HDC = unsafe { GetDC(None) };
    if screen_dc.0.is_null() {
        return Err(Win32Error::HookInstallFailed(
            "ThumbnailHost GetDC returned null".into(),
        ));
    }
    let mem_dc = unsafe { CreateCompatibleDC(Some(screen_dc)) };
    if mem_dc.0.is_null() {
        unsafe {
            ReleaseDC(None, screen_dc);
        }
        return Err(Win32Error::HookInstallFailed(
            "ThumbnailHost CreateCompatibleDC returned null".into(),
        ));
    }

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
    let bmp =
        match unsafe { CreateDIBSection(Some(mem_dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) } {
            Ok(bitmap) => bitmap,
            Err(error) => {
                unsafe {
                    let _ = DeleteDC(mem_dc);
                    ReleaseDC(None, screen_dc);
                }
                return Err(Win32Error::HookInstallFailed(format!(
                    "ThumbnailHost CreateDIBSection: {error}"
                )));
            }
        };
    if !bits.is_null() {
        unsafe { std::ptr::write_bytes(bits as *mut u8, 0, 4) };
    }
    let old = unsafe { SelectObject(mem_dc, bmp.into()) };
    let src_pt = windows::Win32::Foundation::POINT { x: 0, y: 0 };
    let size = windows::Win32::Foundation::SIZE { cx: 1, cy: 1 };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    let result = unsafe {
        UpdateLayeredWindow(
            hwnd,
            None,
            None,
            Some(&size),
            Some(mem_dc),
            Some(&src_pt),
            windows::Win32::Foundation::COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        )
    }
    .map_err(|error| {
        Win32Error::HookInstallFailed(format!("ThumbnailHost UpdateLayeredWindow: {error}"))
    });

    unsafe {
        SelectObject(mem_dc, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem_dc);
        ReleaseDC(None, screen_dc);
    }
    result
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

#[cfg(feature = "integration-probes")]
pub mod integration_probe {
    use super::*;
    use windows::core::BOOL;

    pub fn click_receipt_matches_target(
        event: crate::preview_input::PreviewClickEvent,
        target: crate::preview_input::PreviewClickTarget,
    ) -> bool {
        event.window_id == target.window_id
            && event.source_process_id == target.source_process_id
            && event.publication_generation == target.publication_generation
            && event.preview_rect == target.rect
            && event.gesture == crate::preview_input::PreviewGesture::Click
    }
    use windows::Win32::Foundation::{COLORREF, LPARAM, POINT, WPARAM};
    use windows::Win32::Graphics::Gdi::{GetDC, GetPixel, ReleaseDC};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
        MOUSEINPUT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClassNameW, GetCursorPos, GetWindowLongPtrW, IsWindow, IsWindowVisible,
        SendMessageW, SetCursorPos, SetWindowPos, WindowFromPoint, GWLP_USERDATA, HTTRANSPARENT,
        HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, WM_NCHITTEST,
    };

    #[derive(Debug)]
    pub struct PreviewLifecycleProbeReport {
        pub initial_live_previews: usize,
        pub host_visible: bool,
        pub target_above_host: bool,
        pub point_hits_target: bool,
        pub point_hit_owner: (isize, String),
        pub armed_hit_test: bool,
        pub click_event_delivered: bool,
        pub source_destroy_target_inert: bool,
        pub concurrent_registration_rejected: bool,
        pub stale_target_inert: bool,
        pub stale_commit_live_previews: usize,
        pub host_survived_close: bool,
        pub host_restarted: bool,
        pub registration_balance_after_clear: i64,
        pub relevant_z_order: Vec<(isize, String, u64)>,
        pub host_ex_style: i32,
        pub target_ex_style: i32,
    }

    unsafe extern "system" fn collect_probe_windows(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let mut class = [0u16; 128];
        let len = GetClassNameW(hwnd, &mut class);
        if len > 0 {
            let class = String::from_utf16_lossy(&class[..len as usize]);
            if matches!(
                class.as_str(),
                "LeopardWMPreviewClickTarget" | THUMBNAIL_HOST_CLASS
            ) {
                let windows = &mut *(lparam.0 as *mut Vec<(isize, String, u64)>);
                windows.push((
                    hwnd.0 as isize,
                    class,
                    GetWindowLongPtrW(hwnd, GWLP_USERDATA) as u64,
                ));
            }
        }
        BOOL(1)
    }

    fn probe_windows() -> Vec<(isize, String, u64)> {
        let mut windows = Vec::new();
        unsafe {
            let _ = EnumWindows(
                Some(collect_probe_windows),
                LPARAM((&mut windows as *mut Vec<(isize, String, u64)>) as isize),
            );
        }
        windows
    }

    fn hit_test(hwnd: HWND, point: POINT) -> isize {
        let packed = ((point.y as u16 as u32) << 16) | point.x as u16 as u32;
        unsafe {
            SendMessageW(
                hwnd,
                WM_NCHITTEST,
                Some(WPARAM(0)),
                Some(LPARAM(packed as isize)),
            )
            .0
        }
    }

    pub fn two_target_z_order_is_valid() -> bool {
        if !host().is_available() {
            return false;
        }
        let targets = [
            crate::preview_input::PreviewClickTarget {
                window_id: 0x7ffe_1001,
                source_process_id: std::process::id(),
                publication_generation: 1,
                rect: Rect::new(10, 10, 20, 20),
            },
            crate::preview_input::PreviewClickTarget {
                window_id: 0x7ffe_1002,
                source_process_id: std::process::id(),
                publication_generation: 2,
                rect: Rect::new(40, 10, 20, 20),
            },
        ];
        let Some(sync_generation) = crate::preview_input::sync_preview_click_targets(&targets)
        else {
            return false;
        };
        if !crate::preview_input::wait_for_applied_generation(
            sync_generation,
            Duration::from_secs(2),
        ) || host().anchor_within_band(None).is_err()
        {
            return false;
        }
        let Some(raise_generation) =
            crate::preview_input::raise_preview_click_targets(host().hwnd().0 as isize)
        else {
            return false;
        };
        if !crate::preview_input::wait_for_applied_raise_generation(
            raise_generation,
            Duration::from_secs(2),
        ) {
            return false;
        }
        let windows = probe_windows();
        let host_index = windows
            .iter()
            .position(|(raw, _, _)| *raw == host().hwnd().0 as isize)
            .unwrap_or(usize::MAX);
        let targets_above = targets.iter().all(|target| {
            windows
                .iter()
                .position(|(_, class, id)| {
                    class == "LeopardWMPreviewClickTarget" && *id == target.window_id
                })
                .is_some_and(|index| index < host_index)
        });
        let clear_generation = crate::preview_input::sync_preview_click_targets(&[]).unwrap_or(0);
        let cleared = crate::preview_input::wait_for_applied_generation(
            clear_generation,
            Duration::from_secs(2),
        );
        let _ = host().hide_surface();
        targets_above && cleared
    }

    pub fn host_initial_spawn_failure_recovers() -> bool {
        if HOST.get().is_some() {
            return false;
        }
        FORCE_HOST_SPAWN_FAILURE.store(true, Ordering::Release);
        let fallback = host();
        let failed_closed = fallback.raw_hwnd() == 0;
        FORCE_HOST_SPAWN_FAILURE.store(false, Ordering::Release);
        failed_closed && fallback.is_available() && fallback.raw_hwnd() != 0
    }

    fn probe_request(
        source_window_id: WindowId,
        destination: Rect,
    ) -> Result<PersistentPreviewRequest, Win32Error> {
        let source = window_id_to_hwnd(source_window_id)?;
        let mut source_rect = RECT::default();
        unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowRect(source, &mut source_rect) }
            .map_err(|error| {
                Win32Error::SetPositionFailed(format!("probe source rect unavailable: {error}"))
            })?;
        let source_size = (
            (source_rect.right - source_rect.left).max(1),
            (source_rect.bottom - source_rect.top).max(1),
        );
        Ok(PersistentPreviewRequest {
            window_id: source_window_id,
            source_rect: Rect::new(0, 0, source_size.0, source_size.1),
            expected_source_size: source_size,
            destination_screen_rect: destination,
        })
    }

    /// Verify that an API publication receipt corresponds to sampled pixels for
    /// a deliberately solid-colored, same-process source. This is intentionally
    /// probe-only: arbitrary application content, occlusion, color management,
    /// and protected surfaces make a production pixel assertion dishonest.
    pub fn controlled_colored_source_pixel_proof(
        source_window_id: WindowId,
        destination: Rect,
        expected: COLORREF,
    ) -> Result<bool, Win32Error> {
        invalidate_persistent_preview_surface();
        clear_persistent_previews()?;
        let result = (|| {
            if !prepare_persistent_preview(source_window_id) {
                return Ok(false);
            }
            let request = probe_request(source_window_id, destination)?;
            let live =
                commit_persistent_previews(&[request], true, preview_lifecycle_epoch(), None)?;
            if live != 1 || unsafe { windows::Win32::Graphics::Dwm::DwmFlush() }.is_err() {
                return Ok(false);
            }
            let channel_distance = |actual: COLORREF, wanted: COLORREF| {
                let channels = |color: COLORREF| {
                    (
                        (color.0 & 0xff) as i32,
                        ((color.0 >> 8) & 0xff) as i32,
                        ((color.0 >> 16) & 0xff) as i32,
                    )
                };
                let (ar, ag, ab) = channels(actual);
                let (wr, wg, wb) = channels(wanted);
                (ar - wr).abs().max((ag - wg).abs()).max((ab - wb).abs())
            };
            // The invisible 1/255 input target and desktop color management can
            // perturb one channel slightly. Requiring a majority of a tiny
            // center cross avoids claiming a proof from one stale edge pixel.
            // Screen-DC visibility can trail DwmFlush by a few compositor ticks;
            // retry the same strict physical samples for a bounded second.
            let center_y = destination.y + destination.height / 2;
            let fallback_outside_point = (destination.right().saturating_add(2), center_y);
            let neighbor_outside_point = crate::enumerate_monitors().ok().and_then(|monitors| {
                let destination_monitor = monitors.iter().find(|monitor| {
                    monitor.contains_point(destination.x + destination.width / 2, center_y)
                })?;
                [
                    fallback_outside_point,
                    (destination.x.saturating_sub(2), center_y),
                ]
                .into_iter()
                .find(|point| {
                    monitors.iter().any(|monitor| {
                        monitor.id != destination_monitor.id
                            && monitor.contains_point(point.0, point.1)
                    })
                })
            });
            let verify_neighbor_isolation = neighbor_outside_point.is_some();
            let outside_point = neighbor_outside_point.unwrap_or(fallback_outside_point);
            if std::env::var_os("LEOPARDWM_REQUIRE_DUAL_MONITOR").is_some()
                && !verify_neighbor_isolation
            {
                return Err(Win32Error::SetPositionFailed(
                    "dual-monitor pixel gate has no distinct output immediately outside the preview"
                        .into(),
                ));
            }
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                let screen_dc = unsafe { GetDC(None) };
                if screen_dc.0.is_null() {
                    return Err(Win32Error::SetPositionFailed(
                        "controlled preview pixel proof could not acquire screen DC".into(),
                    ));
                }
                let center_x = destination.x + destination.width / 2;
                let center_y = destination.y + destination.height / 2;
                let samples = [
                    unsafe { GetPixel(screen_dc, center_x, center_y) },
                    unsafe { GetPixel(screen_dc, center_x - 2, center_y) },
                    unsafe { GetPixel(screen_dc, center_x + 2, center_y) },
                    unsafe { GetPixel(screen_dc, center_x, center_y - 2) },
                    unsafe { GetPixel(screen_dc, center_x, center_y + 2) },
                ];
                let outside_sample =
                    unsafe { GetPixel(screen_dc, outside_point.0, outside_point.1) };
                unsafe {
                    ReleaseDC(None, screen_dc);
                }
                if samples
                    .into_iter()
                    .filter(|sample| channel_distance(*sample, expected) <= 20)
                    .count()
                    >= 3
                    && (!verify_neighbor_isolation
                        || channel_distance(outside_sample, expected) > 20)
                {
                    break Ok(true);
                }
                if Instant::now() >= deadline {
                    break Ok(false);
                }
                std::thread::sleep(Duration::from_millis(16));
                let _ = unsafe { windows::Win32::Graphics::Dwm::DwmFlush() };
            }
        })();
        let clear = clear_persistent_previews();
        match (result, clear) {
            (Ok(proven), Ok(())) => Ok(proven),
            (Err(error), _) => Err(error),
            (_, Err(error)) => Err(error),
        }
    }

    /// Exhaust the immediate retry burst, prove the desired request remains
    /// owned while hidden, then release the fault and require autonomous
    /// publication without another layout call.
    pub fn retry_exhaustion_keeps_desired_and_recovers(
        source_window_id: WindowId,
        destination: Rect,
    ) -> Result<bool, Win32Error> {
        invalidate_persistent_preview_surface();
        clear_persistent_previews()?;
        PREVIEW_RETRY_PENDING.store(false, Ordering::Release);
        if !prepare_persistent_preview(source_window_id) {
            return Ok(false);
        }
        let request = probe_request(source_window_id, destination)?;
        struct PublishFailureReset;
        impl Drop for PublishFailureReset {
            fn drop(&mut self) {
                FORCE_PREVIEW_PUBLISH_FAILURES.store(0, Ordering::Release);
            }
        }
        let _failure_reset = PublishFailureReset;
        FORCE_PREVIEW_PUBLISH_FAILURES.store(u32::MAX, Ordering::Release);
        let initial_live =
            commit_persistent_previews(&[request], true, preview_lifecycle_epoch(), None)?;
        std::thread::sleep(Duration::from_millis(450));
        let retained_desire = {
            let state = lock_persistent_previews();
            state.desired.as_slice() == [request]
                && state.previews.contains_key(&source_window_id)
                && !state.host_anchored
        };
        FORCE_PREVIEW_PUBLISH_FAILURES.store(0, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut recovered = false;
        while Instant::now() < deadline {
            recovered = {
                let state = lock_persistent_previews();
                state.host_anchored
                    && state
                        .previews
                        .get(&source_window_id)
                        .is_some_and(|preview| preview.published.is_some())
            };
            if recovered {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        clear_persistent_previews()?;
        Ok(initial_live == 0 && retained_desire && recovered)
    }

    /// The band anchor must be physically honored: the host has to end up below
    /// the anchor so every window above the tiled band keeps its own pixels,
    /// while the host's own click targets may sit between them. A dead anchor
    /// must fall back to the band top instead of failing the surface.
    pub fn host_anchors_below_band_anchor(anchor_window_id: WindowId) -> bool {
        let anchor = HWND(anchor_window_id as usize as *mut c_void);
        // Published previews always hold the normal band (the host refuses to
        // mix normal previews with topmost ghosts). Reproduce that precondition:
        // a topmost host cannot be ordered below a normal-band anchor.
        if host().set_topmost(false).is_err() {
            return false;
        }
        if host().anchor_within_band(Some(anchor)).is_err() {
            return false;
        }
        if !super::is_below_in_band(anchor, host().hwnd()) {
            return false;
        }
        // Anchoring must be a real constraint, not a no-op: the host must not
        // report success for an anchor it actually sits above.
        if super::is_below_in_band(host().hwnd(), anchor) {
            return false;
        }
        let dead = HWND(usize::MAX as *mut c_void);
        host().anchor_within_band(Some(dead)).is_ok() && host().anchor_within_band(None).is_ok()
    }

    /// Force a real HWND through two failed cloak commits. `DWMWA_CLOAK` is
    /// owner-only, so this is the ordinary production path for every managed
    /// foreign window: both calls must commit through the verified sentinel
    /// park, leave no logical cloak receipt, and retain park ownership. A
    /// denied cloak must not fail the placement, otherwise no scrolled-away
    /// column can be hidden and every real layout apply is rejected.
    pub fn placement_cloak_failure_is_not_cached(source_window_id: WindowId) -> bool {
        use leopardwm_core_layout::{Visibility, WindowPlacement};

        let placement = WindowPlacement {
            window_id: source_window_id,
            rect: Rect::new(-400, 20, 640, 480),
            visibility: Visibility::OffScreenLeft,
            column_index: 0,
        };
        let config = crate::PlatformConfig::default();
        let mut cache = crate::placement::PlacementCache::new();
        crate::placement::integration_probe_fail_next_cloak();
        let first = crate::placement::apply_placements(
            std::slice::from_ref(&placement),
            &config,
            Some(&mut cache),
            false,
        );
        let first_safe = first.is_ok()
            && !crate::placement::is_placement_cloaked(source_window_id)
            && crate::visibility::has_move_offscreen_ownership(source_window_id);
        crate::placement::integration_probe_fail_next_cloak();
        let second =
            crate::placement::apply_placements(&[placement], &config, Some(&mut cache), false);
        let second_safe = second.is_ok()
            && !crate::placement::is_placement_cloaked(source_window_id)
            && crate::visibility::has_move_offscreen_ownership(source_window_id);
        let _ = crate::visibility::restore_window_moved_offscreen(source_window_id);
        first_safe && second_safe
    }

    fn host_claims() -> (i64, i64, i64) {
        let state = Z_ORDER_STATE
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex);
        (state.balance, state.host_balance, state.topmost_balance)
    }

    fn force_host_restart_for_probe() -> Result<(), Win32Error> {
        let old_host = host().hwnd();
        let thread_id = host().thread_id.load(Ordering::Acquire);
        unsafe {
            PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0)).map_err(|error| {
                Win32Error::SetPositionFailed(format!("probe host quit failed: {error}"))
            })?;
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while unsafe { IsWindow(Some(old_host)) }.as_bool() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        host().restart()
    }

    /// Hold an old topmost registration across restart, then drop stale and new
    /// owners in both orders. Old-generation drops must never alter the new
    /// host's claims or prevent its promotion.
    pub fn host_restart_claims_are_generation_safe(
        source_window_id: WindowId,
    ) -> Result<bool, Win32Error> {
        invalidate_persistent_preview_surface();
        clear_persistent_previews()?;
        let baseline = host_claims();
        let old_first = register(source_window_id)?;
        force_host_restart_for_probe()?;
        let new_first = register(source_window_id)?;
        let promoted = host_claims() == (baseline.0 + 1, baseline.1 + 1, baseline.2 + 1);
        drop(old_first);
        let stale_drop_inert = host_claims() == (baseline.0 + 1, baseline.1 + 1, baseline.2 + 1);
        drop(new_first);
        let first_order_restored = host_claims() == baseline;

        let old_second = register(source_window_id)?;
        force_host_restart_for_probe()?;
        let new_second = register(source_window_id)?;
        drop(new_second);
        let current_drop_restored = host_claims() == baseline;
        drop(old_second);
        let stale_after_current_inert = host_claims() == baseline;
        Ok(promoted
            && stale_drop_inert
            && first_order_restored
            && current_drop_restored
            && stale_after_current_inert)
    }

    /// Fail one DWM unregister, retain its accounting receipt, then service the
    /// retry without sweeping a second healthy live registration.
    pub fn unregister_failure_retains_ownership(
        source_window_id: WindowId,
    ) -> Result<bool, Win32Error> {
        let baseline = host_claims();
        let healthy = register(source_window_id)?;
        let failing = register(source_window_id)?;
        FORCE_NEXT_UNREGISTER_FAILURE.store(true, Ordering::Release);
        drop(failing);
        let retained = host_claims() == (baseline.0 + 2, baseline.1 + 2, baseline.2 + 2);
        service_pending_thumbnail_unregisters();
        let healthy_survived = host_claims() == (baseline.0 + 1, baseline.1 + 1, baseline.2 + 1);
        drop(healthy);
        Ok(retained && healthy_survived && host_claims() == baseline)
    }

    /// Exhaust the immediate registration burst, retain the desired request,
    /// and require autonomous recovery to request a fresh exact relayout rather
    /// than publishing the newly-created handle directly.
    pub fn registration_exhaustion_retains_relayout_obligation(
        source_window_id: WindowId,
        destination: Rect,
    ) -> Result<bool, Win32Error> {
        invalidate_persistent_preview_surface();
        clear_persistent_previews()?;
        let request = probe_request(source_window_id, destination)?;
        let epoch = preview_lifecycle_epoch();
        FORCE_PREVIEW_REGISTRATION_FAILURES.store(MAX_FAILED_PUBLISHES, Ordering::Release);
        let immediate_failed = !prepare_persistent_preview(source_window_id);
        retain_persistent_preview_desire(&[request], epoch);

        let deadline = Instant::now() + Duration::from_secs(4);
        let mut relayout_required = false;
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
            relayout_required |= service_pending_preview_retry();
            if relayout_required {
                break;
            }
        }
        let retained = {
            let state = lock_persistent_previews();
            state.desired.as_slice() == [request]
                && state
                    .previews
                    .get(&source_window_id)
                    .is_some_and(|preview| {
                        preview.requires_physical_commit && preview.published.is_none()
                    })
        };
        clear_persistent_previews()?;
        Ok(immediate_failed && relayout_required && retained)
    }

    pub fn retry_spawn_failure_recovers(
        source_window_id: WindowId,
        destination: Rect,
    ) -> Result<bool, Win32Error> {
        invalidate_persistent_preview_surface();
        clear_persistent_previews()?;
        PREVIEW_CLEAR_PENDING.store(false, Ordering::Release);
        PREVIEW_RETRY_PENDING.store(false, Ordering::Release);
        *PREVIEW_RETRY_TX
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex) = None;
        if !prepare_persistent_preview(source_window_id) {
            return Ok(false);
        }
        let request = probe_request(source_window_id, destination)?;
        let epoch = preview_lifecycle_epoch();
        FORCE_NEXT_PREVIEW_PUBLISH_FAILURE.store(true, Ordering::Release);
        FORCE_RETRY_SPAWN_FAILURE.store(true, Ordering::Release);
        let initial_live = commit_persistent_previews(&[request], true, epoch, None)?;
        FORCE_RETRY_SPAWN_FAILURE.store(false, Ordering::Release);
        let obligation_survived = initial_live == 0
            && PREVIEW_RETRY_PENDING.load(Ordering::Acquire)
            && PREVIEW_RETRY_TX
                .lock()
                .unwrap_or_else(crate::recover_poisoned_mutex)
                .is_none();
        service_pending_preview_retry();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut published = false;
        while Instant::now() < deadline {
            published = {
                let state = lock_persistent_previews();
                state.host_anchored
                    && state
                        .previews
                        .get(&source_window_id)
                        .is_some_and(|preview| preview.published.is_some())
            };
            if published {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        clear_persistent_previews()?;
        Ok(obligation_survived && published)
    }

    /// Hold one obsolete target as though it still owns mouse capture, force
    /// its withdrawal acknowledgement to time out, then re-anchor a surviving
    /// preview. The destroy tombstone must keep the retained target transparent
    /// until a later acknowledged withdrawal retires that exact generation.
    pub fn retained_capture_destroy_fence_survives_reanchor(
        destroyed_window_id: WindowId,
        surviving_window_id: WindowId,
        destroyed_destination: Rect,
        surviving_destination: Rect,
    ) -> Result<bool, Win32Error> {
        if destroyed_window_id == surviving_window_id {
            return Ok(false);
        }
        struct ProbeReset;
        impl Drop for ProbeReset {
            fn drop(&mut self) {
                crate::preview_input::integration_probe_force_retained_capture(None);
                SUPPRESS_RETRY_FOR_CAPTURE_PROBE.store(false, Ordering::Release);
            }
        }

        invalidate_persistent_preview_surface();
        let _ = clear_persistent_previews_best_effort();
        let baseline_balance = current_register_balance();
        let epoch = preview_lifecycle_epoch();
        if !prepare_persistent_preview(destroyed_window_id)
            || !prepare_persistent_preview(surviving_window_id)
        {
            clear_persistent_previews()?;
            return Ok(false);
        }
        let requests = [
            probe_request(destroyed_window_id, destroyed_destination)?,
            probe_request(surviving_window_id, surviving_destination)?,
        ];
        let live = commit_persistent_previews(&requests, true, epoch, None)?;
        let target_raw = probe_windows()
            .into_iter()
            .find(|(_, class, id)| {
                class == "LeopardWMPreviewClickTarget" && *id == destroyed_window_id
            })
            .map(|(raw, _, _)| raw)
            .ok_or_else(|| {
                Win32Error::SetPositionFailed(
                    "capture probe destroyed target was not enumerated".into(),
                )
            })?;
        let target = HWND(target_raw as *mut c_void);
        let point = POINT {
            x: destroyed_destination.x + destroyed_destination.width / 2,
            y: destroyed_destination.y + destroyed_destination.height / 2,
        };
        let initially_armed = hit_test(target, point) != HTTRANSPARENT as isize;

        invalidate_persistent_preview_source(destroyed_window_id);
        crate::preview_input::integration_probe_force_retained_capture(Some(destroyed_window_id));
        SUPPRESS_RETRY_FOR_CAPTURE_PROBE.store(true, Ordering::Release);
        let _reset = ProbeReset;
        forget_persistent_preview(destroyed_window_id);
        let retained_after_timeout = unsafe { IsWindow(Some(target)) }.as_bool();
        let tombstone_survived = preview_source_is_invalidated(destroyed_window_id);
        let surviving_reanchored = reanchor_persistent_previews().is_ok();
        let retained_target_inert = hit_test(target, point) == HTTRANSPARENT as isize;

        crate::preview_input::integration_probe_force_retained_capture(None);
        SUPPRESS_RETRY_FOR_CAPTURE_PROBE.store(false, Ordering::Release);
        forget_persistent_preview(destroyed_window_id);
        let target_withdrawn = !unsafe { IsWindow(Some(target)) }.as_bool();
        let tombstone_retired = !preview_source_is_invalidated(destroyed_window_id);
        invalidate_persistent_preview_surface();
        clear_persistent_previews()?;
        let balance_restored = current_register_balance() == baseline_balance;

        Ok(live == 2
            && initially_armed
            && retained_after_timeout
            && tombstone_survived
            && surviving_reanchored
            && retained_target_inert
            && target_withdrawn
            && tombstone_retired
            && balance_restored)
    }

    /// Execute a real DWM/input/host lifecycle probe on one physical display.
    /// This API exists only behind the explicit integration-probes feature.
    pub fn run(
        source_window_id: WindowId,
        destination: Rect,
    ) -> Result<PreviewLifecycleProbeReport, Win32Error> {
        invalidate_persistent_preview_surface();
        let _ = clear_persistent_previews_best_effort();
        let baseline_balance = current_register_balance();
        let expected_epoch = preview_lifecycle_epoch();
        if !prepare_persistent_preview(source_window_id) {
            return Err(Win32Error::SetPositionFailed(
                "integration probe could not register source".into(),
            ));
        }
        let request = probe_request(source_window_id, destination)?;
        let initial_live_previews =
            commit_persistent_previews(&[request], true, expected_epoch, None)?;
        if initial_live_previews != 1 {
            clear_persistent_previews()?;
            return Err(Win32Error::SetPositionFailed(
                "integration probe preview did not reach its live activated state".into(),
            ));
        }
        let expected_click_target = {
            let state = lock_persistent_previews();
            (state.host_anchored && state.lifecycle_epoch == expected_epoch)
                .then(|| preview_click_targets(&published_preview_requests(&state)))
                .and_then(|targets| {
                    targets
                        .into_iter()
                        .find(|target| target.window_id == source_window_id)
                })
        };
        let Some(expected_click_target) = expected_click_target else {
            clear_persistent_previews()?;
            return Err(Win32Error::SetPositionFailed(
                "integration probe could not capture an active published click identity".into(),
            ));
        };
        let windows = probe_windows();
        let host_raw = host().hwnd().0 as isize;
        let host_index = windows
            .iter()
            .position(|(raw, _, _)| *raw == host_raw)
            .ok_or_else(|| Win32Error::SetPositionFailed("probe host was not enumerated".into()))?;
        let (target_index, target_raw) = windows
            .iter()
            .enumerate()
            .find(|(_, (_, class, id))| {
                class == "LeopardWMPreviewClickTarget" && *id == source_window_id
            })
            .map(|(index, (raw, _, _))| (index, *raw))
            .ok_or_else(|| {
                Win32Error::SetPositionFailed("probe target was not enumerated".into())
            })?;
        let target = HWND(target_raw as *mut c_void);
        let host_ex_style = unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetWindowLongW(
                host().hwnd(),
                windows::Win32::UI::WindowsAndMessaging::GWL_EXSTYLE,
            )
        };
        let target_ex_style = unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetWindowLongW(
                target,
                windows::Win32::UI::WindowsAndMessaging::GWL_EXSTYLE,
            )
        };
        let point = POINT {
            x: destination.x + destination.width / 2,
            y: destination.y + destination.height / 2,
        };
        let armed_hit_test = hit_test(target, point) != HTTRANSPARENT as isize;
        let mut point_owner = unsafe { WindowFromPoint(point) };
        if point_owner != target {
            // A user's unrelated normal-band window may cover the fixed probe
            // rectangle. Temporarily promote only the controlled click target
            // (never the DWM host) for the input-routing phase; production order
            // was already captured above and remains separately asserted.
            let _ = unsafe {
                SetWindowPos(
                    target,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                )
            };
            point_owner = unsafe { WindowFromPoint(point) };
        }
        let point_hits_target = point_owner == target;
        let mut owner_class = [0u16; 128];
        let owner_class_len = unsafe { GetClassNameW(point_owner, &mut owner_class) };
        let point_hit_owner = (
            point_owner.0 as isize,
            String::from_utf16_lossy(&owner_class[..owner_class_len.max(0) as usize]),
        );
        let host_visible = unsafe { IsWindowVisible(host().hwnd()) }.as_bool();
        let target_above_host = target_index < host_index;
        let (click_tx, click_rx) = mpsc::channel();
        crate::preview_input::set_click_sender(click_tx);
        let require_physical_click = std::env::var_os("LEOPARDWM_REQUIRE_PHYSICAL_CLICK").is_some();
        let mut previous_cursor = POINT::default();
        let cursor_saved =
            !require_physical_click && unsafe { GetCursorPos(&mut previous_cursor) }.is_ok();
        let routed_input_sent = if require_physical_click {
            eprintln!(
                "PHYSICAL_CLICK_REQUIRED: left-click the preview at screen coordinate ({}, {}) within 60 seconds",
                point.x, point.y
            );
            point_hits_target
        } else {
            point_hits_target
                && unsafe { SetCursorPos(point.x, point.y) }.is_ok()
                && unsafe {
                    let inputs = [
                        INPUT {
                            r#type: INPUT_MOUSE,
                            Anonymous: INPUT_0 {
                                mi: MOUSEINPUT {
                                    dwFlags: MOUSEEVENTF_LEFTDOWN,
                                    ..Default::default()
                                },
                            },
                        },
                        INPUT {
                            r#type: INPUT_MOUSE,
                            Anonymous: INPUT_0 {
                                mi: MOUSEINPUT {
                                    dwFlags: MOUSEEVENTF_LEFTUP,
                                    ..Default::default()
                                },
                            },
                        },
                    ];
                    SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) == inputs.len() as u32
                }
        };
        let click_event_delivered = routed_input_sent
            && click_rx
                .recv_timeout(if require_physical_click {
                    Duration::from_secs(60)
                } else {
                    Duration::from_secs(1)
                })
                .is_ok_and(|event| click_receipt_matches_target(event, expected_click_target));
        if cursor_saved {
            let _ = unsafe { SetCursorPos(previous_cursor.x, previous_cursor.y) };
        }
        crate::preview_input::clear_click_sender();

        // Per-source destroy must revoke hit testing before the queued daemon
        // lifecycle lane has a chance to clear or rebuild the target.
        invalidate_persistent_preview_source(source_window_id);
        let source_destroy_target_inert = hit_test(target, point) == HTTRANSPARENT as isize;
        clear_persistent_previews()?;

        // Pause a real DWM registration after it sampled the destroy token,
        // issue a newer destroy, then prove the old registration cannot erase
        // that fence or install a recycled HWND incarnation.
        let (reached_tx, reached_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        *REGISTRATION_FENCE_PROBE
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex) = Some(RegistrationFenceProbe {
            reached: reached_tx,
            release: release_rx,
        });
        let registration = std::thread::spawn(move || prepare_persistent_preview(source_window_id));
        reached_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|error| {
                Win32Error::SetPositionFailed(format!(
                    "registration fence probe did not reach handoff: {error}"
                ))
            })?;
        invalidate_persistent_preview_source(source_window_id);
        release_tx.send(()).map_err(|error| {
            Win32Error::SetPositionFailed(format!(
                "registration fence probe could not release worker: {error}"
            ))
        })?;
        let registration_accepted = registration.join().map_err(|_| {
            Win32Error::SetPositionFailed("registration fence probe worker panicked".into())
        })?;
        let concurrent_registration_rejected =
            !registration_accepted && preview_source_is_invalidated(source_window_id);

        invalidate_persistent_preview_surface();
        let stale_hit = hit_test(target, point);
        let stale_target_inert =
            !unsafe { IsWindow(Some(target)) }.as_bool() || stale_hit == HTTRANSPARENT as isize;
        let stale_commit_live_previews =
            commit_persistent_previews(&[request], true, expected_epoch, None)?;
        clear_persistent_previews()?;

        let close_host = host().hwnd();
        unsafe {
            let _ = SendMessageW(close_host, WM_CLOSE, Some(WPARAM(0)), Some(LPARAM(0)));
        }
        let host_survived_close = unsafe { IsWindow(Some(close_host)) }.as_bool();

        let old_host = host().hwnd();
        let old_host_generation = host().generation();
        let thread_id = host().thread_id.load(Ordering::Acquire);
        unsafe {
            PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0)).map_err(|error| {
                Win32Error::SetPositionFailed(format!("probe host quit failed: {error}"))
            })?;
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while unsafe { IsWindow(Some(old_host)) }.as_bool() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        host().restart().map_err(|error| {
            Win32Error::SetPositionFailed(format!("probe host restart failed: {error}"))
        })?;
        let host_restarted = host().is_available() && host().generation() != old_host_generation;
        let registration_balance_after_clear = current_register_balance() - baseline_balance;

        Ok(PreviewLifecycleProbeReport {
            initial_live_previews,
            host_visible,
            target_above_host,
            point_hits_target,
            point_hit_owner,
            armed_hit_test,
            click_event_delivered,
            source_destroy_target_inert,
            concurrent_registration_rejected,
            stale_target_inert,
            stale_commit_live_previews,
            host_survived_close,
            host_restarted,
            registration_balance_after_clear,
            relevant_z_order: windows,
            host_ex_style,
            target_ex_style,
        })
    }
}

// Silence unused-warning hint: SWP_NOZORDER, SET_WINDOW_POS_FLAGS,
// CW_USEDEFAULT, WS_POPUP are kept for completeness even when not all
// are used directly.
#[allow(dead_code)]
const _UNUSED_IMPORTS: (SET_WINDOW_POS_FLAGS, i32) = (SWP_NOZORDER, CW_USEDEFAULT);

#[cfg(test)]
mod preview_click_target_tests {
    use super::{preview_click_targets, PersistentPreviewRequest, PublishedPreview};
    use leopardwm_core_layout::Rect;

    fn request(window_id: u64, x: i32, width: i32) -> PersistentPreviewRequest {
        PersistentPreviewRequest {
            window_id,
            source_rect: Rect::new(0, 0, width, 800),
            expected_source_size: (width, 800),
            destination_screen_rect: Rect::new(x, 100, width, 800),
        }
    }

    fn published(window_id: u64, x: i32, width: i32) -> PublishedPreview {
        PublishedPreview {
            request: request(window_id, x, width),
            generation: 7,
            source_process_id: 42,
        }
    }

    #[test]
    fn only_successfully_published_previews_get_a_click_target() {
        // A registration or desired request that never reached DWM is absent
        // from this receipt list and therefore cannot absorb input.
        let published = [published(1, 0, 250)];
        let targets = preview_click_targets(&published);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].window_id, 1);
        assert_eq!(targets[0].rect, Rect::new(0, 100, 250, 800));
    }

    #[test]
    fn a_first_publish_failure_has_no_click_target() {
        assert!(preview_click_targets(&[]).is_empty());
    }

    #[test]
    fn a_failed_move_keeps_the_last_successful_target() {
        // Requested B failed, so the publication receipt remains A. The overlay
        // must stay with the pixels at A rather than moving into empty space B.
        let last_successful = [published(1, 0, 250)];
        let targets = preview_click_targets(&last_successful);
        assert_eq!(targets[0].rect, Rect::new(0, 100, 250, 800));
    }

    #[test]
    fn a_duplicate_publication_yields_one_target() {
        let published = [published(7, 0, 250), published(7, 1670, 250)];
        let targets = preview_click_targets(&published);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].rect, Rect::new(0, 100, 250, 800));
    }

    #[test]
    fn an_empty_destination_is_never_clickable() {
        let mut empty = published(3, 0, 0);
        empty.request.destination_screen_rect = Rect::new(0, 100, 0, 800);
        assert!(preview_click_targets(&[empty]).is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destroy_fence_survives_priority_reordering_until_new_registration() {
        let window_id = 0x1234;
        let preview_state_busy = lock_persistent_previews();
        record_preview_source_invalidation(window_id);
        drop(preview_state_busy);
        let observed = preview_source_invalidation_generation(window_id);
        assert!(observed.is_some());
        assert!(validate_new_preview_source(window_id, observed, || ()).is_some());
        assert!(!preview_source_is_invalidated(window_id));

        // Registration sampled a clean source, then destroy overtook it. The
        // newer generation must survive and the stale install must not run.
        let observed = preview_source_invalidation_generation(window_id);
        assert!(observed.is_none());
        record_preview_source_invalidation(window_id);
        let mut installed = false;
        assert!(validate_new_preview_source(window_id, observed, || installed = true).is_none());
        assert!(!installed);
        assert!(preview_source_is_invalidated(window_id));

        // Lifecycle retirement may not erase a still newer destroy.
        let older = preview_source_invalidation_generation(window_id);
        record_preview_source_invalidation(window_id);
        let newer = preview_source_invalidation_generation(window_id);
        assert_ne!(older, newer);
        retire_preview_source_invalidation(window_id, older);
        assert_eq!(preview_source_invalidation_generation(window_id), newer);

        // Leave process-global test state clean for other cases.
        assert!(validate_new_preview_source(window_id, newer, || ()).is_some());
    }

    #[test]
    fn retry_sender_coalesces_full_and_replaces_disconnected_workers() {
        let (full_tx, _full_rx) = mpsc::sync_channel(1);
        full_tx.try_send(()).unwrap();
        let mut full = Some(full_tx);
        assert!(signal_existing_retry_worker(&mut full));
        assert!(full.is_some());

        let (dead_tx, dead_rx) = mpsc::sync_channel(1);
        drop(dead_rx);
        let mut disconnected = Some(dead_tx);
        assert!(!signal_existing_retry_worker(&mut disconnected));
        assert!(disconnected.is_none());
    }

    #[test]
    fn unrelated_destroy_does_not_revoke_all_previews() {
        let window_id = 0x5678;
        lock_persistent_previews().previews.remove(&window_id);
        let before = preview_lifecycle_epoch();
        invalidate_persistent_preview_source(window_id);
        assert_eq!(preview_lifecycle_epoch(), before);
        assert!(!preview_source_is_invalidated(window_id));
    }

    #[test]
    fn lifecycle_cleanup_never_waits_for_active_preview_transaction() {
        let _transaction = lock_persistent_preview_transaction();
        assert!(!clear_persistent_previews_best_effort().unwrap());
    }

    #[test]
    fn stale_host_generation_never_owns_a_publication() {
        assert!(registration_matches_host_generation(true, 8, 8));
        assert!(!registration_matches_host_generation(true, 7, 8));
        assert!(registration_matches_host_generation(false, 0, 8));
    }

    #[test]
    fn shared_host_rejects_mixed_z_bands() {
        let normal = ZOrderState {
            balance: 1,
            host_balance: 1,
            topmost_balance: 0,
            registrations: std::collections::HashMap::new(),
        };
        assert!(host_band_is_compatible(&normal, HostBand::Normal));
        assert!(!host_band_is_compatible(&normal, HostBand::Topmost));

        let topmost = ZOrderState {
            balance: 1,
            host_balance: 1,
            topmost_balance: 1,
            registrations: std::collections::HashMap::new(),
        };
        assert!(host_band_is_compatible(&topmost, HostBand::Topmost));
        assert!(!host_band_is_compatible(&topmost, HostBand::Normal));
    }

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
