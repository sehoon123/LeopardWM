//! Window placement application via SetWindowPos / DeferWindowPos.

use crate::thumbnail::{
    commit_persistent_previews, forget_persistent_preview, has_persistent_preview,
    has_published_persistent_preview, lock_persistent_preview_transaction,
    prepare_persistent_preview, retain_persistent_preview_desire, PersistentPreviewRequest,
};
use crate::types::{AnimationPlacementPolicy, PlatformConfig, Win32Error};
use crate::window_id_to_hwnd;
use crate::window_region::{
    has_owned_window_region, reconcile_window_regions, restore_all_window_regions,
    restore_window_region, WindowRegionClip,
};
use leopardwm_core_layout::{Rect, Visibility, WindowId, WindowPlacement};
use std::collections::{HashMap, HashSet};
#[cfg(feature = "integration-probes")]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmFlush, DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS,
    DWMWINDOWATTRIBUTE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BeginDeferWindowPos, DeferWindowPos, EndDeferWindowPos, GetClassNameW, GetWindowRect,
    GetWindowThreadProcessId, IsHungAppWindow, IsIconic, IsWindow, IsZoomed, SetWindowPos,
    ShowWindow, SET_WINDOW_POS_FLAGS, SWP_ASYNCWINDOWPOS, SWP_FRAMECHANGED, SWP_NOACTIVATE,
    SWP_NOSIZE, SWP_NOZORDER, SW_RESTORE,
};

/// Undocumented but well-known DWM attribute for cloaking windows.
/// Cloaked windows remain composed by DWM (surface stays alive) but are
/// invisible to the user. Used by the Windows shell for virtual desktops.
const DWMWA_CLOAK: DWMWINDOWATTRIBUTE = DWMWINDOWATTRIBUTE(13i32);

/// Disable DWM-managed visual transitions (minimize/maximize fade,
/// position interpolation between SetWindowPos calls, etc.) on a
/// specific window. Tiling WMs want instant snap behavior, not DWM
/// smoothing — without this, dragging a window into a tabbed column
/// makes the dropped window visibly "slide" from the drop point to
/// its layout slot.
const DWMWA_TRANSITIONS_FORCEDISABLED: DWMWINDOWATTRIBUTE = DWMWINDOWATTRIBUTE(3i32);

/// Set or clear the DWM cloak on a window. Bypasses both `GLOBAL_CLOAKED`
/// and `GHOST_CLOAKED` — only callers that have already evaluated the
/// OR-cloak invariant (or recovery paths that want to force-uncloak
/// regardless) should call this directly.
#[cfg(feature = "integration-probes")]
static FORCE_NEXT_CLOAK_FAILURE: AtomicBool = AtomicBool::new(false);

/// Inject one failed cloak write for the owned-HWND integration probe. The
/// production path still receives the real DWM result, including the usual
/// `E_ACCESSDENIED` for a foreign source HWND.
#[cfg(feature = "integration-probes")]
pub(crate) fn integration_probe_fail_next_cloak() {
    FORCE_NEXT_CLOAK_FAILURE.store(true, Ordering::Release);
}

unsafe fn dwm_set_cloak(hwnd: HWND, cloaked: bool) -> bool {
    // NOTE: DWMWA_CLOAK only succeeds on windows owned by the calling
    // process; cloaking another process's window returns E_ACCESSDENIED
    // (0x80070005). Callers that require an actually-hidden source (the DWM
    // thumbnail animation) must check this result instead of treating logical
    // set membership as proof that the external HWND was cloaked.
    #[cfg(feature = "integration-probes")]
    if FORCE_NEXT_CLOAK_FAILURE.swap(false, Ordering::AcqRel) {
        return false;
    }
    let value = BOOL::from(cloaked);
    DwmSetWindowAttribute(
        hwnd,
        DWMWA_CLOAK,
        &value as *const _ as _,
        std::mem::size_of::<BOOL>() as u32,
    )
    .is_ok()
}

/// Serialize logical cloak-set mutations with the physical DWM commit. The
/// animation worker and daemon thread can otherwise race and leave DWM in the
/// opposite state from the final logical OR.
static CLOAK_COMMIT: Mutex<()> = Mutex::new(());

fn lock_cloak_commit() -> std::sync::MutexGuard<'static, ()> {
    CLOAK_COMMIT
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
}

/// Apply the current logical OR without acquiring `CLOAK_COMMIT`.
/// Callers must already hold the commit lock.
fn apply_cloak_state_locked(wid: WindowId) -> bool {
    let should_cloak = ghost_cloaked_contains(wid) || global_cloaked_contains(wid);
    let Ok(hwnd) = window_id_to_hwnd(wid) else {
        return false;
    };
    if !unsafe { IsWindow(Some(hwnd)).as_bool() } {
        return false;
    }
    unsafe { dwm_set_cloak(hwnd, should_cloak) }
}

fn global_cloak_receipt(wid: WindowId) -> Option<crate::event_hooks::WindowEventIdentity> {
    matching_cloak_receipt(&mut lock_cloaked(), wid)
}

fn global_cloaked_contains(wid: WindowId) -> bool {
    global_cloak_receipt(wid).is_some()
}

/// Change normal-placement cloak ownership only if the corresponding DWM
/// write commits. On failure restore the old logical receipt and re-apply that
/// receipt best-effort, so a transient access failure can never turn a
/// foreign/unresponsive HWND into a falsely cached hidden window.
///
/// Callers must hold `CLOAK_COMMIT`.
fn commit_global_cloak_state_locked(wid: WindowId, should_cloak: bool) -> bool {
    let previous = global_cloak_receipt(wid);
    let next = if should_cloak {
        let Some(identity) = crate::event_hooks::current_window_event_identity(wid) else {
            return false;
        };
        Some(identity)
    } else {
        None
    };
    if previous == next {
        return apply_cloak_state_locked(wid);
    }
    {
        let mut guard = lock_cloaked();
        let cloaked = guard.get_or_insert_with(HashMap::new);
        if let Some(identity) = next {
            cloaked.insert(wid, identity);
        } else {
            cloaked.remove(&wid);
        }
    }

    // If the HWND changed after the old receipt was read, retire that receipt
    // without issuing a DWM write against the replacement.
    if !should_cloak
        && previous.as_ref().is_some_and(|identity| {
            crate::event_hooks::current_window_event_identity(wid).as_ref() != Some(identity)
        })
    {
        return true;
    }
    if apply_cloak_state_locked(wid) {
        return true;
    }

    let mut guard = lock_cloaked();
    let cloaked = guard.get_or_insert_with(HashMap::new);
    if let Some(identity) = previous {
        cloaked.insert(wid, identity);
    } else {
        cloaked.remove(&wid);
    }
    drop(guard);
    let _ = apply_cloak_state_locked(wid);
    false
}

// ---------------------------------------------------------------------
// GHOST_CLOAKED — distinct cloak set populated only by the ghost-animation
// path. Logical-OR'd with GLOBAL_CLOAKED to determine the effective cloak
// state (see `apply_cloak_state`).
// ---------------------------------------------------------------------

type CloakLedger = HashMap<WindowId, crate::event_hooks::WindowEventIdentity>;

static GHOST_CLOAKED: Mutex<Option<CloakLedger>> = Mutex::new(None);

fn lock_ghost_cloaked() -> std::sync::MutexGuard<'static, Option<CloakLedger>> {
    GHOST_CLOAKED
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
}

fn reconcile_cloak_receipt(
    ledger: &mut Option<CloakLedger>,
    wid: WindowId,
    current: Option<&crate::event_hooks::WindowEventIdentity>,
) -> Option<crate::event_hooks::WindowEventIdentity> {
    let expected = ledger.as_ref()?.get(&wid)?.clone();
    if current == Some(&expected) {
        Some(expected)
    } else {
        if let Some(ledger) = ledger.as_mut() {
            ledger.remove(&wid);
        }
        None
    }
}

fn matching_cloak_receipt(
    ledger: &mut Option<CloakLedger>,
    wid: WindowId,
) -> Option<crate::event_hooks::WindowEventIdentity> {
    let current = crate::event_hooks::current_window_event_identity(wid);
    reconcile_cloak_receipt(ledger, wid, current.as_ref())
}

fn ghost_cloak_receipt(wid: WindowId) -> Option<crate::event_hooks::WindowEventIdentity> {
    matching_cloak_receipt(&mut lock_ghost_cloaked(), wid)
}

fn ghost_cloaked_contains(wid: WindowId) -> bool {
    ghost_cloak_receipt(wid).is_some()
}

/// Mark a source for ghost animation only when DWM physically cloaks it.
/// External application HWNDs normally reject DWMWA_CLOAK with
/// E_ACCESSDENIED; in that case roll back the logical mark so the caller can
/// safely fall back to live placement instead of drawing a thumbnail over an
/// uncloaked source.
pub fn try_mark_ghost_cloaked(wid: WindowId) -> bool {
    let _commit = lock_cloak_commit();
    let Some(identity) = crate::event_hooks::current_window_event_identity(wid) else {
        return false;
    };
    lock_ghost_cloaked()
        .get_or_insert_with(HashMap::new)
        .insert(wid, identity.clone());
    if apply_cloak_state_locked(wid)
        && crate::event_hooks::current_window_event_identity(wid).as_ref() == Some(&identity)
    {
        true
    } else {
        if let Some(ref mut set) = *lock_ghost_cloaked() {
            set.remove(&wid);
        }
        // A concurrent replacement must never inherit a cloak intended for the
        // source. This compensation is show-only.
        if crate::event_hooks::current_window_event_identity(wid).as_ref() != Some(&identity) {
            if let Ok(hwnd) = window_id_to_hwnd(wid) {
                let _ = unsafe { dwm_set_cloak(hwnd, false) };
            }
        } else {
            let _ = apply_cloak_state_locked(wid);
        }
        false
    }
}

/// Atomically remove a window from the ghost-cloak set and commit the new OR
/// state (which uncloaks it unless normal placement still requires a cloak).
pub fn unmark_ghost_cloaked(wid: WindowId) {
    let _commit = lock_cloak_commit();
    let Some(identity) = ghost_cloak_receipt(wid) else {
        unmark_ghost_cloaked_locked(wid);
        return;
    };
    unmark_ghost_cloaked_locked(wid);
    if crate::event_hooks::current_window_event_identity(wid).as_ref() != Some(&identity)
        || apply_cloak_state_locked(wid)
        || !crate::is_valid_window(wid)
    {
        return;
    }

    lock_ghost_cloaked()
        .get_or_insert_with(HashMap::new)
        .insert(wid, identity);
    let _ = apply_cloak_state_locked(wid);
    tracing::warn!("Retaining ghost-cloak recovery receipt for window {wid:#x}");
}

fn unmark_ghost_cloaked_locked(wid: WindowId) {
    let mut guard = lock_ghost_cloaked();
    if let Some(ref mut set) = *guard {
        set.remove(&wid);
    }
}

/// Logical-only removal for a proven recycled/dead HWND. Serializes the set
/// mutation but deliberately performs no DWM call on the potentially unrelated
/// handle now occupying the same numeric value.
pub fn forget_recycled_ghost_cloak(wid: WindowId) {
    let _commit = lock_cloak_commit();
    unmark_ghost_cloaked_locked(wid);
}

// ---------------------------------------------------------------------
// DIRECT_CLOAKED — windows cloaked outside the placement system (e.g. a
// stashed scratchpad window removed from all workspaces). NOT consulted by
// `apply_cloak_state` or `uncloak_all_tracked`, so normal placement never
// touches them — but `dwm_uncloak_all` drains it, so shutdown / panic /
// emergency-uncloak recovery always restores them. Without this set such a
// window would be cloaked with no recovery path = permanently invisible.
// ---------------------------------------------------------------------

static DIRECT_CLOAKED: Mutex<Option<CloakLedger>> = Mutex::new(None);

fn lock_direct_cloaked() -> std::sync::MutexGuard<'static, Option<CloakLedger>> {
    DIRECT_CLOAKED
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
}

fn direct_cloak_receipt(wid: WindowId) -> Option<crate::event_hooks::WindowEventIdentity> {
    matching_cloak_receipt(&mut lock_direct_cloaked(), wid)
}

/// Disable (or re-enable) DWM-managed visual transitions on a window.
/// Pass `true` to make subsequent `SetWindowPos` calls land instantly
/// without DWM's automatic position-interpolation smoothing.
pub fn set_dwm_transitions_disabled(window_id: WindowId, disabled: bool) {
    let Ok(hwnd) = window_id_to_hwnd(window_id) else {
        return;
    };
    unsafe {
        let value = BOOL::from(disabled);
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_TRANSITIONS_FORCEDISABLED,
            &value as *const _ as _,
            std::mem::size_of::<BOOL>() as u32,
        );
    }
}

/// Lock GLOBAL_CLOAKED, recovering from poison (a prior panic while holding
/// the lock). All access to the cloaked set goes through this helper so that
/// shutdown/panic cleanup paths never silently give up.
fn lock_cloaked() -> std::sync::MutexGuard<'static, Option<CloakLedger>> {
    GLOBAL_CLOAKED
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
}

/// Force-cloak a single window directly, without touching either tracking
/// set. For windows held OUTSIDE normal layout management (e.g. a stashed
/// scratchpad window that has been removed from its workspace) — nothing
/// in the placement path will reposition or uncloak it, so a direct cloak
/// is safe and stays put until the owner uncloaks it.
pub fn dwm_cloak_window(window_id: WindowId) -> bool {
    let _commit = lock_cloak_commit();
    let Some(identity) = crate::event_hooks::current_window_event_identity(window_id) else {
        return false;
    };
    let Ok(hwnd) = window_id_to_hwnd(window_id) else {
        return false;
    };
    if !unsafe { IsWindow(Some(hwnd)).as_bool() } || !unsafe { dwm_set_cloak(hwnd, true) } {
        return false;
    }
    if crate::event_hooks::current_window_event_identity(window_id).as_ref() != Some(&identity) {
        let _ = unsafe { dwm_set_cloak(hwnd, false) };
        return false;
    }
    lock_direct_cloaked()
        .get_or_insert_with(HashMap::new)
        .insert(window_id, identity);
    true
}

/// Force-uncloak a window by its WindowId regardless of either tracking
/// set's membership. Removes from both `GLOBAL_CLOAKED` and
/// `GHOST_CLOAKED`. Used by shutdown / panic cleanup.
///
/// Bypasses `apply_cloak_state`'s OR-check: the intent here is "force
/// visible" regardless of why the window was originally cloaked.
pub fn dwm_uncloak_window(window_id: WindowId) {
    let _commit = lock_cloak_commit();
    let global = global_cloak_receipt(window_id);
    let ghost = ghost_cloak_receipt(window_id);
    let direct = direct_cloak_receipt(window_id);
    let expected = global
        .as_ref()
        .or(ghost.as_ref())
        .or(direct.as_ref())
        .cloned();

    if let Some(set) = lock_cloaked().as_mut() {
        set.remove(&window_id);
    }
    unmark_ghost_cloaked_locked(window_id);
    if let Some(set) = lock_direct_cloaked().as_mut() {
        set.remove(&window_id);
    }

    let Some(expected) = expected else {
        return;
    };
    if crate::event_hooks::current_window_event_identity(window_id).as_ref() != Some(&expected) {
        return;
    }
    let uncloaked = window_id_to_hwnd(window_id)
        .ok()
        .is_some_and(|hwnd| unsafe {
            IsWindow(Some(hwnd)).as_bool() && dwm_set_cloak(hwnd, false)
        });
    if uncloaked {
        return;
    }

    if let Some(identity) = global {
        lock_cloaked()
            .get_or_insert_with(HashMap::new)
            .insert(window_id, identity);
    }
    if let Some(identity) = ghost {
        lock_ghost_cloaked()
            .get_or_insert_with(HashMap::new)
            .insert(window_id, identity);
    }
    if let Some(identity) = direct {
        lock_direct_cloaked()
            .get_or_insert_with(HashMap::new)
            .insert(window_id, identity);
    }
}

/// Force-uncloak every tracked window from both sets. Called during
/// shutdown and panic recovery. Bypasses `apply_cloak_state`.
pub fn dwm_uncloak_all() {
    invalidate_preview_surface_and_clear_best_effort("global uncloak");
    restore_all_window_regions();
    let _commit = lock_cloak_commit();
    let mut ids = HashSet::new();
    if let Some(set) = lock_cloaked().as_ref() {
        ids.extend(set.keys().copied());
    }
    if let Some(set) = lock_ghost_cloaked().as_ref() {
        ids.extend(set.keys().copied());
    }
    if let Some(set) = lock_direct_cloaked().as_ref() {
        ids.extend(set.keys().copied());
    }

    for window_id in ids {
        let global = global_cloak_receipt(window_id);
        let ghost = ghost_cloak_receipt(window_id);
        let direct = direct_cloak_receipt(window_id);
        let expected = global.as_ref().or(ghost.as_ref()).or(direct.as_ref());
        let Some(expected) = expected else {
            continue;
        };
        if crate::event_hooks::current_window_event_identity(window_id).as_ref() != Some(expected) {
            continue;
        }
        let uncloaked = window_id_to_hwnd(window_id)
            .ok()
            .is_some_and(|hwnd| unsafe {
                IsWindow(Some(hwnd)).as_bool() && dwm_set_cloak(hwnd, false)
            });
        if !uncloaked {
            continue;
        }
        if let Some(set) = lock_cloaked().as_mut() {
            set.remove(&window_id);
        }
        if let Some(set) = lock_ghost_cloaked().as_mut() {
            set.remove(&window_id);
        }
        if let Some(set) = lock_direct_cloaked().as_mut() {
            set.remove(&window_id);
        }
    }
}

/// Check if a window is currently cloaked by the placement system OR the
/// ghost-animation system. Used by the event hook to suppress spurious
/// SHOW/LOCATIONCHANGE events fired by DWM when we cloak/uncloak windows
/// during placement or ghost transitions.
///
/// Returns the logical OR of `GLOBAL_CLOAKED` (off-screen parking) and
/// `GHOST_CLOAKED` (ghost-animation in flight) membership.
pub fn is_placement_cloaked(window_id: WindowId) -> bool {
    let Some(current) = crate::event_hooks::current_window_event_identity_nonblocking(window_id)
    else {
        return false;
    };
    let contains = |ledger: &'static Mutex<Option<CloakLedger>>| match ledger.try_lock() {
        Ok(mut ledger) => reconcile_cloak_receipt(&mut ledger, window_id, Some(&current)).is_some(),
        Err(std::sync::TryLockError::Poisoned(error)) => {
            let mut ledger = error.into_inner();
            reconcile_cloak_receipt(&mut ledger, window_id, Some(&current)).is_some()
        }
        // Emitting one redundant WinEvent is safer than stalling a system
        // callback behind a placement transaction.
        Err(std::sync::TryLockError::WouldBlock) => false,
    };
    contains(&GLOBAL_CLOAKED) || contains(&GHOST_CLOAKED)
}

/// Drain and uncloak all tracked windows. Called when the placement list
/// becomes empty (e.g., switching to an empty workspace) so that windows
/// from the previous call are not left permanently invisible.
fn uncloak_all_tracked() {
    let _commit = lock_cloak_commit();
    let ids: Vec<WindowId> = lock_cloaked()
        .as_ref()
        .map(|set| set.keys().copied().collect())
        .unwrap_or_default();
    for wid in ids {
        // Keep a failed uncloak receipt instead of draining it before DWM has
        // accepted the visibility transition.
        let _ = commit_global_cloak_state_locked(wid, false);
    }
}

/// Global set of window IDs currently cloaked by the placement system.
static GLOBAL_CLOAKED: Mutex<Option<CloakLedger>> = Mutex::new(None);

/// Cache of last-applied window placements and border insets.
///
/// The position cache skips redundant SetWindowPos calls during animations.
/// The inset cache preserves known-good invisible border insets so that windows
/// returning from off-screen (where DWM may lose track of extended frame bounds)
/// are positioned correctly.
pub struct PlacementCache {
    positions: HashMap<WindowId, (Rect, Visibility)>,
    insets: HashMap<WindowId, (i32, i32, i32, i32)>,
    /// Renderer classification cached for the lifetime of a placement entry.
    /// It is pruned as soon as the HWND leaves the current layout, preventing
    /// recycled handles from inheriting an old animation policy.
    compositor_sensitive: HashMap<WindowId, bool>,
    /// Generation of `GLOBAL_INSET_CACHE` reflected by `insets`. An atomic
    /// generation lets display/theme/DPI changes invalidate the animation
    /// worker's thread-local cache without locking on every frame.
    inset_generation: u64,
}

impl Default for PlacementCache {
    fn default() -> Self {
        Self::new()
    }
}

impl PlacementCache {
    pub fn new() -> Self {
        Self {
            positions: HashMap::new(),
            insets: HashMap::new(),
            compositor_sensitive: HashMap::new(),
            inset_generation: INSET_CACHE_GENERATION.load(Ordering::Acquire),
        }
    }

    pub fn clear(&mut self) {
        self.positions.clear();
        self.compositor_sensitive.clear();
        // Keep inset cache — insets are a window property, not position-dependent.
    }

    /// Clear the cached border insets. Call when system theme or DWM metrics
    /// change (e.g., high contrast toggle) so that stale invisible-border
    /// values don't cause incorrect window sizing.
    pub fn clear_insets(&mut self) {
        self.insets.clear();
        self.inset_generation = INSET_CACHE_GENERATION.load(Ordering::Acquire);
    }

    /// Lazily observe a global inset invalidation. This is one atomic load per
    /// placement batch and avoids both stale cross-DPI metrics and a mutex/DWM
    /// query on every animation-frame window.
    fn sync_inset_generation(&mut self) {
        let current = INSET_CACHE_GENERATION.load(Ordering::Acquire);
        if self.inset_generation != current {
            self.insets.clear();
            // Position cache entries must also be cleared: an unchanged layout
            // rect still needs a SetWindowPos when its frame insets changed.
            self.positions.clear();
            self.inset_generation = current;
        }
    }
}

/// A window whose actual visible width exceeds the requested placement width,
/// indicating it enforces a minimum size. The `min_width` is in layout
/// pixels (matches what the layout engine would allocate).
#[derive(Debug, Clone)]
pub struct WidthViolation {
    pub window_id: WindowId,
    /// Minimum width in layout coordinates.
    pub min_width: i32,
    /// Whether the window honored the requested left edge. This distinguishes
    /// a real viewport-sized minimum from an app-owned fullscreen surface that
    /// ignored placement and stayed at the monitor origin.
    pub position_matches: bool,
    /// Requested visible left edge, compared with the work-area origin by the
    /// daemon so equality is accepted only when the comparison discriminates.
    pub requested_left: i32,
}

/// A window whose actual visible height exceeds the requested placement height.
/// Symmetric to `WidthViolation`. The `min_height` is in layout pixels.
#[derive(Debug, Clone)]
pub struct HeightViolation {
    pub window_id: WindowId,
    /// Minimum height in layout coordinates.
    pub min_height: i32,
    /// Whether the window honored the requested top edge.
    pub position_matches: bool,
    /// Requested visible top edge; height analogue of `requested_left`.
    pub requested_top: i32,
}

/// Result of apply_placements, including any detected size violations.
pub struct ApplyPlacementsResult {
    /// Width violations detected after positioning (windows wider than requested).
    pub width_violations: Vec<WidthViolation>,
    /// Height violations detected after positioning (windows taller than requested).
    pub height_violations: Vec<HeightViolation>,
    /// Visible windows whose DWM frame did not land on the requested
    /// left/top/right/bottom edges. The daemon performs one guarded corrective
    /// landing after stale insets have been invalidated.
    pub geometry_mismatches: Vec<WindowId>,
}

// Collect all (hwnd, adjusted_rect, flags) entries for deferred positioning.
// Pre-compute border insets and cache checks before the batch to minimize
// time between BeginDeferWindowPos and EndDeferWindowPos.
struct DeferEntry {
    hwnd: HWND,
    window_id: u64,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    /// Exact visible rectangle requested by the layout engine (pre-insets).
    /// Landing verification compares all four DWM edges against it.
    layout_rect: Rect,
    /// Insets used to translate `layout_rect` into the outer chrome rectangle.
    used_insets: (i32, i32, i32, i32),
    /// High-contrast deliberately bypasses regular invisible insets, so a
    /// fresh non-zero DWM inset must not be mistaken for stale cache state.
    validate_insets: bool,
    visibility: Visibility,
    flags: windows::Win32::UI::WindowsAndMessaging::SET_WINDOW_POS_FLAGS,
    column_index: usize,
    preview_source: bool,
}

/// Windows that returned to the viewport from a preview or from parking and are
/// still owed one compositor repair.
///
/// The evidence is consumed by whichever pass observes it, and during an animated
/// scroll that is an intermediate frame: the first interpolated rect that no
/// longer crosses the monitor edge drops the preview registration and the cloak,
/// so the exact landing would see nothing left to repair. Latching the identity
/// keeps the repair for the landing, which is the only pass that may resize.
static PENDING_RETURN_REPAIR: OnceLock<Mutex<HashSet<WindowId>>> = OnceLock::new();

fn pending_return_repair() -> &'static Mutex<HashSet<WindowId>> {
    PENDING_RETURN_REPAIR.get_or_init(|| Mutex::new(HashSet::new()))
}

fn latch_return_repair(window_id: WindowId) {
    pending_return_repair()
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
        .insert(window_id);
}

/// Claim only repair receipts owned by this exact landing. A disjoint batch
/// must leave another HWND's renderer-repair obligation intact.
fn take_return_repairs_for(window_ids: &HashSet<WindowId>) -> HashSet<WindowId> {
    let mut pending = pending_return_repair()
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex);
    let claimed: HashSet<WindowId> = pending
        .iter()
        .filter(|window_id| window_ids.contains(window_id))
        .copied()
        .collect();
    pending.retain(|window_id| !claimed.contains(window_id));
    claimed
}

/// Forget a latched repair for a window that will not be placed again.
pub(crate) fn forget_return_repair(window_id: WindowId) {
    pending_return_repair()
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
        .remove(&window_id);
}

/// Whether a placement brings a window back to the viewport after it was
/// represented by an edge preview or parked off every monitor.
///
/// That transition moves a window thousands of pixels in one step, and Chromium,
/// Electron and WinUI renderers routinely present one more frame at the old
/// transform: the frame, focus border and taskbar are correct while the painted
/// content sits beside them. A real size delta is the only thing that makes such
/// a renderer rebuild, which is what `nudge_sticky_compositor_windows` does.
///
/// The cached placement cannot carry this on its own: the exact landing pass runs
/// without a cache, so `previous` is `None` there. The durable signals are the
/// preview registration and the placement cloak, both of which describe the state
/// the window is leaving.
///
/// Pure so the rule is testable without Win32.
pub(crate) fn returns_from_offscreen_park(
    current_visibility: Visibility,
    had_preview: bool,
    was_cloaked: bool,
    was_directly_parked: bool,
    previous: Option<(Rect, Visibility)>,
) -> bool {
    if current_visibility != Visibility::Visible {
        return false;
    }
    had_preview
        || was_cloaked
        || was_directly_parked
        || previous.is_some_and(|(_, visibility)| visibility != Visibility::Visible)
}

fn invalidate_preview_surface_and_clear_best_effort(context: &str) {
    crate::thumbnail::invalidate_persistent_preview_surface();
    match crate::thumbnail::clear_persistent_previews_best_effort() {
        Ok(true) => {}
        Ok(false) => tracing::warn!("Preview cleanup deferred during {context}"),
        Err(error) => tracing::warn!("Preview cleanup degraded during {context}: {error}"),
    }
}

/// Apply window placements from the layout engine.
///
/// Visible windows are positioned immediately via SetWindowPos.
/// Off-screen windows are moved to sentinel coordinates far off-screen.
///
/// When `cache` is provided, placements whose rect and visibility match the
/// cached values are skipped, avoiding redundant Win32 calls during animations
/// where most windows haven't moved.
pub fn apply_placements(
    placements: &[WindowPlacement],
    config: &PlatformConfig,
    cache: Option<&mut PlacementCache>,
    nudge_sticky_compositors: bool,
) -> Result<ApplyPlacementsResult, Win32Error> {
    apply_placements_with_regions(placements, &[], config, cache, nudge_sticky_compositors)
}

pub fn apply_placements_with_regions(
    placements: &[WindowPlacement],
    region_clips: &[WindowRegionClip],
    config: &PlatformConfig,
    cache: Option<&mut PlacementCache>,
    nudge_sticky_compositors: bool,
) -> Result<ApplyPlacementsResult, Win32Error> {
    let expected_identities: HashMap<_, _> = placements
        .iter()
        .filter_map(|placement| {
            crate::current_window_event_identity(placement.window_id)
                .map(|identity| (placement.window_id, identity))
        })
        .collect();
    apply_placements_with_regions_fenced(
        placements,
        region_clips,
        &expected_identities,
        config,
        cache,
        nudge_sticky_compositors,
    )
}

pub fn apply_placements_with_regions_fenced(
    placements: &[WindowPlacement],
    region_clips: &[WindowRegionClip],
    expected_identities: &HashMap<WindowId, crate::WindowEventIdentity>,
    config: &PlatformConfig,
    mut cache: Option<&mut PlacementCache>,
    nudge_sticky_compositors: bool,
) -> Result<ApplyPlacementsResult, Win32Error> {
    let empty_result = ApplyPlacementsResult {
        width_violations: Vec::new(),
        height_violations: Vec::new(),
        geometry_mismatches: Vec::new(),
    };
    if placements.is_empty() {
        if let Some(cache) = cache {
            cache.clear();
        }
        invalidate_preview_surface_and_clear_best_effort("empty layout");
        // Empty layout is also a hard region-lifecycle boundary.
        restore_all_window_regions();
        // Uncloak all tracked windows — no placements means all previous
        // windows have left this layout (e.g., workspace switch to empty workspace).
        uncloak_all_tracked();
        return Ok(empty_result);
    }

    let stale_identities: Vec<_> = placements
        .iter()
        .filter(|placement| {
            expected_identities.get(&placement.window_id)
                != crate::current_window_event_identity(placement.window_id).as_ref()
        })
        .map(|placement| placement.window_id)
        .collect();
    if !stale_identities.is_empty() {
        return Err(Win32Error::SetPositionFailed(format!(
            "layout contains stale HWND incarnation(s): {stale_identities:?}"
        )));
    }

    if let Some(ref mut cache) = cache {
        cache.sync_inset_generation();
    }

    // Cache presence identifies an intermediate animation frame. The exact
    // landing pass has no cache and remains fully synchronous. Intermediate
    // dispatch is selected per HWND so ordinary windows keep the low-latency
    // async path while compositor-sensitive renderers cannot build a backlog.
    let animation_frame = cache.is_some();
    let managed_window_ids: HashSet<WindowId> = placements
        .iter()
        .map(|placement| placement.window_id)
        .collect();
    let _preview_transaction = lock_persistent_preview_transaction();

    // Prepare all window entries — visible and off-screen alike.
    // All windows get full position + size with border inset adjustment.
    // Off-screen windows are kept at their layout-flow position; DWM cloaking
    // makes them invisible.
    let offscreen_count = placements
        .iter()
        .filter(|p| p.visibility != Visibility::Visible)
        .count();

    // In high contrast mode, DWM paints a visible border in the normally-invisible
    // frame area.  If we expand by the usual insets, adjacent windows' visible borders
    // overlap and the layout gaps disappear.  Zero the insets to keep correct spacing.
    let high_contrast = crate::is_high_contrast_enabled();

    let DeferBuild {
        entries,
        preview_requests,
        desired_preview_requests,
        new_preview_count,
        skipped,
        safe_fallbacks,
        unsafe_hung_sensitive,
        unavailable_window_ids,
    } = build_defer_entries(
        placements,
        region_clips,
        &mut cache,
        animation_frame,
        config.animation_placement_policy,
        high_contrast,
    );

    if !unavailable_window_ids.is_empty() {
        return Err(Win32Error::SetPositionFailed(format!(
            "layout contains invalid window(s): {unavailable_window_ids:?}"
        )));
    }
    if !unsafe_hung_sensitive.is_empty() {
        return Err(Win32Error::SetPositionFailed(format!(
            "hung compositor-sensitive window(s) require exact landing: {unsafe_hung_sensitive:?}"
        )));
    }

    let (applied, mut failed_window_ids) = position_entries(&entries);
    failed_window_ids.extend(placements.iter().filter_map(|placement| {
        (expected_identities.get(&placement.window_id)
            != crate::current_window_event_identity(placement.window_id).as_ref())
        .then_some(placement.window_id)
    }));
    verify_preview_source_landings(&entries, &mut failed_window_ids);
    // A returning window stays cloaked until its visible landing succeeded; the
    // old ordering uncloaked first and could flash a stale offscreen/intermediate
    // position when SetWindowPos later failed. A failed uncloak retains its
    // receipt and rejects this landing rather than claiming it became visible.
    let visible_uncloak_failures = uncloak_becoming_visible(&entries, &failed_window_ids);
    failed_window_ids.extend(visible_uncloak_failures);

    let flush_needed = preview_commit_needs_flush(
        animation_frame,
        new_preview_count,
        preview_requests
            .iter()
            .filter(|request| !failed_window_ids.contains(&request.window_id))
            .count(),
    );
    if flush_needed && unsafe { DwmFlush() }.is_err() {
        failed_window_ids.extend(preview_requests.iter().map(|request| request.window_id));
    }

    // Keep every visibility protection until the source was both physically
    // verified at its park rect and compositor-committed there. Only then may
    // its cloak/owned region be released for DWM thumbnail capture.
    let mut committed_preview_requests: Vec<_> = preview_requests
        .iter()
        .copied()
        .filter(|request| !failed_window_ids.contains(&request.window_id))
        .collect();
    failed_window_ids.extend(uncloak_preview_sources(&committed_preview_requests));
    for clip in region_clips {
        if !failed_window_ids.contains(&clip.window_id)
            && !restore_window_region(clip.window_id, true)
        {
            failed_window_ids.insert(clip.window_id);
        }
    }
    committed_preview_requests.retain(|request| !failed_window_ids.contains(&request.window_id));
    // Region/cloak release changes the source surface DWM captures. Commit that
    // release too; otherwise the first thumbnail can sample the protected frame.
    if !committed_preview_requests.is_empty() && unsafe { DwmFlush() }.is_err() {
        failed_window_ids.extend(
            committed_preview_requests
                .iter()
                .map(|request| request.window_id),
        );
        committed_preview_requests.clear();
    }
    let active_preview_count = commit_persistent_previews(
        &committed_preview_requests,
        !animation_frame || new_preview_count > 0,
        config.preview_lifecycle_epoch,
        config.preview_host_below,
        // Animation frames may not arm hit testing: the DWM surface and the
        // input HWND cannot be moved atomically, so a click could land on a
        // target the next frame invalidates.
        !animation_frame,
    )?;

    // If the source move failed, retain an older LeopardWM region rather than
    // exposing the full foreign HWND. No new SetWindowRgn state is created by
    // this backend.
    let preserved_region_ids: HashSet<WindowId> = region_clips
        .iter()
        .filter(|clip| failed_window_ids.contains(&clip.window_id))
        .map(|clip| clip.window_id)
        .collect();
    reconcile_window_regions(&managed_window_ids, &preserved_region_ids, !animation_frame);

    // Detect size violations by comparing the DWM extended frame bounds
    // (the window's actual visible content area) against the layout rect the
    // layout engine asked for. This deliberately bypasses the cached-inset
    // math used for SetWindowPos: if the cached insets go stale (e.g. apps
    // like Slack/Spotify toggle custom client frames at runtime) the frame-
    // vs-frame comparison silently cancels out and violations are missed.
    //
    // Visible-bounds-vs-layout-rect is the honest comparison: the layout
    // engine allocates `placement.rect.width × placement.rect.height` of
    // visible real estate, and we check whether the window actually fits.
    //
    // Skipped during async animation frames — DWM returns stale (pre-resize)
    // bounds which would create false constraints that prevent columns from
    // shrinking. The synchronous landing pass detects real violations
    // authoritatively.
    let (width_violations, height_violations, geometry_mismatches) = if !animation_frame {
        detect_size_violations(&entries, &failed_window_ids, &mut cache)
    } else {
        (Vec::new(), Vec::new(), Vec::new())
    }; // end: skip landing verification during async frames

    // Cloak off-screen windows AFTER positioning. DWM cloaking keeps the
    // composition surface alive (preventing content shift on return) while
    // hiding the window from view (preventing peeking through outer gaps).
    // A rejected foreign-HWND cloak falls back to a verified sentinel park and
    // is still reported as a failed placement; never cache API acceptance as a
    // hidden-surface receipt.
    sync_cloak_state(
        &entries,
        placements,
        &mut failed_window_ids,
        &committed_preview_requests,
    );
    let retained_preview_desire: Vec<_> = desired_preview_requests
        .into_iter()
        .filter(|request| !failed_window_ids.contains(&request.window_id))
        .collect();
    retain_persistent_preview_desire(&retained_preview_desire, config.preview_lifecycle_epoch);

    // Update cache only after every visibility side effect committed. In
    // particular, a foreign HWND whose cloak was denied must retry rather than
    // making a later animation frame skip a false hidden state.
    if let Some(cache) = cache {
        let current_ids: std::collections::HashSet<u64> =
            placements.iter().map(|p| p.window_id).collect();
        cache.positions.retain(|id, _| current_ids.contains(id));
        cache.insets.retain(|id, _| current_ids.contains(id));
        cache
            .compositor_sensitive
            .retain(|id, _| current_ids.contains(id));
        for entry in &entries {
            if !failed_window_ids.contains(&entry.window_id) {
                cache
                    .positions
                    .insert(entry.window_id, (entry.layout_rect, entry.visibility));
            }
        }
    }

    // DirectComposition swap-chain repair.
    //
    // On the synchronous landing pass, nudge windows whose compositor rebuilds
    // its swap chain only on observed size deltas. During rapid scroll the
    // intermediate async frames coalesce on the app's UI thread, leaving the
    // internal render target stuck at an interim size; the landing SetWindowPos
    // arrives with the same rect as the last async frame, so the compositor
    // sees "no size change" and never rebuilds. A brief (w-1 -> w) resize pair
    // forces a real delta through. Scoped to known-affected classes to avoid a
    // universal flicker tax.
    // Two reasons to repair on an exact landing: the legacy post-animation case,
    // and a window that just returned from an edge preview or off-monitor
    // parking. The latter is independent of `compositor_safe_mode`, because safe
    // mode only serialises *animation* frames — it does nothing for a single
    // multi-thousand-pixel jump back into the viewport.
    if !animation_frame {
        // Claim only receipts this exact landing can actually process. Entries
        // rejected earlier in the batch retain their receipt for a later retry.
        let repairable_ids: HashSet<WindowId> = entries
            .iter()
            .filter(|entry| {
                entry.visibility == Visibility::Visible
                    && entry.w > 1
                    && !failed_window_ids.contains(&entry.window_id)
            })
            .map(|entry| entry.window_id)
            .collect();
        let returned_from_park = take_return_repairs_for(&repairable_ids);
        let nudge_targets: Vec<NudgeTarget> = entries
            .iter()
            .filter(|e| {
                repairable_ids.contains(&e.window_id)
                    && (nudge_sticky_compositors || returned_from_park.contains(&e.window_id))
            })
            .map(|e| NudgeTarget {
                window_id: e.window_id,
                hwnd: e.hwnd,
                x: e.x,
                y: e.y,
                w: e.w,
                h: e.h,
            })
            .collect();
        for window_id in nudge_sticky_compositor_windows(&nudge_targets) {
            latch_return_repair(window_id);
            failed_window_ids.insert(window_id);
        }

        // A MoveOffScreen marker is a crash-surviving ownership receipt.
        // Release it only after visible-edge readback and any required renderer
        // nudge both committed. API acceptance is not physical completion.
        let mismatched: HashSet<WindowId> = geometry_mismatches.iter().copied().collect();
        for entry in entries.iter().filter(|entry| {
            entry.visibility == Visibility::Visible
                && !failed_window_ids.contains(&entry.window_id)
                && !mismatched.contains(&entry.window_id)
        }) {
            crate::visibility::clear_move_offscreen_marker(entry.window_id);
        }
    }

    tracing::debug!(
        "Applied {} placements ({} skipped unchanged), {} DWM preview(s), {} safe fallback(s), {} off-screen total",
        applied,
        skipped,
        active_preview_count,
        safe_fallbacks,
        offscreen_count,
    );

    if !failed_window_ids.is_empty() {
        let mut failed: Vec<_> = failed_window_ids.into_iter().collect();
        failed.sort_unstable();
        return Err(Win32Error::SetPositionFailed(format!(
            "layout did not safely commit window(s): {failed:?}"
        )));
    }

    Ok(ApplyPlacementsResult {
        width_violations,
        height_violations,
        geometry_mismatches,
    })
}

/// Resolve the top-left position used for an off-screen placement.
///
/// Inactive tabs intentionally use a zero-area layout marker. Because their
/// `SWP_NOSIZE` application retains the real HWND dimensions, parking such a
/// marker merely one viewport left can still leave a wide window visible.
/// Send those markers to the global sentinel while preserving ordinary strip
/// off-screen positions for scrolling columns.
fn offscreen_position(placement: &WindowPlacement, inset_l: i32, inset_t: i32) -> (i32, i32) {
    if placement.rect.width == 0 && placement.rect.height == 0 {
        let sentinel = crate::MOVE_OFFSCREEN_SENTINEL_COORD;
        (sentinel, sentinel)
    } else {
        (
            placement.rect.x.saturating_sub(inset_l),
            placement.rect.y.saturating_sub(inset_t),
        )
    }
}

/// Whether an animation frame can move a visible window without re-sending
/// its unchanged size. Avoiding redundant size messages keeps a horizontal
/// animation out of the target application's swap-chain resize path.
fn animation_move_is_position_only(
    previous: Option<(Rect, Visibility)>,
    current: &WindowPlacement,
) -> bool {
    previous.is_some_and(|(rect, visibility)| {
        visibility == Visibility::Visible
            && current.visibility == Visibility::Visible
            && rect.width == current.rect.width
            && rect.height == current.rect.height
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnimationDispatchMode {
    Synchronous,
    Asynchronous,
    SkipHungSensitive,
}

/// Select per-HWND dispatch without consulting Win32, keeping policy testable.
fn animation_dispatch_mode(
    policy: AnimationPlacementPolicy,
    compositor_sensitive: bool,
    target_is_hung: bool,
) -> AnimationDispatchMode {
    match (policy, compositor_sensitive, target_is_hung) {
        (AnimationPlacementPolicy::AdaptiveCompositorSafe, true, true) => {
            AnimationDispatchMode::SkipHungSensitive
        }
        (AnimationPlacementPolicy::AdaptiveCompositorSafe, true, false) => {
            AnimationDispatchMode::Synchronous
        }
        _ => AnimationDispatchMode::Asynchronous,
    }
}

fn visible_position_flags(
    animation_frame: bool,
    dispatch: AnimationDispatchMode,
    position_only: bool,
) -> SET_WINDOW_POS_FLAGS {
    let mut flags = SWP_NOZORDER | SWP_NOACTIVATE;
    if animation_frame && dispatch == AnimationDispatchMode::Asynchronous {
        flags |= SWP_ASYNCWINDOWPOS;
    }
    // WM_NCCALCSIZE is unnecessary on intermediate movement frames and is a
    // major source of renderer churn. The exact landing always recalculates it.
    if !animation_frame {
        flags |= SWP_FRAMECHANGED;
    }
    if position_only {
        flags |= SWP_NOSIZE;
    }
    flags
}

fn cached_compositor_sensitive(
    hwnd: HWND,
    window_id: WindowId,
    cache: Option<&mut PlacementCache>,
) -> bool {
    if let Some(value) = cache
        .as_ref()
        .and_then(|cache| cache.compositor_sensitive.get(&window_id).copied())
    {
        return value;
    }

    let class = window_class_name(hwnd);
    // A valid HWND with a temporarily unreadable class is treated as sensitive:
    // the safe failure mode is one synchronous movement frame, not an async
    // burst whose renderer behavior we cannot classify.
    let value = class.is_empty() || crate::thumbnail::is_compositor_sensitive_class_str(&class);
    if let Some(cache) = cache {
        cache.compositor_sensitive.insert(window_id, value);
    }
    value
}

fn preview_commit_needs_flush(
    animation_frame: bool,
    new_preview_count: usize,
    committed_preview_count: usize,
) -> bool {
    // existing animation frames do not block on DwmFlush; only activation and
    // the exact landing synchronize with the compositor.
    new_preview_count > 0 || (!animation_frame && committed_preview_count > 0)
}

fn persistent_preview_request(
    window_id: WindowId,
    target_outer: Rect,
    clip_bounds: Rect,
) -> Option<PersistentPreviewRequest> {
    let left = target_outer.x.max(clip_bounds.x);
    let top = target_outer.y.max(clip_bounds.y);
    let right = target_outer.right().min(clip_bounds.right());
    let bottom = target_outer.bottom().min(clip_bounds.bottom());
    if right <= left || bottom <= top {
        return None;
    }
    let source = Rect::new(
        left.saturating_sub(target_outer.x),
        top.saturating_sub(target_outer.y),
        right - left,
        bottom - top,
    );
    let destination = Rect::new(left, top, right - left, bottom - top);
    Some(PersistentPreviewRequest {
        window_id,
        source_rect: source,
        expected_source_size: (target_outer.width.max(1), target_outer.height.max(1)),
        destination_screen_rect: destination,
    })
}

struct DeferBuild {
    entries: Vec<DeferEntry>,
    /// Handles already registered and safe to unprotect/publish in this pass.
    preview_requests: Vec<PersistentPreviewRequest>,
    /// Full verified parked intent, including sources whose DWM registration
    /// failed transiently and must trigger a future exact relayout.
    desired_preview_requests: Vec<PersistentPreviewRequest>,
    new_preview_count: usize,
    skipped: u32,
    safe_fallbacks: u32,
    unsafe_hung_sensitive: Vec<WindowId>,
    unavailable_window_ids: Vec<WindowId>,
}

/// Build the defer-entry list for all placements, skipping cache-unchanged windows.
fn build_defer_entries(
    placements: &[WindowPlacement],
    region_clips: &[WindowRegionClip],
    cache: &mut Option<&mut PlacementCache>,
    animation_frame: bool,
    policy: AnimationPlacementPolicy,
    high_contrast: bool,
) -> DeferBuild {
    let mut skipped = 0u32;
    let mut safe_fallbacks = 0u32;
    let mut entries: Vec<DeferEntry> = Vec::with_capacity(placements.len());
    let mut preview_requests = Vec::with_capacity(region_clips.len());
    let mut desired_preview_requests = Vec::with_capacity(region_clips.len());
    let mut new_preview_count = 0usize;
    let mut unsafe_hung_sensitive = Vec::new();
    let mut unavailable_window_ids = Vec::new();
    let mut validated_hwnds = HashMap::with_capacity(placements.len());
    // Complete a side-effect-free validity pass for the whole batch before
    // restoring, registering previews, or latching renderer repair on any
    // member. A pre-existing dead sibling cannot partially mutate live HWNDs.
    for requested in placements {
        match window_id_to_hwnd(requested.window_id) {
            Ok(hwnd) if unsafe { IsWindow(Some(hwnd)).as_bool() } => {
                validated_hwnds.insert(requested.window_id, hwnd);
            }
            _ => unavailable_window_ids.push(requested.window_id),
        }
    }
    if !unavailable_window_ids.is_empty() {
        return DeferBuild {
            entries,
            preview_requests,
            desired_preview_requests,
            new_preview_count,
            skipped,
            safe_fallbacks,
            unsafe_hung_sensitive,
            unavailable_window_ids,
        };
    }

    for requested in placements {
        let region_clip = region_clips
            .iter()
            .find(|clip| clip.window_id == requested.window_id);
        let hwnd = validated_hwnds[&requested.window_id];
        unsafe {
            if IsIconic(hwnd).as_bool() {
                continue;
            }
            if requested.visibility == Visibility::Visible
                && requested.column_index != usize::MAX
                && IsZoomed(hwnd).as_bool()
            {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
        }

        let (inset_l, inset_t, inset_r, inset_b) = if high_contrast {
            (0, 0, 0, 0)
        } else {
            cached_border_insets(hwnd, requested.window_id, cache.as_deref_mut())
        };
        let target_frame_w = requested.rect.width + inset_l + inset_r;
        let target_frame_h = requested.rect.height + inset_t + inset_b;
        let target_outer = Rect::new(
            requested.rect.x - inset_l,
            requested.rect.y - inset_t,
            target_frame_w.max(1),
            target_frame_h.max(1),
        );

        // Captured before this pass mutates preview or cloak state: they are the
        // durable evidence that the window is returning from a parked preview.
        let had_preview = has_persistent_preview(requested.window_id);
        let was_cloaked = is_placement_cloaked(requested.window_id);
        let was_directly_parked =
            crate::visibility::has_move_offscreen_ownership(requested.window_id);
        let mut preview_source = false;
        let mut preview_was_published = false;
        let mut placement = requested.clone();
        let mut preview_request = None;
        if let Some(clip) = region_clip {
            placement.rect = clip.fallback_rect;
            placement.visibility = clip.fallback_visibility;
            if clip.fallback_visibility != Visibility::Visible
                && !ghost_cloaked_contains(requested.window_id)
            {
                if let Some(request) =
                    persistent_preview_request(requested.window_id, target_outer, clip.clip_bounds)
                {
                    desired_preview_requests.push(request);
                    let preview_existed = has_persistent_preview(requested.window_id);
                    preview_was_published = has_published_persistent_preview(requested.window_id);
                    if prepare_persistent_preview(requested.window_id) {
                        preview_request = Some(request);
                        preview_source = true;
                        if !preview_existed {
                            new_preview_count += 1;
                        }
                    } else {
                        // Keep the source in its verified safe fallback. The
                        // desired request survives so registration recovery can
                        // request a fresh exact pass; it is not yet eligible for
                        // cloak/region release or DWM publication.
                        safe_fallbacks += 1;
                    }
                } else {
                    safe_fallbacks += 1;
                }
            }
        }

        let previous = cache
            .as_ref()
            .and_then(|cache| cache.positions.get(&placement.window_id).copied());
        let unchanged = previous == Some((placement.rect, placement.visibility));
        if unchanged
            && !was_directly_parked
            && region_clip.is_none()
            && !has_owned_window_region(placement.window_id)
        {
            skipped += 1;
            continue;
        }
        let position_only = animation_move_is_position_only(previous, &placement);
        let managed_transition =
            region_clip.is_some() || has_owned_window_region(placement.window_id) || preview_source;
        let dispatch = if animation_frame {
            let sensitive = managed_transition
                || (policy == AnimationPlacementPolicy::AdaptiveCompositorSafe
                    && cached_compositor_sensitive(
                        hwnd,
                        placement.window_id,
                        cache.as_deref_mut(),
                    ));
            let hung = sensitive && unsafe { IsHungAppWindow(hwnd).as_bool() };
            if managed_transition && !hung {
                AnimationDispatchMode::Synchronous
            } else {
                animation_dispatch_mode(policy, sensitive, hung)
            }
        } else {
            AnimationDispatchMode::Synchronous
        };
        if dispatch == AnimationDispatchMode::SkipHungSensitive {
            // A skipped request may survive only when the worker cache proves
            // this exact fallback is already applied and DWM has successfully
            // published it before. A new registration is not a park receipt.
            // Nor is SWP_ASYNCWINDOWPOS: success only means the request was queued
            // to the hung owner, and foreign HWND cloaking commonly fails. Force
            // the frame to fail so the daemon takes its bounded exact-landing
            // path instead of caching false physical success.
            let already_safely_parked =
                preview_was_published && previous == Some((placement.rect, placement.visibility));
            if already_safely_parked {
                skipped += 1;
                if let Some(request) = preview_request {
                    preview_requests.push(request);
                }
                continue;
            }
            safe_fallbacks += 1;
            unsafe_hung_sensitive.push(placement.window_id);
            continue;
        }

        if let Some(request) = preview_request {
            preview_requests.push(request);
        }

        if preview_source {
            let flags = SWP_NOZORDER | SWP_NOACTIVATE;
            entries.push(DeferEntry {
                hwnd,
                window_id: placement.window_id,
                x: placement.rect.x.saturating_sub(inset_l),
                y: placement.rect.y.saturating_sub(inset_t),
                w: target_frame_w.max(1),
                h: target_frame_h.max(1),
                layout_rect: placement.rect,
                used_insets: (inset_l, inset_t, inset_r, inset_b),
                validate_insets: !high_contrast,
                visibility: placement.visibility,
                flags,
                column_index: placement.column_index,
                preview_source: true,
            });
        } else if placement.visibility == Visibility::Visible {
            let frame_w = placement.rect.width + inset_l + inset_r;
            let frame_h = placement.rect.height + inset_t + inset_b;
            let flags = visible_position_flags(animation_frame, dispatch, position_only);
            // Read the state the window is leaving *before* this pass changes it.
            if returns_from_offscreen_park(
                placement.visibility,
                had_preview,
                was_cloaked,
                was_directly_parked,
                previous,
            ) {
                // Latched rather than acted on here: only the exact landing may
                // resize, and an animation frame would otherwise consume the
                // evidence before the landing sees it.
                latch_return_repair(placement.window_id);
            }
            entries.push(DeferEntry {
                hwnd,
                window_id: placement.window_id,
                x: placement.rect.x - inset_l,
                y: placement.rect.y - inset_t,
                w: frame_w,
                h: frame_h,
                layout_rect: placement.rect,
                used_insets: (inset_l, inset_t, inset_r, inset_b),
                validate_insets: !high_contrast,
                visibility: placement.visibility,
                flags,
                column_index: placement.column_index,
                preview_source: false,
            });
        } else {
            let frame_w = placement.rect.width + inset_l + inset_r;
            let (x, y) = offscreen_position(&placement, inset_l, inset_t);
            let mut flags = SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE;
            if animation_frame && dispatch == AnimationDispatchMode::Asynchronous {
                flags |= SWP_ASYNCWINDOWPOS;
            }
            entries.push(DeferEntry {
                hwnd,
                window_id: placement.window_id,
                x,
                y,
                w: frame_w,
                h: 0,
                layout_rect: placement.rect,
                used_insets: (inset_l, inset_t, inset_r, inset_b),
                validate_insets: !high_contrast,
                visibility: placement.visibility,
                flags,
                column_index: placement.column_index,
                preview_source: false,
            });
        }
    }

    DeferBuild {
        entries,
        preview_requests,
        desired_preview_requests,
        new_preview_count,
        skipped,
        safe_fallbacks,
        unsafe_hung_sensitive,
        unavailable_window_ids,
    }
}

/// Install a bridge before uncloaking or moving. Unsupported application-owned
/// regions use the existing safe whole-window fallback before presentation.
fn uncloak_preview_sources(requests: &[PersistentPreviewRequest]) -> HashSet<WindowId> {
    if requests.is_empty() {
        return HashSet::new();
    }
    let _commit = lock_cloak_commit();
    requests
        .iter()
        .filter_map(|request| {
            global_cloaked_contains(request.window_id)
                .then_some(request.window_id)
                .filter(|window_id| !commit_global_cloak_state_locked(*window_id, false))
        })
        .collect()
}

fn should_cloak_entry(
    entry: &DeferEntry,
    placement_exists: bool,
    positioning_failed: bool,
) -> bool {
    placement_exists
        && !positioning_failed
        && entry.visibility != Visibility::Visible
        && !entry.preview_source
}

/// Uncloak entries becoming visible and drop them from the tracking set.
fn uncloak_becoming_visible(
    entries: &[DeferEntry],
    failed_window_ids: &HashSet<WindowId>,
) -> HashSet<WindowId> {
    let _commit = lock_cloak_commit();
    entries
        .iter()
        .filter(|entry| {
            entry.visibility == Visibility::Visible
                && !failed_window_ids.contains(&entry.window_id)
                && global_cloaked_contains(entry.window_id)
        })
        .filter_map(|entry| {
            (!commit_global_cloak_state_locked(entry.window_id, false)).then_some(entry.window_id)
        })
        .collect()
}

/// Position all entries in one DeferWindowPos batch; returns (applied, failed ids).
fn position_entries(entries: &[DeferEntry]) -> (u32, HashSet<u64>) {
    let mut applied = 0u32;

    // Track windows that failed positioning (excluded from cache).
    let mut failed_window_ids: std::collections::HashSet<u64> = std::collections::HashSet::new();

    // Batch all SetWindowPos calls via DeferWindowPos for atomic repositioning.
    if !entries.is_empty() {
        unsafe {
            match BeginDeferWindowPos(entries.len() as i32) {
                Err(_) => {
                    // Fallback: apply individually if batching fails
                    for entry in entries {
                        if SetWindowPos(
                            entry.hwnd,
                            None,
                            entry.x,
                            entry.y,
                            entry.w,
                            entry.h,
                            entry.flags,
                        )
                        .is_err()
                        {
                            failed_window_ids.insert(entry.window_id);
                        }
                    }
                    applied = (entries.len() - failed_window_ids.len()) as u32;
                }
                Ok(initial_hdwp) => {
                    let mut hdwp = initial_hdwp;
                    let mut batch_ok = true;
                    for entry in entries {
                        match DeferWindowPos(
                            hdwp,
                            entry.hwnd,
                            None,
                            entry.x,
                            entry.y,
                            entry.w,
                            entry.h,
                            entry.flags,
                        ) {
                            Ok(new_hdwp) => hdwp = new_hdwp,
                            Err(_) => {
                                batch_ok = false;
                                break;
                            }
                        }
                    }
                    if batch_ok {
                        if EndDeferWindowPos(hdwp).is_err() {
                            // EndDeferWindowPos failed — fall back to individual calls
                            for entry in entries {
                                if SetWindowPos(
                                    entry.hwnd,
                                    None,
                                    entry.x,
                                    entry.y,
                                    entry.w,
                                    entry.h,
                                    entry.flags,
                                )
                                .is_err()
                                {
                                    failed_window_ids.insert(entry.window_id);
                                }
                            }
                            applied = (entries.len() - failed_window_ids.len()) as u32;
                        } else {
                            applied = entries.len() as u32;
                        }
                    } else {
                        // DeferWindowPos failed — HDWP is already freed by Win32.
                        // Fall back to individual SetWindowPos calls.
                        for entry in entries {
                            if SetWindowPos(
                                entry.hwnd,
                                None,
                                entry.x,
                                entry.y,
                                entry.w,
                                entry.h,
                                entry.flags,
                            )
                            .is_err()
                            {
                                failed_window_ids.insert(entry.window_id);
                            }
                        }
                        applied = (entries.len() - failed_window_ids.len()) as u32;
                    }
                }
            }
        }
    }

    (applied, failed_window_ids)
}

/// Confirm that a preview source actually honored the off-monitor park request.
/// `SetWindowPos` success only means Windows accepted the request; applications
/// can synchronously constrain or undo it. Publishing before this check exposes
/// both the real crossing HWND and its thumbnail.
fn verify_preview_source_landings(
    entries: &[DeferEntry],
    failed_window_ids: &mut HashSet<WindowId>,
) {
    for entry in entries {
        if !entry.preview_source || failed_window_ids.contains(&entry.window_id) {
            continue;
        }
        let mut actual = RECT::default();
        let landed = unsafe { GetWindowRect(entry.hwnd, &mut actual) }.is_ok()
            && (actual.left - entry.x).abs() <= EDGE_EPSILON_PX
            && (actual.top - entry.y).abs() <= EDGE_EPSILON_PX
            && (actual.right - entry.x.saturating_add(entry.w)).abs() <= EDGE_EPSILON_PX
            && (actual.bottom - entry.y.saturating_add(entry.h)).abs() <= EDGE_EPSILON_PX;
        if landed {
            continue;
        }

        // An application-enforced minimum size can accept top/left while its
        // right/bottom still reaches a monitor. Preserve the actual size and use
        // the global sentinel as a final physical fallback; unlike above/left
        // adjacency this remains clear even when the frame is larger than asked.
        let emergency_flags = SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE;
        let emergency_moved = unsafe {
            SetWindowPos(
                entry.hwnd,
                None,
                crate::MOVE_OFFSCREEN_SENTINEL_COORD,
                crate::MOVE_OFFSCREEN_SENTINEL_COORD,
                0,
                0,
                emergency_flags,
            )
        }
        .is_ok();
        let mut emergency = RECT::default();
        let emergency_rect_available =
            emergency_moved && unsafe { GetWindowRect(entry.hwnd, &mut emergency) }.is_ok();
        let emergency_rect = Rect::new(
            emergency.left,
            emergency.top,
            emergency.right.saturating_sub(emergency.left),
            emergency.bottom.saturating_sub(emergency.top),
        );
        // Win32 clamps very large negative coordinates to its signed virtual
        // coordinate floor (commonly -32768), so equality with the requested
        // -100000 sentinel is not a valid receipt. The safety property is the
        // complete actual rectangle clearing every current monitor.
        let emergency_landed = emergency_rect_available
            && emergency_rect.width > 0
            && emergency_rect.height > 0
            && crate::enumerate_monitors().is_ok_and(|monitors| {
                monitors
                    .iter()
                    .all(|monitor| !emergency_rect.intersects(&monitor.rect))
            });
        if !emergency_landed {
            failed_window_ids.insert(entry.window_id);
        }
    }
}

/// Per-window suspect state for the size-violation two-pass confirmation:
/// `(width_suspect, height_suspect)` — whether that axis's oversize looked stale
/// (beyond the stale-bounds ratio) on the window's previous landing pass. A
/// genuine min-size reproduces and is promoted on the second sighting; a one-off
/// stale DWM read does not reproduce and is dropped. Module-global because
/// `detect_size_violations` is a free function called once per settle across all
/// workspaces, so a window is only re-measured when its workspace next lands.
/// Entries are evicted on window destroy (`clear_suspected_oversize`) so the map
/// stays bounded and a recycled HWND never inherits a stale suspect bit.
static SUSPECTED_OVERSIZE: Mutex<Option<HashMap<u64, (bool, bool)>>> = Mutex::new(None);

fn lock_suspected_oversize() -> std::sync::MutexGuard<'static, Option<HashMap<u64, (bool, bool)>>> {
    SUSPECTED_OVERSIZE
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
}

/// Drop a window's suspect state. Called when a window is destroyed/unmanaged so
/// the set stays bounded and a recycled HWND starts fresh.
pub fn clear_suspected_oversize(window_id: WindowId) {
    forget_persistent_preview(window_id);
    forget_return_repair(window_id);
    crate::visibility::clear_move_offscreen_marker(window_id);
    crate::window_region::forget_window_region(window_id);
    let mut guard = lock_suspected_oversize();
    if let Some(map) = guard.as_mut() {
        map.remove(&window_id);
    }
}

/// Decide how to treat one axis's oversize measurement on the landing pass.
///
/// `over` = visibly larger than its slot; `looks_stale` = the excess exceeds the
/// stale-bounds ratio (a lagging app, but also a genuinely large min-size);
/// `was_suspected` = this axis was already suspect on the window's previous
/// landing pass; `absurd` = the excess is implausibly large for a real min-size.
/// Returns `(record_violation, suspect_now)`.
///
/// A small genuine excess is recorded immediately. A moderate suspicious excess
/// is recorded only once it reproduces across two landing passes — a true stale
/// read resolves by the next pass and is dropped, while a genuine large min-size
/// (e.g. a restored column saved narrower than the window's minimum) reproduces
/// and is honored, so the column stops re-resizing on every switch. An absurd
/// excess is never trusted, even if it reproduces, so a chronically-lagging app
/// can't inflate the layout (the case the original ratio guard protected).
fn classify_oversize(
    over: bool,
    looks_stale: bool,
    was_suspected: bool,
    absurd: bool,
) -> (bool, bool) {
    if !over {
        (false, false)
    } else if !looks_stale {
        (true, false)
    } else if absurd {
        (false, false)
    } else if was_suspected {
        (true, false)
    } else {
        (false, true)
    }
}

/// Tolerance used when comparing requested and compositor-reported frame edges.
pub const EDGE_EPSILON_PX: i32 = 2;

/// Return `(position_mismatch, any_edge_mismatch, undersized)` for a visible
/// DWM frame against the layout's requested visible rectangle.
fn geometry_mismatch_flags(visible: Rect, requested: Rect) -> (bool, bool, bool) {
    let position_mismatch = (visible.x - requested.x).abs() > EDGE_EPSILON_PX
        || (visible.y - requested.y).abs() > EDGE_EPSILON_PX;
    let right_mismatch = (visible.right() - requested.right()).abs() > EDGE_EPSILON_PX;
    let bottom_mismatch = (visible.bottom() - requested.bottom()).abs() > EDGE_EPSILON_PX;
    let undersized = visible.width + EDGE_EPSILON_PX < requested.width
        || visible.height + EDGE_EPSILON_PX < requested.height;
    (
        position_mismatch,
        position_mismatch || right_mismatch || bottom_mismatch,
        undersized,
    )
}

fn should_verify_visible_landing(visibility: Visibility, _column_index: usize) -> bool {
    visibility == Visibility::Visible
}

/// Verify all four visible DWM edges and detect genuine min-size violations
/// on the synchronous landing pass.
fn detect_size_violations(
    entries: &[DeferEntry],
    failed_window_ids: &HashSet<u64>,
    cache: &mut Option<&mut PlacementCache>,
) -> (Vec<WidthViolation>, Vec<HeightViolation>, Vec<WindowId>) {
    let mut width_violations = Vec::new();
    let mut height_violations = Vec::new();
    let mut geometry_mismatches = Vec::new();
    // Wait for the compositor to composite a frame before reading DWM
    // bounds. Sync SetWindowPos only guarantees the target thread received
    // WM_WINDOWPOSCHANGED — it does NOT wait for the target to process and
    // re-render. Under CPU pressure (e.g. a background `cargo test` build),
    // the target thread can lag behind: we'd read PRE-shrink bounds,
    // interpret the oversized rect as a min-size violation, and record a
    // bogus constraint that breaks subsequent layouts (e.g. a 50/50 column
    // turning into 75/50 because one window's min_height got inflated).
    //
    // DwmFlush blocks for ~one vsync (~16ms) until the compositor has
    // presented a frame incorporating our just-applied positions. Cheap
    // on the landing pass (runs once per settle, not per frame).
    unsafe {
        let _ = DwmFlush();
    }
    for entry in entries {
        if !should_verify_visible_landing(entry.visibility, entry.column_index)
            || failed_window_ids.contains(&entry.window_id)
        {
            continue;
        }

        // Query the exact visible bounds. Comparing all four edges catches
        // stale left/top insets and under-sized frames that width/height-only
        // validation missed (visible symptom: one side clipped and blank
        // desktop on the opposite side).
        let visible_rect = unsafe {
            let mut ext = RECT::default();
            if DwmGetWindowAttribute(
                entry.hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                &mut ext as *mut RECT as *mut _,
                std::mem::size_of::<RECT>() as u32,
            )
            .is_err()
            {
                geometry_mismatches.push(entry.window_id);
                continue;
            }
            Rect::new(
                ext.left,
                ext.top,
                ext.right - ext.left,
                ext.bottom - ext.top,
            )
        };
        let visible_w = visible_rect.width;
        let visible_h = visible_rect.height;
        let requested = entry.layout_rect;
        let (position_mismatch, edge_mismatch, undersized) =
            geometry_mismatch_flags(visible_rect, requested);

        // An inset can change during a window's lifetime (custom chrome, theme,
        // or a cross-monitor DPI transition). If the fresh chrome-vs-DWM inset
        // differs from what this SetWindowPos used, retry with the fresh metric
        // before recording any min-size constraint; otherwise one stale frame
        // permanently inflates the column/stack.
        if edge_mismatch && entry.validate_insets {
            let fresh_insets = invisible_border_insets(entry.hwnd);
            if fresh_insets != entry.used_insets {
                tracing::debug!(
                    "Stale frame insets for {:?}: used {:?}, fresh {:?}, requested {:?}, visible {:?}",
                    entry.hwnd,
                    entry.used_insets,
                    fresh_insets,
                    requested,
                    visible_rect,
                );
                invalidate_window_insets(entry.window_id, cache);
                if let Some(map) = lock_suspected_oversize().as_mut() {
                    map.remove(&entry.window_id);
                }
                geometry_mismatches.push(entry.window_id);
                continue;
            }
        }

        // Floating geometry is user-visible state too, but it must never seed
        // tiled min-size constraints. Any missed edge is a placement mismatch
        // that the daemon may retry once and then roll back transactionally.
        if entry.column_index == usize::MAX {
            // A floating app may enforce a larger minimum size; unlike a tiled
            // column there is no constraint model to update, and a larger float
            // whose requested top-left landed is still honestly visible. A
            // displaced or undersized float is not acceptable.
            if position_mismatch || undersized {
                tracing::debug!(
                    "Floating geometry mismatch: hwnd={:?} requested={:?} visible={:?}",
                    entry.hwnd,
                    requested,
                    visible_rect
                );
                geometry_mismatches.push(entry.window_id);
            } else if edge_mismatch {
                tracing::debug!(
                    "Floating window enforced a larger size: hwnd={:?} requested={:?} visible={:?}",
                    entry.hwnd,
                    requested,
                    visible_rect
                );
            }
            continue;
        }

        // Stale-bounds ratio — a genuine min-size violation usually has the
        // window just barely larger than requested. If DWM reports bounds >1.5x
        // the requested size, the target thread may be lagging behind our
        // just-applied resize under CPU pressure (despite the DwmFlush above this
        // can still happen for unresponsive apps), and recording it would inflate
        // future layouts. But a genuinely narrow column (e.g. a restored
        // workspace saved narrower than the window's true minimum) also reports
        // >1.5x and must not be discarded forever, or the column re-resizes on
        // every switch. So a suspicious excess is confirmed across two landing
        // passes (`classify_oversize`): a real min-size reproduces and is
        // honored; a one-off stale read resolves by the next pass and is dropped.
        const STALE_BOUNDS_RATIO: i32 = 3; // visible > requested * 3/2 → suspect
        const ABSURD_BOUNDS_RATIO: i32 = 4; // visible > requested * 4 → never trust
        let looks_stale_w =
            requested.width > 0 && visible_w * 2 > requested.width * STALE_BOUNDS_RATIO;
        let looks_stale_h =
            requested.height > 0 && visible_h * 2 > requested.height * STALE_BOUNDS_RATIO;
        let absurd_w = requested.width > 0 && visible_w > requested.width * ABSURD_BOUNDS_RATIO;
        let absurd_h = requested.height > 0 && visible_h > requested.height * ABSURD_BOUNDS_RATIO;
        let w_over = visible_w > requested.width + EDGE_EPSILON_PX;
        let h_over = visible_h > requested.height + EDGE_EPSILON_PX;

        // Read the prior-pass per-axis suspect state and update it for this
        // window in one locked section.
        let (record_w, suspect_w, record_h, suspect_h) = {
            let mut guard = lock_suspected_oversize();
            let map = guard.get_or_insert_with(HashMap::new);
            let (was_w, was_h) = map.get(&entry.window_id).copied().unwrap_or((false, false));
            let (record_w, suspect_w) = classify_oversize(w_over, looks_stale_w, was_w, absurd_w);
            let (record_h, suspect_h) = classify_oversize(h_over, looks_stale_h, was_h, absurd_h);
            if suspect_w || suspect_h {
                map.insert(entry.window_id, (suspect_w, suspect_h));
            } else {
                map.remove(&entry.window_id);
            }
            (record_w, suspect_w, record_h, suspect_h)
        };

        if record_w {
            tracing::debug!(
                "Width violation: {:?} requested {}px, visible {}px",
                entry.hwnd,
                requested.width,
                visible_w,
            );
            width_violations.push(WidthViolation {
                window_id: entry.window_id,
                min_width: visible_w,
                position_matches: (visible_rect.x - requested.x).abs() <= EDGE_EPSILON_PX,
                requested_left: requested.x,
            });
        } else if suspect_w {
            tracing::debug!(
                "Deferring suspect width until next landing confirms: {:?} \
                 requested {}px, visible {}px",
                entry.hwnd,
                requested.width,
                visible_w,
            );
        }
        if record_h {
            tracing::debug!(
                "Height violation: {:?} requested {}px, visible {}px",
                entry.hwnd,
                requested.height,
                visible_h,
            );
            height_violations.push(HeightViolation {
                window_id: entry.window_id,
                min_height: visible_h,
                position_matches: (visible_rect.y - requested.y).abs() <= EDGE_EPSILON_PX,
                requested_top: requested.y,
            });
        } else if suspect_h {
            tracing::debug!(
                "Deferring suspect height until next landing confirms: {:?} \
                 requested {}px, visible {}px",
                entry.hwnd,
                requested.height,
                visible_h,
            );
        }

        // A left/top displacement or an under-sized frame is not explained by
        // a minimum-size constraint. Request one guarded corrective landing.
        // Oversize-only mismatches use the existing constraint re-apply path.
        if position_mismatch || undersized {
            geometry_mismatches.push(entry.window_id);
        }
    }
    (width_violations, height_violations, geometry_mismatches)
}

/// Cloak newly off-screen entries and prune cloaks for windows no longer in the layout.
///
/// A DWM cloak rejection is not a logical state transition. `DWMWA_CLOAK` only
/// succeeds on windows this process owns, so every managed foreign HWND is
/// denied. We therefore fall back to a monitor-clearing sentinel park and keep
/// its durable marker. That park is verified against every monitor rectangle
/// before it returns, which is strictly stronger evidence of a hidden window
/// than an accepted DWM write, so it commits the hidden entry for any layout
/// rect. Only a park that could not be verified fails the placement.
fn sync_cloak_state(
    entries: &[DeferEntry],
    placements: &[WindowPlacement],
    failed_window_ids: &mut HashSet<u64>,
    preview_requests: &[PersistentPreviewRequest],
) {
    let preview_ids: HashSet<WindowId> = preview_requests
        .iter()
        .map(|request| request.window_id)
        .collect();
    let current_ids: HashSet<WindowId> = placements
        .iter()
        .map(|placement| placement.window_id)
        .collect();
    let _commit = lock_cloak_commit();

    for entry in entries {
        if failed_window_ids.contains(&entry.window_id) {
            // Do not replace a prior failed receipt with an unverified logical
            // cloak. A future exact pass can retry from the original evidence.
            continue;
        }

        let placement_exists = current_ids.contains(&entry.window_id);
        let should_cloak = should_cloak_entry(entry, placement_exists, false);
        if should_cloak {
            if !commit_global_cloak_state_locked(entry.window_id, true)
                && crate::visibility::move_window_offscreen(entry.window_id).is_err()
            {
                failed_window_ids.insert(entry.window_id);
            }
        } else if global_cloaked_contains(entry.window_id)
            && !commit_global_cloak_state_locked(entry.window_id, false)
        {
            failed_window_ids.insert(entry.window_id);
        }
    }

    // Preview sources have a separately verified park and DWM publication
    // transaction. Their global placement cloak must be removed only if DWM
    // confirms the corresponding uncloak.
    for window_id in preview_ids {
        if global_cloaked_contains(window_id) && !commit_global_cloak_state_locked(window_id, false)
        {
            failed_window_ids.insert(window_id);
        }
    }

    let stale: Vec<WindowId> = lock_cloaked()
        .as_ref()
        .map(|cloaked| {
            cloaked
                .keys()
                .filter(|window_id| !current_ids.contains(window_id))
                .copied()
                .collect()
        })
        .unwrap_or_default();
    for window_id in stale {
        // Failed cleanup remains owned for a later recovery pass rather than
        // silently dropping the only uncloak receipt.
        let _ = commit_global_cloak_state_locked(window_id, false);
    }
}

/// Read the class name of a window. Returns empty string on failure.
fn window_class_name(hwnd: HWND) -> String {
    let mut buf: [u16; 256] = [0; 256];
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    if len > 0 {
        String::from_utf16_lossy(&buf[..len as usize])
    } else {
        String::new()
    }
}

/// Position data passed to the nudge helper.
struct NudgeTarget {
    window_id: WindowId,
    hwnd: HWND,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

fn window_incarnation_identity(hwnd: HWND) -> Option<(u32, u32, String)> {
    if !unsafe { IsWindow(Some(hwnd)).as_bool() } {
        return None;
    }
    let mut process_id = 0u32;
    let thread_id = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
    let class = window_class_name(hwnd);
    (process_id != 0 && thread_id != 0 && !class.is_empty())
        .then_some((process_id, thread_id, class))
}

/// Send a (w-1 -> w) synchronous SetWindowPos pair to each known
/// compositor-sensitive window. The final restore also forces non-client
/// recalculation, then one DwmFlush publishes the repaired surfaces before the
/// landing is considered complete.
fn nudge_sticky_compositor_windows(targets: &[NudgeTarget]) -> Vec<WindowId> {
    let mut repaired = Vec::new();
    let mut failed = Vec::new();
    for t in targets {
        let Some(identity) = window_incarnation_identity(t.hwnd) else {
            failed.push(t.window_id);
            continue;
        };
        let class = identity.2.clone();
        if !crate::thumbnail::is_compositor_sensitive_class_str(&class) {
            continue;
        }
        let flags = SWP_NOZORDER | SWP_NOACTIVATE;
        unsafe {
            if SetWindowPos(t.hwnd, None, t.x, t.y, t.w - 1, t.h, flags).is_err() {
                failed.push(t.window_id);
                continue;
            }
            // Re-validate the HWND between the pair: the first SetWindowPos
            // pumps messages on the target thread and can cause the window to
            // be destroyed; the handle could be recycled for an unrelated
            // window before the restore call lands. Re-checking both the
            // handle validity and the class name catches recycling. If either
            // fails the target is left at w-1 rather than risk resizing the
            // wrong window — next apply pass will correct it.
            if window_incarnation_identity(t.hwnd).as_ref() != Some(&identity) {
                failed.push(t.window_id);
                continue;
            }
            if let Err(e) = SetWindowPos(t.hwnd, None, t.x, t.y, t.w, t.h, flags | SWP_FRAMECHANGED)
            {
                // Restore failed — window is stranded at w-1 (1px narrower)
                // until the next apply_layout re-places it. Log so the state
                // is diagnosable; the next apply will correct geometry.
                tracing::warn!(
                    "Nudge restore SetWindowPos failed for hwnd={:?} class={} — window left at w-1 until next apply: {:?}",
                    t.hwnd, class, e
                );
                failed.push(t.window_id);
                continue;
            }
        }
        if window_incarnation_identity(t.hwnd).as_ref() != Some(&identity) {
            failed.push(t.window_id);
            continue;
        }
        repaired.push(t.window_id);
        tracing::debug!(
            "Refreshed compositor-sensitive window (class={}, hwnd={:?})",
            class,
            t.hwnd
        );
    }
    if !repaired.is_empty() && unsafe { DwmFlush() }.is_err() {
        failed.extend(repaired);
    }
    failed.sort_unstable();
    failed.dedup();
    failed
}

type InsetMap = HashMap<WindowId, (i32, i32, i32, i32)>;

/// Global inset cache for the `apply_layout` path (which passes `cache: None`).
/// Ensures windows returning from off-screen get correct insets even without
/// a per-worker PlacementCache.
static GLOBAL_INSET_CACHE: Mutex<Option<InsetMap>> = Mutex::new(None);
/// Invalidates thread-local `PlacementCache` inset/position entries without a
/// per-frame global lock. Incremented for display/theme changes and when a
/// landing pass proves that one window's cached insets went stale.
static INSET_CACHE_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Clear the global inset cache. Must be called when system theme or DWM
/// metrics change (e.g., high contrast toggle, display change) so that stale
/// invisible-border values don't cause incorrect window sizing.
pub fn clear_inset_cache() {
    if let Ok(mut global) = GLOBAL_INSET_CACHE.lock() {
        *global = None;
    }
    INSET_CACHE_GENERATION.fetch_add(1, Ordering::AcqRel);
}

/// Evict one proven-stale inset and invalidate the animation worker's local
/// cache. The local cache cannot be addressed directly from the landing
/// worker, so the generation performs the cross-thread handoff.
fn invalidate_window_insets(window_id: WindowId, cache: &mut Option<&mut PlacementCache>) {
    if let Some(ref mut cache) = *cache {
        cache.insets.remove(&window_id);
        cache.positions.remove(&window_id);
    }
    if let Ok(mut global) = GLOBAL_INSET_CACHE.lock() {
        if let Some(ref mut map) = *global {
            map.remove(&window_id);
        }
    }
    INSET_CACHE_GENERATION.fetch_add(1, Ordering::AcqRel);
}

/// Look up border insets for a window, using a sticky cache to protect against
/// stale DWM data for windows that were parked off-screen.
///
/// Border insets are determined by window style and DPI, not position, so they
/// are cached until display/theme invalidation or landing verification proves
/// that a window changed its frame metrics.
fn cached_border_insets(
    hwnd: HWND,
    window_id: WindowId,
    local_cache: Option<&mut PlacementCache>,
) -> (i32, i32, i32, i32) {
    // Check local (per-worker) cache first
    if let Some(cached) = local_cache
        .as_ref()
        .and_then(|c| c.insets.get(&window_id).copied())
    {
        return cached;
    }
    // Check global cache (shared across apply_layout threads)
    if let Ok(global) = GLOBAL_INSET_CACHE.lock() {
        if let Some(cached) = global.as_ref().and_then(|m| m.get(&window_id).copied()) {
            // Promote to local cache for fast subsequent lookups
            if let Some(cache) = local_cache {
                cache.insets.insert(window_id, cached);
            }
            return cached;
        }
    }
    // No cache — query DWM and cache if non-zero
    let fresh = invisible_border_insets(hwnd);
    if fresh != (0, 0, 0, 0) {
        if let Some(cache) = local_cache {
            cache.insets.insert(window_id, fresh);
        }
        if let Ok(mut global) = GLOBAL_INSET_CACHE.lock() {
            global
                .get_or_insert_with(HashMap::new)
                .insert(window_id, fresh);
        }
    }
    fresh
}

/// Public wrapper over `invisible_border_insets` that takes a `WindowId`.
/// Returns `(left, top, right, bottom)` insets, or `(0, 0, 0, 0)` if the
/// window has no DWM bounds available. Used by callers that need to
/// translate between chrome (`GetWindowRect`) coordinates and visible-
/// content (layout) coordinates without reaching into placement internals.
pub fn get_window_invisible_insets(window_id: WindowId) -> (i32, i32, i32, i32) {
    let Ok(hwnd) = window_id_to_hwnd(window_id) else {
        return (0, 0, 0, 0);
    };
    invisible_border_insets(hwnd)
}

/// Compute invisible border insets for a window.
///
/// Windows 10/11 windows have invisible borders (typically ~7px on left, right,
/// bottom and 0px on top). `SetWindowPos` operates on the full frame rect
/// including these borders. To make the *visible* area fill our target rect,
/// we expand the frame rect by the invisible border amount.
///
/// Returns (left, top, right, bottom) insets to subtract/add to the target rect.
/// Largest invisible resize border Windows actually draws, with headroom.
///
/// It is about 7px at 100% DPI and scales with the monitor, so ~14px at 200%.
/// A measurement past this bound is not a border: it is a displaced compositor
/// surface being reported as geometry.
pub(crate) const MAX_INVISIBLE_BORDER_PX: i32 = 32;

/// Insets implied by a window's outer frame and its visible DWM frame, or `None`
/// when the pair cannot honestly describe an invisible border.
///
/// `DWMWA_EXTENDED_FRAME_BOUNDS` reports where DWM is *compositing* the window.
/// For a Chromium, Electron or WinUI window whose visual is still at a stale
/// transform, that is not where the window is, and the difference is exactly what
/// this function would otherwise return as an inset. Accepting it is worse than
/// having no measurement: the caller places the chrome shifted by the error so
/// the displaced content lands on the layout slot, the next verification pass
/// then sees matching edges, and the bogus inset is cached and blessed. The
/// window stays visibly broken until something clears the inset cache.
///
/// So a measurement is only trusted when every side is a plausible border and
/// the horizontal and vertical pairs are symmetric, which a real invisible border
/// always is. Pure, and unit-tested against the displaced-visual case.
pub(crate) fn insets_from_frames(frame: Rect, extended: Rect) -> Option<(i32, i32, i32, i32)> {
    let left = extended.x - frame.x;
    let top = extended.y - frame.y;
    let right = frame.right() - extended.right();
    let bottom = frame.bottom() - extended.bottom();

    let plausible = |value: i32| (0..=MAX_INVISIBLE_BORDER_PX).contains(&value);
    if !plausible(left) || !plausible(top) || !plausible(right) || !plausible(bottom) {
        return None;
    }
    // Windows draws the same border on both sides; a horizontal or vertical
    // asymmetry means one edge absorbed a displacement.
    const MAX_ASYMMETRY_PX: i32 = 2;
    if (left - right).abs() > MAX_ASYMMETRY_PX {
        return None;
    }
    Some((left, top, right, bottom))
}

pub(crate) fn invisible_border_insets(hwnd: HWND) -> (i32, i32, i32, i32) {
    unsafe {
        let mut frame_rect = RECT::default();
        if GetWindowRect(hwnd, &mut frame_rect).is_err() {
            return (0, 0, 0, 0);
        }

        let mut extended_rect = RECT::default();
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut extended_rect as *mut RECT as *mut _,
            std::mem::size_of::<RECT>() as u32,
        )
        .is_err()
        {
            return (0, 0, 0, 0);
        }

        let frame = Rect::new(
            frame_rect.left,
            frame_rect.top,
            frame_rect.right - frame_rect.left,
            frame_rect.bottom - frame_rect.top,
        );
        let extended = Rect::new(
            extended_rect.left,
            extended_rect.top,
            extended_rect.right - extended_rect.left,
            extended_rect.bottom - extended_rect.top,
        );
        // No measurement is better than a poisoned one: zero insets place the
        // chrome exactly where the layout asked, which is off by at most a real
        // border, instead of off by a whole displaced surface.
        insets_from_frames(frame, extended).unwrap_or((0, 0, 0, 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_oversize() {
        // (over, looks_stale, was_suspected, absurd)
        // Not oversize -> never record, never suspect.
        assert_eq!(
            classify_oversize(false, false, false, false),
            (false, false)
        );
        // Oversize but within the stale ratio -> genuine, record immediately.
        assert_eq!(classify_oversize(true, false, false, false), (true, false));
        // Suspiciously large, first sighting -> defer (suspect), don't record.
        assert_eq!(classify_oversize(true, true, false, false), (false, true));
        // Suspiciously large, reproduced from last pass -> confirmed, record.
        assert_eq!(classify_oversize(true, true, true, false), (true, false));
        // Absurdly large -> never trusted, even reproduced (chronic stale read).
        assert_eq!(classify_oversize(true, true, true, true), (false, false));
        assert_eq!(classify_oversize(true, true, false, true), (false, false));
    }

    #[test]
    fn test_animation_move_uses_no_size_only_for_stable_visible_dimensions() {
        let current = WindowPlacement {
            window_id: 1,
            rect: Rect::new(120, 40, 800, 600),
            visibility: Visibility::Visible,
            column_index: 0,
        };

        assert!(animation_move_is_position_only(
            Some((Rect::new(100, 40, 800, 600), Visibility::Visible)),
            &current,
        ));
        assert!(!animation_move_is_position_only(
            Some((Rect::new(100, 40, 799, 600), Visibility::Visible)),
            &current,
        ));
        assert!(!animation_move_is_position_only(
            Some((Rect::new(100, 40, 800, 600), Visibility::OffScreenLeft)),
            &current,
        ));
        assert!(!animation_move_is_position_only(None, &current));
    }

    #[test]
    fn test_adaptive_animation_dispatch_serializes_sensitive_windows() {
        assert_eq!(
            animation_dispatch_mode(
                AnimationPlacementPolicy::AdaptiveCompositorSafe,
                true,
                false,
            ),
            AnimationDispatchMode::Synchronous
        );
        assert_eq!(
            animation_dispatch_mode(AnimationPlacementPolicy::AdaptiveCompositorSafe, true, true,),
            AnimationDispatchMode::SkipHungSensitive
        );
        assert_eq!(
            animation_dispatch_mode(
                AnimationPlacementPolicy::AdaptiveCompositorSafe,
                false,
                false,
            ),
            AnimationDispatchMode::Asynchronous
        );
        assert_eq!(
            animation_dispatch_mode(AnimationPlacementPolicy::LegacyAsync, true, true),
            AnimationDispatchMode::Asynchronous
        );
    }

    #[test]
    fn test_adaptive_sensitive_move_is_sync_position_only_without_frame_change() {
        let flags = visible_position_flags(true, AnimationDispatchMode::Synchronous, true);
        assert_eq!(flags.0 & SWP_ASYNCWINDOWPOS.0, 0);
        assert_ne!(flags.0 & SWP_NOSIZE.0, 0);
        assert_eq!(flags.0 & SWP_FRAMECHANGED.0, 0);

        let ordinary = visible_position_flags(true, AnimationDispatchMode::Asynchronous, true);
        assert_ne!(ordinary.0 & SWP_ASYNCWINDOWPOS.0, 0);
        assert_ne!(ordinary.0 & SWP_NOSIZE.0, 0);
        assert_eq!(ordinary.0 & SWP_FRAMECHANGED.0, 0);

        let first_sensitive_frame =
            visible_position_flags(true, AnimationDispatchMode::Synchronous, false);
        assert_eq!(first_sensitive_frame.0 & SWP_ASYNCWINDOWPOS.0, 0);
        assert_eq!(first_sensitive_frame.0 & SWP_NOSIZE.0, 0);
        assert_eq!(first_sensitive_frame.0 & SWP_FRAMECHANGED.0, 0);

        let landing = visible_position_flags(false, AnimationDispatchMode::Synchronous, false);
        assert_eq!(landing.0 & SWP_ASYNCWINDOWPOS.0, 0);
        assert_ne!(landing.0 & SWP_FRAMECHANGED.0, 0);
    }

    #[test]
    fn test_geometry_mismatch_flags_cover_all_edges_with_tolerance() {
        let requested = Rect::new(100, 200, 800, 600);
        assert_eq!(
            geometry_mismatch_flags(requested, requested),
            (false, false, false)
        );
        assert_eq!(
            geometry_mismatch_flags(Rect::new(102, 198, 800, 600), requested),
            (false, false, false),
            "two-pixel DWM rounding is tolerated on every edge"
        );
        assert_eq!(
            geometry_mismatch_flags(Rect::new(97, 200, 800, 600), requested),
            (true, true, false),
            "left displacement is a positional edge mismatch"
        );
        assert_eq!(
            geometry_mismatch_flags(Rect::new(100, 197, 800, 600), requested),
            (true, true, false),
            "top displacement is a positional edge mismatch"
        );
        assert_eq!(
            geometry_mismatch_flags(Rect::new(100, 200, 790, 600), requested),
            (false, true, true),
            "right-edge undersize is retried without a min-width constraint"
        );
        assert_eq!(
            geometry_mismatch_flags(Rect::new(100, 200, 800, 590), requested),
            (false, true, true),
            "bottom-edge undersize is retried without a min-height constraint"
        );
    }

    #[test]
    fn test_global_inset_generation_invalidates_local_positions_and_insets() {
        let mut cache = PlacementCache::new();
        let wid = 0x7FFF_FF02;
        cache
            .positions
            .insert(wid, (Rect::new(1, 2, 300, 200), Visibility::Visible));
        cache.insets.insert(wid, (8, 0, 8, 8));

        clear_inset_cache();
        cache.sync_inset_generation();

        assert!(cache.positions.is_empty());
        assert!(cache.insets.is_empty());
    }

    #[test]
    fn test_placement_cache_clear_drops_renderer_classification() {
        let mut cache = PlacementCache::new();
        let wid = 0x7FFF_FF03;
        cache
            .positions
            .insert(wid, (Rect::new(1, 2, 300, 200), Visibility::Visible));
        cache.insets.insert(wid, (8, 0, 8, 8));
        cache.compositor_sensitive.insert(wid, true);

        cache.clear();

        assert!(cache.positions.is_empty());
        assert!(cache.compositor_sensitive.is_empty());
        assert_eq!(cache.insets.get(&wid), Some(&(8, 0, 8, 8)));
    }

    #[test]
    fn test_failed_direct_cloak_does_not_create_a_false_recovery_receipt() {
        // A direct cloak records ownership only after DWM accepts it. An
        // invalid/foreign HWND must rely on its verified sentinel park rather
        // than appearing logically cloaked when no physical write occurred.
        let wid: WindowId = 0x7FFF_FF01;
        assert!(!dwm_cloak_window(wid));
        assert!(
            !lock_direct_cloaked()
                .as_ref()
                .is_some_and(|s| s.contains_key(&wid)),
            "failed direct cloak must not record false ownership"
        );
    }

    #[test]
    fn test_apply_placements_empty() {
        // Verify empty placements succeed without error
        let config = PlatformConfig::default();
        let result = apply_placements(&[], &config, None, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_failed_physical_ghost_cloak_rolls_back_logical_mark() {
        let invalid_window = 0;
        {
            let mut guard = lock_ghost_cloaked();
            if let Some(ref mut set) = *guard {
                set.remove(&invalid_window);
            }
        }

        assert!(!try_mark_ghost_cloaked(invalid_window));
        assert!(!ghost_cloaked_contains(invalid_window));
    }

    #[test]
    fn invalid_nudge_target_is_reported_as_failed() {
        let failed = nudge_sticky_compositor_windows(&[NudgeTarget {
            window_id: 77,
            hwnd: HWND::default(),
            x: 0,
            y: 0,
            w: 800,
            h: 600,
        }]);
        assert_eq!(failed, vec![77]);
    }

    #[test]
    fn test_zero_sized_offscreen_marker_uses_global_sentinel() {
        let placement = WindowPlacement {
            window_id: 1,
            rect: Rect::new(0, 0, 0, 0),
            visibility: Visibility::OffScreenLeft,
            column_index: 0,
        };
        assert_eq!(
            offscreen_position(&placement, 8, 8),
            (
                crate::MOVE_OFFSCREEN_SENTINEL_COORD,
                crate::MOVE_OFFSCREEN_SENTINEL_COORD,
            )
        );

        let ordinary = WindowPlacement {
            window_id: 2,
            rect: Rect::new(-100, 50, 800, 600),
            visibility: Visibility::OffScreenLeft,
            column_index: 0,
        };
        assert_eq!(offscreen_position(&ordinary, 8, 6), (-108, 44));
    }

    #[test]
    fn test_apply_placements_rejects_invalid_windows() {
        let config = PlatformConfig::default();
        let placements = vec![WindowPlacement {
            window_id: 0,
            rect: Rect::new(0, 0, 800, 600),
            visibility: Visibility::OffScreenLeft,
            column_index: 0,
        }];

        // A skipped invalid HWND cannot satisfy a requested layout receipt.
        let result = apply_placements(&placements, &config, None, false);
        assert!(result.is_err());
    }

    #[test]
    fn recycled_hwnd_cloak_receipt_is_pruned() {
        let wid = 0x1234;
        let original = crate::event_hooks::WindowEventIdentity {
            token: 1,
            process_id: 2,
            thread_id: 3,
            class_name: "Window".into(),
        };
        let replacement = crate::event_hooks::WindowEventIdentity {
            token: 4,
            ..original.clone()
        };
        let mut ledger = Some(HashMap::from([(wid, original)]));

        assert!(reconcile_cloak_receipt(&mut ledger, wid, Some(&replacement)).is_none());
        assert!(!ledger.as_ref().unwrap().contains_key(&wid));
    }
}

#[cfg(test)]
mod inset_plausibility_tests {
    use super::{insets_from_frames, MAX_INVISIBLE_BORDER_PX};
    use leopardwm_core_layout::Rect;

    #[test]
    fn a_real_invisible_border_is_accepted() {
        // 1920x1080 client, 7px border on the sides and bottom, none on top.
        let frame = Rect::new(93, 100, 1934, 1087);
        let extended = Rect::new(100, 100, 1920, 1080);
        assert_eq!(insets_from_frames(frame, extended), Some((7, 0, 7, 7)));
    }

    #[test]
    fn a_zero_border_window_is_accepted() {
        let rect = Rect::new(0, 0, 800, 600);
        assert_eq!(insets_from_frames(rect, rect), Some((0, 0, 0, 0)));
    }

    #[test]
    fn a_displaced_compositor_surface_is_rejected() {
        // The window is at x=100 but DWM reports the visual 240px to the right,
        // which would otherwise be read as a 240px left border and a clamped
        // right border, and then baked into the placement.
        let frame = Rect::new(100, 100, 1920, 1080);
        let extended = Rect::new(340, 100, 1920, 1080);
        assert_eq!(insets_from_frames(frame, extended), None);
    }

    #[test]
    fn an_asymmetric_pair_inside_the_bound_is_still_rejected() {
        // Both sides plausible on their own, but a real border is symmetric.
        let frame = Rect::new(100, 100, 1920, 1080);
        let extended = Rect::new(120, 100, 1900, 1080);
        assert_eq!(insets_from_frames(frame, extended), None);
    }

    #[test]
    fn a_border_past_the_bound_is_rejected() {
        let over = MAX_INVISIBLE_BORDER_PX + 1;
        let frame = Rect::new(0, 0, 1000 + over * 2, 800);
        let extended = Rect::new(over, 0, 1000, 800);
        assert_eq!(insets_from_frames(frame, extended), None);
    }

    #[test]
    fn a_negative_side_is_rejected() {
        // Extended bounds outside the frame cannot describe a border.
        let frame = Rect::new(100, 100, 800, 600);
        let extended = Rect::new(90, 100, 820, 600);
        assert_eq!(insets_from_frames(frame, extended), None);
    }
}

#[cfg(test)]
mod visible_landing_policy_tests {
    use super::should_verify_visible_landing;
    use leopardwm_core_layout::Visibility;

    #[test]
    fn visible_floats_require_physical_landing_verification() {
        assert!(should_verify_visible_landing(
            Visibility::Visible,
            usize::MAX
        ));
        assert!(should_verify_visible_landing(Visibility::Visible, 0));
        assert!(!should_verify_visible_landing(
            Visibility::OffScreenLeft,
            usize::MAX
        ));
    }
}

#[cfg(test)]
mod compositor_return_repair_tests {
    use super::{latch_return_repair, returns_from_offscreen_park, take_return_repairs_for};
    use leopardwm_core_layout::{Rect, Visibility};
    use std::collections::HashSet;

    fn parked() -> Option<(Rect, Visibility)> {
        Some((
            Rect::new(-32000, -32000, 800, 600),
            Visibility::OffScreenLeft,
        ))
    }

    fn onscreen() -> Option<(Rect, Visibility)> {
        Some((Rect::new(100, 100, 800, 600), Visibility::Visible))
    }

    #[test]
    fn a_window_leaving_a_preview_is_repaired() {
        // The exact landing pass runs without a cache, so the preview
        // registration is the only signal available there.
        assert!(returns_from_offscreen_park(
            Visibility::Visible,
            true,
            false,
            false,
            None
        ));
    }

    #[test]
    fn a_window_leaving_the_placement_cloak_is_repaired() {
        assert!(returns_from_offscreen_park(
            Visibility::Visible,
            false,
            true,
            false,
            None
        ));
    }

    #[test]
    fn a_window_leaving_a_direct_sentinel_park_is_repaired() {
        assert!(returns_from_offscreen_park(
            Visibility::Visible,
            false,
            false,
            true,
            None
        ));
    }

    #[test]
    fn a_cached_parked_window_becoming_visible_is_repaired() {
        assert!(returns_from_offscreen_park(
            Visibility::Visible,
            false,
            false,
            false,
            parked()
        ));
    }

    #[test]
    fn an_ordinary_move_is_not_repaired() {
        // Repairing every scroll landing would resize every Chromium window on
        // every navigation.
        assert!(!returns_from_offscreen_park(
            Visibility::Visible,
            false,
            false,
            false,
            onscreen()
        ));
        assert!(!returns_from_offscreen_park(
            Visibility::Visible,
            false,
            false,
            false,
            None
        ));
    }

    #[test]
    fn a_placement_that_is_not_visible_is_never_repaired() {
        // Nudging a window on its way out would resize it off-screen for nothing.
        for previous in [None, parked(), onscreen()] {
            assert!(!returns_from_offscreen_park(
                Visibility::OffScreenRight,
                true,
                true,
                true,
                previous
            ));
        }
    }

    #[test]
    fn disjoint_exact_landing_keeps_another_window_repair_receipt() {
        let returning = 0x7fff_ff10;
        let unrelated = 0x7fff_ff11;
        latch_return_repair(returning);

        let unrelated_ids = HashSet::from([unrelated]);
        assert!(take_return_repairs_for(&unrelated_ids).is_empty());

        let returning_ids = HashSet::from([returning]);
        assert_eq!(take_return_repairs_for(&returning_ids), returning_ids);
    }
}

#[cfg(test)]
mod persistent_preview_placement_tests {
    use super::{persistent_preview_request, should_cloak_entry, DeferEntry};
    use leopardwm_core_layout::{Rect, Visibility};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::SET_WINDOW_POS_FLAGS;

    fn preview_entry() -> DeferEntry {
        DeferEntry {
            hwnd: HWND::default(),
            window_id: 1,
            x: 0,
            y: 0,
            w: 750,
            h: 800,
            layout_rect: Rect::new(0, 0, 750, 800),
            used_insets: (0, 0, 0, 0),
            validate_insets: false,
            visibility: Visibility::OffScreenLeft,
            flags: SET_WINDOW_POS_FLAGS::default(),
            column_index: 0,
            preview_source: true,
        }
    }

    #[test]
    fn new_preview_flushes_once_but_existing_animation_frames_do_not() {
        assert!(super::preview_commit_needs_flush(true, 1, 1));
        assert!(!super::preview_commit_needs_flush(true, 0, 1));
        assert!(super::preview_commit_needs_flush(false, 0, 1));
        assert!(!super::preview_commit_needs_flush(false, 0, 0));
    }

    #[test]
    fn left_preview_is_cropped_strictly_to_the_owner_monitor() {
        let request = persistent_preview_request(
            1,
            Rect::new(500, 0, 750, 800),
            Rect::new(1000, 0, 1000, 800),
        )
        .unwrap();
        assert_eq!(request.source_rect, Rect::new(500, 0, 250, 800));
        assert_eq!(
            request.destination_screen_rect,
            Rect::new(1000, 0, 250, 800)
        );
        assert_eq!(request.expected_source_size, (750, 800));
        assert!(!request
            .destination_screen_rect
            .intersects(&Rect::new(0, 0, 1000, 800)));
    }

    #[test]
    fn right_preview_is_symmetric_and_cannot_touch_the_next_monitor() {
        let request = persistent_preview_request(
            1,
            Rect::new(1750, 0, 750, 800),
            Rect::new(1000, 0, 1000, 800),
        )
        .unwrap();
        assert_eq!(request.source_rect, Rect::new(0, 0, 250, 800));
        assert_eq!(
            request.destination_screen_rect,
            Rect::new(1750, 0, 250, 800)
        );
        assert!(!request
            .destination_screen_rect
            .intersects(&Rect::new(2000, 0, 1000, 800)));
    }

    #[test]
    fn preview_source_remains_uncloaked_while_ordinary_offscreen_windows_cloak() {
        let preview = preview_entry();
        assert!(!should_cloak_entry(&preview, true, false));
        let ordinary = DeferEntry {
            preview_source: false,
            ..preview
        };
        assert!(should_cloak_entry(&ordinary, true, false));
        assert!(!should_cloak_entry(&ordinary, false, false));
        assert!(!should_cloak_entry(&ordinary, true, true));
    }
}
