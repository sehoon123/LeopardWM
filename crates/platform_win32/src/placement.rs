//! Window placement application via SetWindowPos / DeferWindowPos.

use crate::types::{AnimationPlacementPolicy, PlatformConfig, Win32Error};
use crate::window_id_to_hwnd;
use crate::window_region::{
    apply_window_region_clip, has_owned_window_region, prepare_window_region_clip,
    reconcile_window_regions, restore_all_window_regions, restore_window_region, WindowRegionClip,
};
use leopardwm_core_layout::{Rect, Visibility, WindowId, WindowPlacement};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmFlush, DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS,
    DWMWINDOWATTRIBUTE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BeginDeferWindowPos, DeferWindowPos, EndDeferWindowPos, GetClassNameW, GetWindowRect,
    IsHungAppWindow, IsIconic, IsWindow, IsZoomed, SetWindowPos, ShowWindow, SET_WINDOW_POS_FLAGS,
    SWP_ASYNCWINDOWPOS, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SW_RESTORE,
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
unsafe fn dwm_set_cloak(hwnd: HWND, cloaked: bool) -> bool {
    // NOTE: DWMWA_CLOAK only succeeds on windows owned by the calling
    // process; cloaking another process's window returns E_ACCESSDENIED
    // (0x80070005). Callers that require an actually-hidden source (the DWM
    // thumbnail animation) must check this result instead of treating logical
    // set membership as proof that the external HWND was cloaked.
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

fn global_cloaked_contains(wid: WindowId) -> bool {
    let guard = lock_cloaked();
    guard.as_ref().is_some_and(|set| set.contains(&wid))
}

// ---------------------------------------------------------------------
// GHOST_CLOAKED — distinct cloak set populated only by the ghost-animation
// path. Logical-OR'd with GLOBAL_CLOAKED to determine the effective cloak
// state (see `apply_cloak_state`).
// ---------------------------------------------------------------------

static GHOST_CLOAKED: Mutex<Option<HashSet<WindowId>>> = Mutex::new(None);

fn lock_ghost_cloaked() -> std::sync::MutexGuard<'static, Option<HashSet<WindowId>>> {
    GHOST_CLOAKED
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
}

fn ghost_cloaked_contains(wid: WindowId) -> bool {
    let guard = lock_ghost_cloaked();
    guard.as_ref().is_some_and(|set| set.contains(&wid))
}

/// Mark a source for ghost animation only when DWM physically cloaks it.
/// External application HWNDs normally reject DWMWA_CLOAK with
/// E_ACCESSDENIED; in that case roll back the logical mark so the caller can
/// safely fall back to live placement instead of drawing a thumbnail over an
/// uncloaked source.
pub fn try_mark_ghost_cloaked(wid: WindowId) -> bool {
    let _commit = lock_cloak_commit();
    {
        let mut guard = lock_ghost_cloaked();
        guard.get_or_insert_with(HashSet::new).insert(wid);
    }
    if apply_cloak_state_locked(wid) {
        true
    } else {
        {
            let mut guard = lock_ghost_cloaked();
            if let Some(ref mut set) = *guard {
                set.remove(&wid);
            }
        }
        let _ = apply_cloak_state_locked(wid);
        false
    }
}

/// Atomically remove a window from the ghost-cloak set and commit the new OR
/// state (which uncloaks it unless normal placement still requires a cloak).
pub fn unmark_ghost_cloaked(wid: WindowId) {
    let _commit = lock_cloak_commit();
    unmark_ghost_cloaked_locked(wid);
    let _ = apply_cloak_state_locked(wid);
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

static DIRECT_CLOAKED: Mutex<Option<HashSet<WindowId>>> = Mutex::new(None);

fn lock_direct_cloaked() -> std::sync::MutexGuard<'static, Option<HashSet<WindowId>>> {
    DIRECT_CLOAKED
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
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
fn lock_cloaked() -> std::sync::MutexGuard<'static, Option<HashSet<WindowId>>> {
    GLOBAL_CLOAKED
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
}

/// Force-cloak a single window directly, without touching either tracking
/// set. For windows held OUTSIDE normal layout management (e.g. a stashed
/// scratchpad window that has been removed from its workspace) — nothing
/// in the placement path will reposition or uncloak it, so a direct cloak
/// is safe and stays put until the owner uncloaks it.
pub fn dwm_cloak_window(window_id: WindowId) {
    let _commit = lock_cloak_commit();
    {
        let mut guard = lock_direct_cloaked();
        guard.get_or_insert_with(HashSet::new).insert(window_id);
    }
    if let Ok(hwnd) = window_id_to_hwnd(window_id) {
        unsafe {
            let _ = dwm_set_cloak(hwnd, true);
        }
    }
}

/// Force-uncloak a window by its WindowId regardless of either tracking
/// set's membership. Removes from both `GLOBAL_CLOAKED` and
/// `GHOST_CLOAKED`. Used by shutdown / panic cleanup.
///
/// Bypasses `apply_cloak_state`'s OR-check: the intent here is "force
/// visible" regardless of why the window was originally cloaked.
pub fn dwm_uncloak_window(window_id: WindowId) {
    let _commit = lock_cloak_commit();
    {
        let mut guard = lock_cloaked();
        if let Some(ref mut set) = *guard {
            set.remove(&window_id);
        }
    }
    {
        let mut guard = lock_ghost_cloaked();
        if let Some(ref mut set) = *guard {
            set.remove(&window_id);
        }
    }
    {
        let mut guard = lock_direct_cloaked();
        if let Some(ref mut set) = *guard {
            set.remove(&window_id);
        }
    }
    if let Ok(hwnd) = window_id_to_hwnd(window_id) {
        unsafe { dwm_set_cloak(hwnd, false) };
    }
}

/// Force-uncloak every tracked window from both sets. Called during
/// shutdown and panic recovery. Bypasses `apply_cloak_state`.
pub fn dwm_uncloak_all() {
    restore_all_window_regions();
    let _commit = lock_cloak_commit();
    let global_ids: Vec<WindowId> = {
        let mut guard = lock_cloaked();
        match guard.as_mut() {
            Some(set) => set.drain().collect(),
            None => Vec::new(),
        }
    };
    let ghost_ids: Vec<WindowId> = {
        let mut guard = lock_ghost_cloaked();
        match guard.as_mut() {
            Some(set) => set.drain().collect(),
            None => Vec::new(),
        }
    };
    let direct_ids: Vec<WindowId> = {
        let mut guard = lock_direct_cloaked();
        match guard.as_mut() {
            Some(set) => set.drain().collect(),
            None => Vec::new(),
        }
    };
    // Use a set union so we don't issue redundant DWM calls for windows
    // present in more than one set. dwm_set_cloak is idempotent.
    let mut seen: HashSet<WindowId> =
        HashSet::with_capacity(global_ids.len() + ghost_ids.len() + direct_ids.len());
    for wid in global_ids.into_iter().chain(ghost_ids).chain(direct_ids) {
        if seen.insert(wid) {
            if let Ok(hwnd) = window_id_to_hwnd(wid) {
                unsafe { dwm_set_cloak(hwnd, false) };
            }
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
    global_cloaked_contains(window_id) || ghost_cloaked_contains(window_id)
}

/// Drain and uncloak all tracked windows. Called when the placement list
/// becomes empty (e.g., switching to an empty workspace) so that windows
/// from the previous call are not left permanently invisible.
fn uncloak_all_tracked() {
    let _commit = lock_cloak_commit();
    let ids: Vec<WindowId> = {
        let mut guard = lock_cloaked();
        match guard.as_mut() {
            Some(set) => set.drain().collect(),
            None => return,
        }
    };
    for wid in ids {
        let _ = apply_cloak_state_locked(wid);
    }
}

/// Global set of window IDs currently cloaked by the placement system.
static GLOBAL_CLOAKED: Mutex<Option<HashSet<WindowId>>> = Mutex::new(None);

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
    /// Visible tiled windows whose DWM frame did not land on the requested
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
    region_clip_bounds: Option<Rect>,
    fallback_rect: Option<Rect>,
    fallback_visibility: Option<Visibility>,
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
        // Empty layout is also a hard region-lifecycle boundary.
        restore_all_window_regions();
        // Uncloak all tracked windows — no placements means all previous
        // windows have left this layout (e.g., workspace switch to empty workspace).
        uncloak_all_tracked();
        return Ok(empty_result);
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

    let DeferEntryPlan {
        mut entries,
        skipped,
        stale_region_window_ids,
    } = build_defer_entries(
        placements,
        region_clips,
        &mut cache,
        animation_frame,
        config.animation_placement_policy,
        high_contrast,
    );

    // Restrict first, then reveal and move. This removes the frame in which
    // DWM could previously display a gray rectangular backing surface outside
    // the owner monitor before SetWindowRgn was committed.
    let mut failed_window_ids = HashSet::new();
    let pre_fallbacks =
        prepare_entry_region_clips(&mut entries, &mut failed_window_ids, animation_frame);

    // Never reveal a window whose safe placement did not land: it is still at
    // whatever rectangle it had, which may cross into a neighboring monitor.
    uncloak_becoming_visible(&entries, &failed_window_ids);

    let (applied, position_failures) = position_entries(&entries);
    failed_window_ids.extend(position_failures);
    let post_fallbacks =
        apply_entry_region_clips(&mut entries, &mut failed_window_ids, animation_frame);

    let mut active_clipped_window_ids: HashSet<WindowId> =
        region_clips.iter().map(|clip| clip.window_id).collect();
    for entry in &entries {
        // A window we could not place keeps whatever restrictive region it has:
        // it is still at its old rectangle, so releasing the region would expose
        // the very overflow the region was installed to hide.
        if entry.region_clip_bounds.is_none() && !failed_window_ids.contains(&entry.window_id) {
            active_clipped_window_ids.remove(&entry.window_id);
        }
    }
    // A hung renderer whose frame was skipped entirely has not moved either, so
    // its region stays until a later pass can place it.
    active_clipped_window_ids.extend(stale_region_window_ids.iter().copied());
    // Regions on windows becoming fully contained keep their old restrictive region until
    // the move completes; only then is the region removed.
    reconcile_window_regions(
        &managed_window_ids,
        &active_clipped_window_ids,
        !animation_frame,
    );
    let region_fallbacks = pre_fallbacks + post_fallbacks;

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

    // Update cache: remove stale entries (windows no longer in placements),
    // update positioned entries, and keep skipped-unchanged entries intact.
    if let Some(cache) = cache {
        let current_ids: std::collections::HashSet<u64> =
            placements.iter().map(|p| p.window_id).collect();
        // Remove windows that are no longer in the layout
        cache.positions.retain(|id, _| current_ids.contains(id));
        cache.insets.retain(|id, _| current_ids.contains(id));
        cache
            .compositor_sensitive
            .retain(|id, _| current_ids.contains(id));
        // Record the effective entry. A region failure may have synchronously
        // switched this HWND to its safe fallback geometry.
        for entry in &entries {
            if !failed_window_ids.contains(&entry.window_id) {
                cache
                    .positions
                    .insert(entry.window_id, (entry.layout_rect, entry.visibility));
            }
        }
    }

    // Cloak off-screen windows AFTER positioning. DWM cloaking keeps the
    // composition surface alive (preventing content shift on return) while
    // hiding the window from view (preventing peeking through outer gaps).
    // Events from cloaking are filtered by is_placement_cloaked() in event_hooks.
    //
    // Routed through `apply_cloak_state` so a window that's also in
    // `GHOST_CLOAKED` stays cloaked even if we remove it from
    // `GLOBAL_CLOAKED` during pruning.
    sync_cloak_state(&entries, placements, &failed_window_ids);

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
    if !animation_frame && nudge_sticky_compositors {
        let nudge_targets: Vec<NudgeTarget> = entries
            .iter()
            .filter(|e| {
                e.visibility == Visibility::Visible
                    && e.w > 1
                    && !failed_window_ids.contains(&e.window_id)
            })
            .map(|e| NudgeTarget {
                hwnd: e.hwnd,
                x: e.x,
                y: e.y,
                w: e.w,
                h: e.h,
            })
            .collect();
        nudge_sticky_compositor_windows(&nudge_targets);
    }

    tracing::debug!(
        "Applied {} placements ({} skipped unchanged), {} region fallback(s), {} off-screen total",
        applied,
        skipped,
        region_fallbacks,
        offscreen_count,
    );

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

/// Outcome of entry building: the placements to apply, how many were skipped,
/// and the windows whose region must survive this pass because their HWND was
/// deliberately left where it is.
struct DeferEntryPlan {
    entries: Vec<DeferEntry>,
    skipped: u32,
    stale_region_window_ids: HashSet<WindowId>,
}

/// Build the defer-entry list for all placements, skipping cache-unchanged windows.
fn build_defer_entries(
    placements: &[WindowPlacement],
    region_clips: &[WindowRegionClip],
    cache: &mut Option<&mut PlacementCache>,
    animation_frame: bool,
    policy: AnimationPlacementPolicy,
    high_contrast: bool,
) -> DeferEntryPlan {
    let mut skipped = 0u32;
    let mut stale_region_window_ids: HashSet<WindowId> = HashSet::new();
    let mut entries: Vec<DeferEntry> = Vec::with_capacity(placements.len());

    for requested in placements {
        let region_clip = region_clips
            .iter()
            .find(|clip| clip.window_id == requested.window_id);
        let placement = requested.clone();
        let previous = cache
            .as_ref()
            .and_then(|cache| cache.positions.get(&placement.window_id).copied());
        // A requested clip is deliberately revalidated even when geometry is
        // unchanged, so an application-owned region replacement is detected.
        if region_clip.is_none() && previous == Some((placement.rect, placement.visibility)) {
            skipped += 1;
            continue;
        }
        let position_only = animation_move_is_position_only(previous, &placement);
        let Ok(hwnd) = window_id_to_hwnd(placement.window_id) else {
            continue;
        };
        unsafe {
            if !IsWindow(Some(hwnd)).as_bool() || IsIconic(hwnd).as_bool() {
                continue;
            }
            // Restore maximized tiled windows before positioning — WS_MAXIMIZE
            // causes some windows to ignore SetWindowPos size changes.
            // Only for tiled windows (column_index != MAX); floating windows
            // may be intentionally maximized by the user.
            if placement.visibility == Visibility::Visible
                && placement.column_index != usize::MAX
                && IsZoomed(hwnd).as_bool()
            {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
        }

        let region_managed = region_clip.is_some() || has_owned_window_region(placement.window_id);
        let dispatch = if animation_frame {
            let sensitive = region_managed
                || (policy == AnimationPlacementPolicy::AdaptiveCompositorSafe
                    && cached_compositor_sensitive(
                        hwnd,
                        placement.window_id,
                        cache.as_deref_mut(),
                    ));
            let hung = sensitive && unsafe { IsHungAppWindow(hwnd).as_bool() };
            if region_managed && !hung {
                // Keep SetWindowRgn and SetWindowPos ordered only for boundary
                // HWNDs. The normal in-monitor animation path remains async.
                AnimationDispatchMode::Synchronous
            } else {
                animation_dispatch_mode(policy, sensitive, hung)
            }
        } else {
            AnimationDispatchMode::Synchronous
        };
        if dispatch == AnimationDispatchMode::SkipHungSensitive {
            // Do not let a hung renderer pin the persistent animation worker.
            // Its cache entry stays unchanged, and the bounded exact landing
            // worker will retry it when the animation finishes. Because the HWND
            // does not move, any region it owns must stay installed — releasing
            // it would uncover the overflow at its current rectangle.
            if region_managed {
                stale_region_window_ids.insert(placement.window_id);
            }
            skipped += 1;
            continue;
        }

        let (inset_l, inset_t, inset_r, inset_b) = if high_contrast {
            (0, 0, 0, 0)
        } else {
            cached_border_insets(hwnd, placement.window_id, cache.as_deref_mut())
        };
        let frame_w = placement.rect.width + inset_l + inset_r;
        let frame_h = placement.rect.height + inset_t + inset_b;

        if placement.visibility == Visibility::Visible {
            let flags = visible_position_flags(animation_frame, dispatch, position_only);
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
                region_clip_bounds: region_clip.map(|clip| clip.clip_bounds),
                fallback_rect: region_clip.map(|clip| clip.fallback_rect),
                fallback_visibility: region_clip.map(|clip| clip.fallback_visibility),
            });
        } else {
            // Off-screen: SWP_NOSIZE keeps current size (no resize side-effects).
            // w stores estimated frame width for clamping only — SetWindowPos
            // ignores it due to SWP_NOSIZE.
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
                region_clip_bounds: region_clip.map(|clip| clip.clip_bounds),
                fallback_rect: region_clip.map(|clip| clip.fallback_rect),
                fallback_visibility: region_clip.map(|clip| clip.fallback_visibility),
            });
        }
    }

    DeferEntryPlan {
        entries,
        skipped,
        stale_region_window_ids,
    }
}

fn configure_entry_fallback(entry: &mut DeferEntry, animation_frame: bool) -> bool {
    let (Some(rect), Some(visibility)) = (entry.fallback_rect, entry.fallback_visibility) else {
        return false;
    };
    let (inset_l, inset_t, inset_r, inset_b) = entry.used_insets;
    entry.layout_rect = rect;
    entry.visibility = visibility;
    entry.region_clip_bounds = None;
    entry.x = rect.x.saturating_sub(inset_l);
    entry.y = rect.y.saturating_sub(inset_t);
    if visibility == Visibility::Visible {
        entry.w = rect.width.saturating_add(inset_l).saturating_add(inset_r);
        entry.h = rect.height.saturating_add(inset_t).saturating_add(inset_b);
        entry.flags = SWP_NOZORDER | SWP_NOACTIVATE;
        if !animation_frame {
            entry.flags |= SWP_FRAMECHANGED;
        }
    } else {
        entry.w = 0;
        entry.h = 0;
        entry.flags = SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE;
    }
    true
}

fn set_entry_to_fallback(entry: &mut DeferEntry, animation_frame: bool) -> bool {
    if !configure_entry_fallback(entry, animation_frame) {
        return false;
    }
    unsafe {
        SetWindowPos(
            entry.hwnd,
            None,
            entry.x,
            entry.y,
            entry.w,
            entry.h,
            entry.flags,
        )
        .is_ok()
    }
}

/// Install a bridge before uncloaking or moving. Unsupported application-owned
/// regions use the existing safe whole-window fallback before presentation.
fn prepare_entry_region_clips(
    entries: &mut [DeferEntry],
    failed_window_ids: &mut HashSet<u64>,
    animation_frame: bool,
) -> u32 {
    let mut fallback_count = 0;
    for entry in entries {
        let Some(clip_bounds) = entry.region_clip_bounds else {
            continue;
        };
        if entry.visibility != Visibility::Visible {
            // Only a visible placement carries usable frame geometry: a parked
            // entry is positioned with SWP_NOSIZE and keeps its real size, so a
            // region derived from its requested rectangle would be meaningless.
            // Dropping the plan hands it to `reconcile_window_regions`, which
            // releases the region only after the parking move has landed.
            entry.region_clip_bounds = None;
            continue;
        }
        let target_outer = Rect::new(entry.x, entry.y, entry.w.max(1), entry.h.max(1));
        let result = prepare_window_region_clip(
            entry.window_id,
            target_outer,
            entry.layout_rect,
            clip_bounds,
        );
        if result.succeeded() {
            continue;
        }

        // Move to the safe geometry now, before `uncloak_becoming_visible` can
        // reveal this HWND at a position Windows chose for it, and keep any
        // region already installed until then: releasing it here would leave the
        // window unclipped at its current boundary-crossing rectangle.
        // `reconcile_window_regions` performs the release once the batch has
        // landed, and only for windows whose move succeeded.
        fallback_count += 1;
        if !set_entry_to_fallback(entry, animation_frame) {
            failed_window_ids.insert(entry.window_id);
        }
    }
    fallback_count
}

/// Commit requested regions after the HWND batch lands. A rare ownership or
/// Win32 failure is converted immediately to the precomputed safe fallback so
/// no frame is allowed to leak into a neighboring monitor.
fn apply_entry_region_clips(
    entries: &mut [DeferEntry],
    failed_window_ids: &mut HashSet<u64>,
    animation_frame: bool,
) -> u32 {
    let mut fallback_count = 0;
    for entry in entries {
        let Some(clip_bounds) = entry.region_clip_bounds else {
            continue;
        };
        if failed_window_ids.contains(&entry.window_id) {
            continue;
        }
        if entry.visibility != Visibility::Visible {
            entry.region_clip_bounds = None;
            continue;
        }
        let outer_rect = Rect::new(entry.x, entry.y, entry.w.max(1), entry.h.max(1));
        let result = apply_window_region_clip(
            entry.window_id,
            outer_rect,
            entry.layout_rect,
            clip_bounds,
            !animation_frame,
        );
        if result.succeeded() {
            continue;
        }
        if animation_frame && has_owned_window_region(entry.window_id) {
            // Intermediate frame: the bridge installed before the move is safe at
            // this position by construction, so keep it and let the exact landing
            // pass reconcile the geometry once the application settles. Forcing
            // the fallback here would pop a column off-monitor mid-animation.
            continue;
        }

        fallback_count += 1;
        // Move to the safe rectangle first, then release the region. The
        // fallback is either contained by the owner work area or clear of every
        // monitor, so a still-restrictive region cannot strand it, while
        // releasing first would flash the unclipped overflow on the neighbor.
        // A failed fallback move deliberately keeps the region installed.
        if set_entry_to_fallback(entry, animation_frame) {
            let _ = restore_window_region(entry.window_id, !animation_frame);
        } else {
            failed_window_ids.insert(entry.window_id);
        }
    }
    fallback_count
}

/// Uncloak entries becoming visible and drop them from the tracking set.
fn uncloak_becoming_visible(entries: &[DeferEntry], failed_window_ids: &HashSet<WindowId>) {
    let _commit = lock_cloak_commit();
    let to_consider: Vec<WindowId> = {
        let mut cloaked = lock_cloaked();
        if let Some(ref mut set) = *cloaked {
            entries
                .iter()
                .filter(|e| {
                    e.visibility == Visibility::Visible
                        && !failed_window_ids.contains(&e.window_id)
                        && set.remove(&e.window_id)
                })
                .map(|e| e.window_id)
                .collect()
        } else {
            Vec::new()
        }
    };
    for wid in to_consider {
        let _ = apply_cloak_state_locked(wid);
    }
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
        if entry.column_index == usize::MAX
            || entry.visibility != Visibility::Visible
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
fn sync_cloak_state(
    entries: &[DeferEntry],
    placements: &[WindowPlacement],
    failed_window_ids: &HashSet<u64>,
) {
    let _commit = lock_cloak_commit();
    let (to_cloak, to_uncloak): (Vec<WindowId>, Vec<WindowId>) = {
        let mut cloaked = lock_cloaked();
        let set = cloaked.get_or_insert_with(HashSet::new);

        let cloak: Vec<WindowId> = entries
            .iter()
            .filter(|e| {
                !failed_window_ids.contains(&e.window_id)
                    && e.visibility != Visibility::Visible
                    && set.insert(e.window_id)
            })
            .map(|e| e.window_id)
            .collect();

        // Prune windows no longer in the layout (e.g., workspace switch).
        let current_ids: HashSet<u64> = placements.iter().map(|p| p.window_id).collect();
        let uncloak: Vec<WindowId> = set
            .iter()
            .filter(|id| !current_ids.contains(id))
            .copied()
            .collect();
        set.retain(|id| current_ids.contains(id));

        (cloak, uncloak)
    };
    for wid in to_cloak {
        let _ = apply_cloak_state_locked(wid);
    }
    for wid in to_uncloak {
        let _ = apply_cloak_state_locked(wid);
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
    hwnd: HWND,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

/// Send a (w-1 -> w) synchronous SetWindowPos pair to each known
/// compositor-sensitive window. The final restore also forces non-client
/// recalculation, then one DwmFlush publishes the repaired surfaces before the
/// landing is considered complete.
fn nudge_sticky_compositor_windows(targets: &[NudgeTarget]) {
    let mut repaired_any = false;
    for t in targets {
        unsafe {
            if !IsWindow(Some(t.hwnd)).as_bool() {
                continue;
            }
        }
        let class = window_class_name(t.hwnd);
        if !crate::thumbnail::is_compositor_sensitive_class_str(&class) {
            continue;
        }
        let flags = SWP_NOZORDER | SWP_NOACTIVATE;
        unsafe {
            if SetWindowPos(t.hwnd, None, t.x, t.y, t.w - 1, t.h, flags).is_err() {
                continue;
            }
            // Re-validate the HWND between the pair: the first SetWindowPos
            // pumps messages on the target thread and can cause the window to
            // be destroyed; the handle could be recycled for an unrelated
            // window before the restore call lands. Re-checking both the
            // handle validity and the class name catches recycling. If either
            // fails the target is left at w-1 rather than risk resizing the
            // wrong window — next apply pass will correct it.
            if !IsWindow(Some(t.hwnd)).as_bool() {
                continue;
            }
            if window_class_name(t.hwnd) != class {
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
                continue;
            }
        }
        repaired_any = true;
        tracing::debug!(
            "Refreshed compositor-sensitive window (class={}, hwnd={:?})",
            class,
            t.hwnd
        );
    }
    if repaired_any {
        unsafe {
            let _ = DwmFlush();
        }
    }
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

        // Insets = how much the frame rect extends beyond the visible area
        let left = extended_rect.left - frame_rect.left;
        let top = extended_rect.top - frame_rect.top;
        let right = frame_rect.right - extended_rect.right;
        let bottom = frame_rect.bottom - extended_rect.bottom;

        (left.max(0), top.max(0), right.max(0), bottom.max(0))
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
    fn test_direct_cloak_is_tracked_for_recovery() {
        // A directly-cloaked window (e.g. a stashed scratchpad) must be
        // tracked in DIRECT_CLOAKED so shutdown/panic recovery can restore
        // it; otherwise it would be permanently invisible. Uses a unique
        // wid so it won't collide with parallel tests touching the set.
        let wid: WindowId = 0x7FFF_FF01;
        dwm_cloak_window(wid);
        assert!(
            lock_direct_cloaked()
                .as_ref()
                .is_some_and(|s| s.contains(&wid)),
            "dwm_cloak_window must record the wid for recovery"
        );
        dwm_uncloak_window(wid);
        assert!(
            !lock_direct_cloaked()
                .as_ref()
                .is_some_and(|s| s.contains(&wid)),
            "dwm_uncloak_window must clear the recovery record"
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
    fn test_apply_placements_skips_invalid_windows() {
        let config = PlatformConfig::default();
        let placements = vec![WindowPlacement {
            window_id: 0,
            rect: Rect::new(0, 0, 800, 600),
            visibility: Visibility::OffScreenLeft,
            column_index: 0,
        }];

        // Invalid windows (hwnd 0) are silently skipped in the deferred batch
        let result = apply_placements(&placements, &config, None, false);
        assert!(result.is_ok());
    }

    /// Verifies the OR-cloak invariant by directly manipulating the two
    /// global sets and asserting `is_placement_cloaked` returns the OR.
    ///
    /// Uses a synthetic high-bit WindowId that won't collide with any
    /// real HWND on the test machine, since the tracking sets are
    /// process-global.
    #[test]
    fn test_or_cloak_invariant() {
        let wid: WindowId = 0xFFFF_FFFF_FFFF_FF00;

        // Snapshot any pre-existing state so we restore cleanly.
        let had_global_before = global_cloaked_contains(wid);
        let had_ghost_before = ghost_cloaked_contains(wid);

        // Case 1: neither set → false.
        {
            let mut g = lock_cloaked();
            if let Some(ref mut s) = *g {
                s.remove(&wid);
            }
        }
        {
            let mut g = lock_ghost_cloaked();
            if let Some(ref mut s) = *g {
                s.remove(&wid);
            }
        }
        assert!(!is_placement_cloaked(wid), "neither set should give false");

        // Case 2: global only → true.
        {
            let mut g = lock_cloaked();
            let s = g.get_or_insert_with(HashSet::new);
            s.insert(wid);
        }
        assert!(is_placement_cloaked(wid), "global only should give true");

        // Case 3: both sets → true.
        {
            let mut g = lock_ghost_cloaked();
            g.get_or_insert_with(HashSet::new).insert(wid);
        }
        assert!(is_placement_cloaked(wid), "both sets should give true");

        // Case 4: ghost only → true.
        {
            let mut g = lock_cloaked();
            if let Some(ref mut s) = *g {
                s.remove(&wid);
            }
        }
        assert!(is_placement_cloaked(wid), "ghost only should give true");

        // Case 5: neither → false again.
        {
            let mut g = lock_ghost_cloaked();
            if let Some(ref mut s) = *g {
                s.remove(&wid);
            }
        }
        assert!(
            !is_placement_cloaked(wid),
            "neither again should give false"
        );

        // Restore pre-existing state for whatever ran before this test.
        if had_global_before {
            let mut g = lock_cloaked();
            let s = g.get_or_insert_with(HashSet::new);
            s.insert(wid);
        }
        if had_ghost_before {
            let mut g = lock_ghost_cloaked();
            g.get_or_insert_with(HashSet::new).insert(wid);
        }
    }
}
