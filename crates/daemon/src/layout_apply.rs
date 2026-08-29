//! Layout application: apply_layout, its phase helpers, and animation frame dispatch.

use crate::animation_worker;
use crate::state::*;
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use tracing::{debug, warn};

const MAX_TIMEOUT_DIAGNOSTIC_CHARS: usize = 256;

/// An exact-viewport minimum is safe to persist only when the requested origin
/// was far enough from the work-area origin to distinguish a window that
/// honored placement from app-owned fullscreen. Share this predicate between
/// landing and animation propagation so their tolerance cannot drift.
pub(crate) fn viewport_equality_is_proven(
    position_matches: bool,
    requested_origin: i32,
    work_origin: i32,
) -> bool {
    position_matches
        && requested_origin.abs_diff(work_origin) > leopardwm_platform_win32::EDGE_EPSILON_PX as u32
}

/// Whether an unchanged desired layout may bypass the synchronous placement
/// worker. A completed animation must always perform one exact landing pass:
/// that pass drains `post_animation_landing_pending`, verifies all four DWM
/// edges, and only in legacy mode repairs sticky compositor surfaces.
pub(crate) fn can_skip_unchanged_layout(
    placements_unchanged: bool,
    bypass_fast_path: bool,
    post_animation_landing_pending: bool,
) -> bool {
    placements_unchanged && !bypass_fast_path && !post_animation_landing_pending
}

/// Tolerance for the fast-path containment check.
///
/// The authoritative outer rectangle includes the invisible resize border
/// (roughly 8-14px depending on DPI) and DPI rounding adds a pixel or two, while
/// an OS-driven relocation moves a window by hundreds of pixels.
pub(crate) const MONITOR_DRIFT_TOLERANCE_PX: i32 = 32;

/// First visible tiled placement whose real on-screen rectangle has left the
/// monitor that owns it and reaches another output.
///
/// `owner_of` maps a window to the monitor whose workspace owns it and
/// `actual_rect` returns its authoritative outer rectangle (`GetWindowRect`,
/// which stays correct even when DWM reports stale extended frame bounds). Both
/// are injected so the rule is testable without real window handles.
pub(crate) fn drifted_off_monitor_window(
    placements: &[leopardwm_core_layout::WindowPlacement],
    monitors: &std::collections::HashMap<
        leopardwm_platform_win32::MonitorId,
        leopardwm_platform_win32::MonitorInfo,
    >,
    owner_of: impl Fn(u64) -> Option<leopardwm_platform_win32::MonitorId>,
    actual_rect: impl Fn(u64) -> Option<leopardwm_core_layout::Rect>,
) -> Option<u64> {
    use leopardwm_core_layout::Visibility;

    placements
        .iter()
        .filter(|placement| {
            placement.visibility == Visibility::Visible && placement.column_index != usize::MAX
        })
        .find(|placement| {
            let Some(owner_id) = owner_of(placement.window_id) else {
                return false;
            };
            let Some(owner) = monitors.get(&owner_id) else {
                return false;
            };
            let Some(actual) = actual_rect(placement.window_id) else {
                return false;
            };
            // A leak is pixels that both reach a different output and sit
            // outside the owner. Testing the whole rectangle instead would flag
            // a mirrored output (whose rectangle overlaps the owner's) for a
            // window that merely overhangs into empty virtual-desktop space,
            // which paints nothing.
            monitors.iter().any(|(id, monitor)| {
                if *id == owner_id {
                    return false;
                }
                let Some(overlap) = intersection(actual, monitor.rect) else {
                    return false;
                };
                escapes_bounds(overlap, owner.rect, MONITOR_DRIFT_TOLERANCE_PX)
            })
        })
        .map(|placement| placement.window_id)
}

/// Positive-area overlap of two rectangles, if any.
fn intersection(
    left: leopardwm_core_layout::Rect,
    right: leopardwm_core_layout::Rect,
) -> Option<leopardwm_core_layout::Rect> {
    let x = left.x.max(right.x);
    let y = left.y.max(right.y);
    let width = left.right().min(right.right()).saturating_sub(x);
    let height = left.bottom().min(right.bottom()).saturating_sub(y);
    (width > 0 && height > 0).then(|| leopardwm_core_layout::Rect::new(x, y, width, height))
}

/// Whether positive pixels shared by `rect` and `neighbor` lie outside its
/// owner. Mirrored/overlapping outputs can intersect a wholly owner-contained
/// window; that is not monitor overflow.
fn overlaps_neighbor_outside_owner(
    rect: leopardwm_core_layout::Rect,
    neighbor: leopardwm_core_layout::Rect,
    owner: leopardwm_core_layout::Rect,
) -> bool {
    let Some(overlap) = intersection(rect, neighbor) else {
        return false;
    };
    let overlap_area = i64::from(overlap.width) * i64::from(overlap.height);
    let owner_area = intersection(overlap, owner)
        .map(|inside| i64::from(inside.width) * i64::from(inside.height))
        .unwrap_or(0);
    overlap_area > owner_area
}

/// Whether `rect` reaches more than `tolerance` past any edge of `bounds`.
fn escapes_bounds(
    rect: leopardwm_core_layout::Rect,
    bounds: leopardwm_core_layout::Rect,
    tolerance: i32,
) -> bool {
    rect.x < bounds.x - tolerance
        || rect.right() > bounds.right() + tolerance
        || rect.y < bounds.y - tolerance
        || rect.bottom() > bounds.bottom() + tolerance
}

pub(crate) fn legacy_compositor_nudge_required(
    landing_pending: bool,
    compositor_safe_mode: bool,
) -> bool {
    landing_pending && !compositor_safe_mode
}

fn bounded_timeout_diagnostic(value: String) -> Option<String> {
    if value.is_empty() {
        return None;
    }

    let mut chars = value.chars();
    let mut bounded: String = chars.by_ref().take(MAX_TIMEOUT_DIAGNOSTIC_CHARS).collect();
    if chars.next().is_some() {
        bounded.push('…');
    }
    Some(bounded)
}

pub(crate) fn run_layout_apply_recovery_pass(window_ids: &[u64], context: &str) {
    #[cfg(not(test))]
    run_visibility_recovery_pass(window_ids, context);
    #[cfg(test)]
    let _ = (window_ids, context);
}

/// Message sent back from the apply-layout worker thread.
type ApplyWorkerMsg = (
    Result<()>,
    Vec<leopardwm_platform_win32::WidthViolation>,
    Vec<leopardwm_platform_win32::HeightViolation>,
    Vec<u64>,
);

/// Keep tiled placements isolated to their owning monitor.
///
/// Non-focused tiled windows that intersect a neighboring output are parked
/// clear of every monitor. The active focused column is horizontally contained
/// by its work area instead, so focus navigation remains usable without leaking
/// pixels into another monitor. Floating windows may span monitors intentionally.
fn clamp_horizontally_inside(
    rect: leopardwm_core_layout::Rect,
    bounds: leopardwm_core_layout::Rect,
) -> leopardwm_core_layout::Rect {
    let width = rect.width.max(1).min(bounds.width.max(1));
    let max_x = bounds
        .x
        .saturating_add(bounds.width.max(1).saturating_sub(width));
    leopardwm_core_layout::Rect::new(rect.x.clamp(bounds.x, max_x), rect.y, width, rect.height)
}

pub(crate) fn park_offscreen_avoiding_neighbors(
    placements: &mut [leopardwm_core_layout::WindowPlacement],
    owner_id: leopardwm_platform_win32::MonitorId,
    focused_column: Option<usize>,
    monitors: &std::collections::HashMap<
        leopardwm_platform_win32::MonitorId,
        leopardwm_platform_win32::MonitorInfo,
    >,
    monitor_rects: &[leopardwm_core_layout::Rect],
) {
    use leopardwm_core_layout::Visibility;

    let Some(owner) = monitors.get(&owner_id) else {
        return;
    };
    let owner_rect = owner.rect;
    let intersects_neighbor = |rect: leopardwm_core_layout::Rect| {
        monitors.iter().any(|(id, monitor)| {
            *id != owner_id && overlaps_neighbor_outside_owner(rect, monitor.rect, owner_rect)
        })
    };

    for placement in placements {
        if !intersects_neighbor(placement.rect) {
            continue;
        }

        if placement.visibility == Visibility::Visible {
            let crosses_horizontal_edge =
                placement.rect.x < owner_rect.x || placement.rect.right() > owner_rect.right();
            let crosses_vertical_edge =
                placement.rect.y < owner_rect.y || placement.rect.bottom() > owner_rect.bottom();

            // Mirrored displays can report overlapping coordinates. A window
            // wholly inside its owner is valid even if another monitor overlaps.
            if placement.column_index == usize::MAX
                || (!crosses_horizontal_edge && !crosses_vertical_edge)
            {
                continue;
            }

            if focused_column == Some(placement.column_index) && crosses_horizontal_edge {
                placement.rect = clamp_horizontally_inside(placement.rect, owner.work_area);
                if !crosses_vertical_edge && !intersects_neighbor(placement.rect) {
                    continue;
                }
            }

            placement.visibility = if placement.rect.x < owner_rect.x {
                Visibility::OffScreenLeft
            } else {
                Visibility::OffScreenRight
            };
        }

        placement.rect = offscreen_park_rect(placement.rect, owner_rect, monitor_rects);
    }
}

/// Bounds of the on-owner edge strip, or `None` when the placement shares no
/// pixels with its owner monitor.
///
/// The strip is never reduced by windows that cover it. The preview host is
/// anchored directly below the bottommost visible tiled window, so anything
/// above the tiled band — including higher-integrity windows this process
/// cannot move — paints over the preview and keeps its own input. Subtracting
/// those rectangles instead cut the published preview down to the largest
/// uncovered run, which is exactly the truncated preview users saw whenever a
/// launcher or dialog sat over the edge strip.
fn preview_clip_bounds(
    placement_rect: leopardwm_core_layout::Rect,
    owner_rect: leopardwm_core_layout::Rect,
) -> Option<leopardwm_core_layout::Rect> {
    intersection(placement_rect, owner_rect)?;
    Some(owner_rect)
}

/// Window the preview host must sit directly below, in z-order.
///
/// Every window that owns pixels inside a published strip must stay above the
/// host, otherwise the preview paints over it and steals its input. Unmanaged
/// windows are not necessarily above the tiled band — a game launcher can sit
/// behind every tiled window and still cover an edge strip — so the anchor is
/// the deepest window that either covers a strip or is an on-screen tiled
/// window. Parked sources sit off every monitor and therefore never match.
pub(crate) fn preview_host_band_anchor(
    windows_top_to_bottom: &[leopardwm_platform_win32::WindowInfo],
    preview_strips: &[leopardwm_core_layout::Rect],
    visible_tiled_window_ids: &std::collections::HashSet<u64>,
) -> Option<u64> {
    windows_top_to_bottom
        .iter()
        .rev()
        .find(|window| {
            visible_tiled_window_ids.contains(&window.hwnd)
                || preview_strips
                    .iter()
                    .any(|strip| window.rect.intersects(strip))
        })
        .map(|window| window.hwnd)
}

/// Anchor for this pass, or `None` when no preview will be published or the
/// occluder snapshot could not be proven.
fn resolve_preview_host_below(
    occluders: Option<&[leopardwm_platform_win32::WindowInfo]>,
    preview_strips: &[leopardwm_core_layout::Rect],
    placements: &[leopardwm_core_layout::WindowPlacement],
) -> Option<u64> {
    let occluders = occluders?;
    if preview_strips.is_empty() {
        return None;
    }
    let visible_tiled_ids: std::collections::HashSet<u64> = placements
        .iter()
        .filter(|placement| {
            placement.column_index != usize::MAX
                && placement.visibility == leopardwm_core_layout::Visibility::Visible
        })
        .map(|placement| placement.window_id)
        .collect();
    preview_host_band_anchor(occluders, preview_strips, &visible_tiled_ids)
}

/// Z-ordered snapshot of windows that may own pixels over an edge preview.
///
/// `None` is fail-closed: without it the host's band position cannot be proven,
/// so previews are suppressed instead of being published over an unknown owner.
fn visible_occluder_snapshot() -> Option<Vec<leopardwm_platform_win32::WindowInfo>> {
    #[cfg(test)]
    {
        None
    }
    #[cfg(not(test))]
    {
        leopardwm_platform_win32::enumerate_visible_top_level_occluders().ok()
    }
}

/// Moving DWM preview pixels and a separately-pumped input HWND on every frame
/// cannot be atomic. During animation, park non-ghost edge sources at their
/// safe fallback and publish no persistent preview; the exact landing publishes
/// one settled pixel/input surface. Ghost clips remain because their source and
/// destination are owned by the animation worker and are not interactive.
fn suppress_persistent_previews_during_animation(
    placements: &mut [leopardwm_core_layout::WindowPlacement],
    clips: &mut Vec<leopardwm_platform_win32::WindowRegionClip>,
    ghosted: Option<&std::collections::HashSet<u64>>,
) {
    clips.retain(|clip| {
        if ghosted.is_some_and(|ids| ids.contains(&clip.window_id)) {
            return true;
        }
        if let Some(placement) = placements
            .iter_mut()
            .find(|placement| placement.window_id == clip.window_id)
        {
            placement.rect = clip.fallback_rect;
            placement.visibility = clip.fallback_visibility;
        }
        false
    });
}

fn upsert_region_clip(
    clips: &mut Vec<leopardwm_platform_win32::WindowRegionClip>,
    clip: leopardwm_platform_win32::WindowRegionClip,
) {
    if let Some(existing) = clips
        .iter_mut()
        .find(|existing| existing.window_id == clip.window_id)
    {
        *existing = clip;
    } else {
        clips.push(clip);
    }
}

/// Apply the configured multi-monitor overflow policy to one owner monitor.
/// `clip` preserves partial columns and emits a Win32 region plan; `hide` uses
/// the existing whole-window fallback directly. Fully off-screen placements
/// are always parked clear of every monitor.
/// Desktop geometry the overflow policy needs beyond the owner monitor itself.
pub(crate) struct OverflowContext<'a> {
    pub monitors: &'a std::collections::HashMap<
        leopardwm_platform_win32::MonitorId,
        leopardwm_platform_win32::MonitorInfo,
    >,
    /// Every monitor rectangle, used to park a window clear of all of them.
    pub monitor_rects: &'a [leopardwm_core_layout::Rect],
    /// Whether a z-ordered occluder snapshot was proven for this pass. Without
    /// it the host's band position cannot be verified, so no edge preview may
    /// be published.
    pub occluders_known: bool,
}

fn prepare_monitor_overflow(
    placements: &mut [leopardwm_core_layout::WindowPlacement],
    owner_id: leopardwm_platform_win32::MonitorId,
    focused_column: Option<usize>,
    mode: crate::config::MonitorOverflowModeConfig,
    desktop: &OverflowContext<'_>,
    region_clips: &mut Vec<leopardwm_platform_win32::WindowRegionClip>,
    preview_strips: &mut Vec<leopardwm_core_layout::Rect>,
) {
    let OverflowContext {
        monitors,
        monitor_rects,
        occluders_known,
    } = desktop;
    use crate::config::MonitorOverflowModeConfig;
    use leopardwm_core_layout::Visibility;

    if mode == MonitorOverflowModeConfig::Hide || !occluders_known {
        park_offscreen_avoiding_neighbors(
            placements,
            owner_id,
            focused_column,
            monitors,
            monitor_rects,
        );
        return;
    }

    let Some(owner) = monitors.get(&owner_id) else {
        return;
    };
    let owner_rect = owner.rect;
    let intersects_neighbor = |rect: leopardwm_core_layout::Rect| {
        monitors.iter().any(|(id, monitor)| {
            *id != owner_id && overlaps_neighbor_outside_owner(rect, monitor.rect, owner_rect)
        })
    };
    for placement in placements.iter_mut() {
        if placement.column_index == usize::MAX {
            continue;
        }
        if !intersects_neighbor(placement.rect) {
            continue;
        }

        if placement.visibility != Visibility::Visible {
            placement.rect = offscreen_park_rect(placement.rect, owner_rect, monitor_rects);
            continue;
        }

        let crosses_owner = placement.rect.x < owner_rect.x
            || placement.rect.right() > owner_rect.right()
            || placement.rect.y < owner_rect.y
            || placement.rect.bottom() > owner_rect.bottom();
        // Mirrored outputs can overlap in virtual coordinates. A tiled window
        // wholly inside its owner is valid even if another monitor overlaps it.
        if !crosses_owner {
            continue;
        }

        let (fallback_rect, fallback_visibility) = if focused_column == Some(placement.column_index)
        {
            (
                placement.rect.clamped_inside(owner.work_area),
                Visibility::Visible,
            )
        } else {
            let visibility = if placement.rect.x < owner_rect.x {
                Visibility::OffScreenLeft
            } else {
                Visibility::OffScreenRight
            };
            (
                offscreen_park_rect(placement.rect, owner_rect, monitor_rects),
                visibility,
            )
        };

        let Some(clip_bounds) = preview_clip_bounds(placement.rect, owner_rect) else {
            placement.rect = fallback_rect;
            placement.visibility = fallback_visibility;
            continue;
        };

        // A window region can only hide pixels; it cannot pull a window that
        // shares no pixels with its owner monitor back onto it. Clipping such a
        // placement would install an empty region, so the HWND would be
        // presented as an empty rectangle sitting on the neighbor's desktop
        // instead of window content. Apply the safe geometry immediately and
        // plan no region for it. Reachable whenever an interpolated frame, a
        // drag, or a display-topology change leaves a still-visible placement
        // completely outside its owner.
        if !placement.rect.intersects(&owner_rect) {
            placement.rect = fallback_rect;
            placement.visibility = fallback_visibility;
            continue;
        }

        if let Some(strip) = intersection(placement.rect, owner_rect) {
            preview_strips.push(strip);
        }
        upsert_region_clip(
            region_clips,
            leopardwm_platform_win32::WindowRegionClip {
                window_id: placement.window_id,
                clip_bounds,
                fallback_rect,
                fallback_visibility,
            },
        );
    }
}

/// Pick an off-screen rect for `window` that clears every monitor, tried along
/// the owning monitor's edges in order (below, above, right, left) and picking
/// the first that lands on no monitor. Falls back to the far sentinel only if
/// the owner is boxed in on all four sides.
fn offscreen_park_rect(
    window: leopardwm_core_layout::Rect,
    owner: leopardwm_core_layout::Rect,
    monitor_rects: &[leopardwm_core_layout::Rect],
) -> leopardwm_core_layout::Rect {
    use leopardwm_core_layout::Rect;
    const MARGIN: i32 = 4;
    let candidates = [
        Rect::new(
            owner.x,
            owner.y.saturating_add(owner.height).saturating_add(MARGIN),
            window.width,
            window.height,
        ),
        Rect::new(
            owner.x,
            owner.y.saturating_sub(window.height).saturating_sub(MARGIN),
            window.width,
            window.height,
        ),
        Rect::new(
            owner.x.saturating_add(owner.width).saturating_add(MARGIN),
            owner.y,
            window.width,
            window.height,
        ),
        Rect::new(
            owner.x.saturating_sub(window.width).saturating_sub(MARGIN),
            owner.y,
            window.width,
            window.height,
        ),
    ];
    for candidate in candidates {
        if !monitor_rects.iter().any(|m| candidate.intersects(m)) {
            return candidate;
        }
    }
    const SENTINEL: i32 = leopardwm_platform_win32::MOVE_OFFSCREEN_SENTINEL_COORD;
    Rect::new(SENTINEL, SENTINEL, window.width, window.height)
}

impl AppState {
    /// Record a short suppression window for moved/resized feedback generated by apply_layout().
    pub(crate) fn arm_moved_or_resized_suppression<I>(&mut self, window_ids: I)
    where
        I: IntoIterator<Item = u64>,
    {
        let now = std::time::Instant::now();
        self.moved_or_resized_suppression
            .retain(|_, deadline| *deadline > now);
        let deadline = now + MOVED_OR_RESIZED_SUPPRESSION_WINDOW;
        for hwnd in window_ids {
            self.moved_or_resized_suppression.insert(hwnd, deadline);
        }
    }

    /// Returns true when a moved/resized event should be ignored due to recent apply_layout().
    pub(crate) fn should_suppress_moved_or_resized(&mut self, hwnd: u64) -> bool {
        let now = std::time::Instant::now();
        match self.moved_or_resized_suppression.get(&hwnd).copied() {
            Some(deadline) if deadline > now => true,
            Some(_) => {
                self.moved_or_resized_suppression.remove(&hwnd);
                false
            }
            None => false,
        }
    }

    /// Join any finished timed-out apply workers so the pending list does not grow indefinitely.
    /// Returns the number of workers reaped in this pass.
    pub(crate) fn reap_finished_pending_apply_workers(&mut self) -> usize {
        if self.pending_apply_workers.is_empty() {
            return 0;
        }
        let mut still_running = Vec::with_capacity(self.pending_apply_workers.len());
        let mut reaped = 0usize;
        for handle in self.pending_apply_workers.drain(..) {
            if handle.is_finished() {
                let _ = handle.join();
                reaped += 1;
            } else {
                still_running.push(handle);
            }
        }
        self.pending_apply_workers = still_running;
        if reaped > 0
            && self.pending_apply_workers.is_empty()
            && self.pause_cleanup_after_pending_apply
        {
            // A timed-out worker may have entered Win32 before its epoch was
            // invalidated. Re-run the same paused cleanup after its final
            // recovery pass so late regions/previews/overlay state cannot
            // remain published.
            self.enter_paused_state("late layout worker completion");
            self.pause_cleanup_after_pending_apply = false;
            self.pending_apply_reap_scheduled = false;
        }
        reaped
    }

    /// Mark shutdown/revert in progress and take ownership of any timed-out apply workers.
    pub(crate) fn begin_shutdown_or_revert(&mut self) -> Vec<std::thread::JoinHandle<()>> {
        self.apply_worker_cancelled.store(true, Ordering::SeqCst);
        self.apply_epoch.fetch_add(1, Ordering::SeqCst);
        std::mem::take(&mut self.pending_apply_workers)
    }

    /// Connected monitors in deterministic placement-ownership order. A physical
    /// HWND can accidentally appear in two active workspace models; focused
    /// monitor wins, then numeric monitor id, so one batch never positions the
    /// same HWND twice or overwrites its clip nondeterministically.
    fn ordered_active_monitor_ids(&self) -> Vec<leopardwm_platform_win32::MonitorId> {
        let mut ids: Vec<_> = self
            .workspaces
            .keys()
            .copied()
            .filter(|monitor_id| self.monitors.contains_key(monitor_id))
            .collect();
        ids.sort_by_key(|monitor_id| (*monitor_id != self.focused_monitor, *monitor_id));
        ids
    }

    /// Compute animated placements and send them to the animation worker.
    ///
    /// Returns the exact dispatched epoch, or `None` when paused, empty, or
    /// adaptive safe mode collapsed into one exact synchronous landing.
    pub(crate) fn send_animation_frame(
        &mut self,
        worker: &animation_worker::AnimationWorkerHandle,
    ) -> Result<Option<u64>> {
        if self.paused || self.display_change_pending {
            return Ok(None);
        }
        // Gate after console-signal / shutdown latch so we never compute or
        // dispatch a frame that would re-park windows after restore.
        if self.apply_worker_cancelled.load(Ordering::SeqCst) {
            return Ok(None);
        }

        // Product-safe default: never emit a burst of asynchronous live-HWND
        // moves. DirectComposition and swap-chain applications can leave their
        // internal render surface at an intermediate transform even when the
        // outer HWND lands correctly. Collapse every animation source here so
        // hotkeys, mouse focus, workspace switches, drag/drop, and future event
        // paths all share the same guarantee.
        if self.settle_animations_for_compositor_safety() {
            self.apply_layout()?;
            self.sync_taskbar_buttons();
            let pending_sticky = self.pending_sticky_refocus.take();
            if !self.paused {
                self.sync_foreground_window();
                if let Some(window_id) = pending_sticky {
                    self.refocus_sticky_window(window_id);
                }
            }
            return Ok(None);
        }

        // Commit deferred min-size clears before the frame reads constraints,
        // matching the invariant maintained by apply_layout. Without this, a
        // composition change occurring during an active animation would leave
        // stale per-sibling constraints in effect for the remaining frames.
        for ws_vec in self.workspaces.values_mut() {
            for ws in ws_vec.iter_mut() {
                ws.commit_pending_min_size_clears();
            }
        }
        let mut all_placements = Vec::new();
        let mut region_clips = Vec::new();
        let monitor_rects: Vec<_> = self.monitors.values().map(|monitor| monitor.rect).collect();
        let mut owner_ranges = Vec::with_capacity(self.monitors.len());
        let mut seen_windows = std::collections::HashSet::new();
        for monitor_id in self.ordered_active_monitor_ids() {
            let idx = self.active_workspace_idx(monitor_id);
            if let Some(workspace) = self
                .workspaces
                .get(&monitor_id)
                .and_then(|workspaces| workspaces.get(idx))
            {
                let viewport = self.layout_viewport(monitor_id);
                let start = all_placements.len();
                for placement in workspace.compute_placements_animated(viewport) {
                    if seen_windows.insert(placement.window_id) {
                        all_placements.push(placement);
                    } else {
                        warn!(
                            "Ignoring duplicate active placement for HWND {:#x} on monitor {}",
                            placement.window_id, monitor_id
                        );
                    }
                }
                let focused_column =
                    (monitor_id == self.focused_monitor).then(|| workspace.focused_column_index());
                owner_ranges.push((monitor_id, focused_column, start, all_placements.len()));
            }
        }
        let base_placement_len = all_placements.len();
        if all_placements.is_empty()
            && self
                .layout_transition
                .as_ref()
                .is_none_or(|transition| transition.exit_rects.is_empty())
        {
            return Ok(None);
        }

        // Interpolate first, then enforce monitor isolation against the actual
        // frame rectangles. Doing this earlier lets a transition move a visible
        // tiled window into a neighboring monitor after the safety check.
        if let Some(ref transition) = self.layout_transition {
            Self::apply_transition_interpolation(transition, &mut all_placements);
        }
        // Band anchor for this pass: the host is published below every window
        // that owns pixels inside a strip, so those owners keep pixels and input.
        let occluders = visible_occluder_snapshot();
        let desktop = OverflowContext {
            monitors: &self.monitors,
            monitor_rects: &monitor_rects,
            occluders_known: occluders.is_some(),
        };
        let mut preview_strips = Vec::new();
        for (owner_id, focused_column, start, end) in owner_ranges {
            prepare_monitor_overflow(
                &mut all_placements[start..end],
                owner_id,
                focused_column,
                self.config.layout.monitor_overflow,
                &desktop,
                &mut region_clips,
                &mut preview_strips,
            );
        }
        self.preview_host_below =
            resolve_preview_host_below(occluders.as_deref(), &preview_strips, &all_placements);
        // `apply_transition_interpolation` appends exiting windows. They are
        // few, so resolve only those owners rather than allocating a per-frame
        // HWND-to-monitor map for every placement.
        for placement in &mut all_placements[base_placement_len..] {
            if let Some((owner_id, _)) = self.find_window_workspace(placement.window_id) {
                // Exiting windows are not the active focused column for
                // this frame; never pin an old workspace back on-screen.
                prepare_monitor_overflow(
                    std::slice::from_mut(placement),
                    owner_id,
                    None,
                    // Exiting workspaces cannot own an interactive preview: by
                    // the time a release is dispatched their identity is inactive.
                    // Park tiled exits as soon as they reach another monitor;
                    // floating exits retain their sentinel and are left alone.
                    crate::config::MonitorOverflowModeConfig::Hide,
                    &desktop,
                    &mut region_clips,
                    &mut preview_strips,
                );
            }
        }

        // Filter out the dragged window and placeholder so SetWindowPos doesn't
        // fight the OS drag or try to position the sentinel.
        if let Some(ref drag) = self.drag_state {
            if drag.is_tiled {
                all_placements.retain(|p| {
                    p.window_id != drag.hwnd && p.window_id != crate::state::DRAG_PLACEHOLDER_HWND
                });
            }
        }

        suppress_persistent_previews_during_animation(
            &mut all_placements,
            &mut region_clips,
            self.layout_transition
                .as_ref()
                .map(|transition| &transition.ghosted_wids),
        );

        self.arm_moved_or_resized_suppression(all_placements.iter().map(|p| p.window_id));
        self.applying_layout = true;

        // Partition into live placements + ghost-thumbnail updates. Ghost
        // wids are excluded from `placements` so the worker doesn't fire
        // per-frame SetWindowPos on the cloaked source HWND.
        let (live_placements, ghost_updates) = Self::partition_for_animation(
            all_placements,
            self.layout_transition.as_ref(),
            &self.ghost_handles,
            &region_clips,
        );

        let mut platform_config = self.platform_config.clone();
        platform_config.preview_lifecycle_epoch =
            leopardwm_platform_win32::thumbnail::preview_lifecycle_epoch();
        platform_config.preview_host_below = self.preview_host_below.map(|hwnd| hwnd as isize);
        platform_config.animation_placement_policy = if self.config.behavior.compositor_safe_mode {
            leopardwm_platform_win32::AnimationPlacementPolicy::AdaptiveCompositorSafe
        } else {
            leopardwm_platform_win32::AnimationPlacementPolicy::LegacyAsync
        };
        let frame_epoch = self.apply_epoch.fetch_add(1, Ordering::SeqCst) + 1;
        let expected_identities = live_placements
            .iter()
            .filter_map(|placement| {
                self.window_incarnations
                    .get(&placement.window_id)
                    .map(|identity| (placement.window_id, identity.to_platform()))
            })
            .collect();
        let request = animation_worker::FrameRequest {
            apply_epoch: frame_epoch,
            placements: live_placements,
            expected_identities,
            region_clips,
            ghost_updates,
            platform_config,
        };
        if let Err(e) = worker.send_frame(request) {
            self.applying_layout = false;
            return Err(anyhow::anyhow!(e));
        }
        // Reposition the border immediately — both the worker's
        // SetWindowPos calls below and this border SetWindowPos commit
        // before the next DwmFlush vsync, so the border arrives on screen
        // in the same frame as the windows. Without this the FIRST
        // animation frame paints with the stale border position, since
        // the post-frame `tick + show_border` reorder in `main.rs` only
        // fires for frames 2+ (frame 1's `AnimationFrameApplied` arrives
        // AFTER frame 1 has already been presented).
        if let Some(hwnd) = self.previous_focused_hwnd {
            if self.config.appearance.active_border {
                self.show_border(hwnd);
            }
        }
        self.update_tab_strip();
        Ok(Some(frame_epoch))
    }

    fn collect_layout_apply_timeout_candidates(
        &self,
        window_ids: &[u64],
    ) -> Vec<LayoutApplyTimeoutCandidate> {
        let mut executable_by_pid: HashMap<u32, Option<String>> = HashMap::new();

        window_ids
            .iter()
            .map(|&hwnd| {
                let Some(info) = self.lookup_window_info(hwnd) else {
                    return LayoutApplyTimeoutCandidate {
                        hwnd,
                        class_name: None,
                        title: None,
                        executable: None,
                    };
                };
                let executable = executable_by_pid
                    .entry(info.process_id)
                    .or_insert_with(|| {
                        leopardwm_platform_win32::get_process_executable(info.process_id)
                    })
                    .clone()
                    .and_then(bounded_timeout_diagnostic);

                LayoutApplyTimeoutCandidate {
                    hwnd,
                    class_name: bounded_timeout_diagnostic(info.class_name),
                    title: bounded_timeout_diagnostic(info.title),
                    executable,
                }
            })
            .collect()
    }

    /// Recalculate layout and apply placements for all monitors.
    /// Uses animated offsets if any workspace has an active animation.
    /// No-op when tiling is paused.
    pub(crate) fn apply_layout(&mut self) -> Result<()> {
        let reaped_workers = self.reap_finished_pending_apply_workers();
        if reaped_workers > 0 {
            let managed_window_ids = self.all_managed_window_ids();
            run_layout_apply_recovery_pass(&managed_window_ids, "late-apply-worker");
        }

        if self.paused {
            return Ok(());
        }
        // During layout transitions, the animation worker drives positioning.
        if self.layout_transition.is_some() {
            return Ok(());
        }
        if self.apply_worker_cancelled.load(Ordering::SeqCst) {
            return Err(anyhow!(
                "Layout application skipped: shutdown/revert cleanup is in progress"
            ));
        }
        if !self.pending_apply_workers.is_empty() {
            return Err(anyhow!(
                "Layout application skipped: previous timed-out apply worker is still finishing"
            ));
        }
        // Main-loop state is newer than every frame already queued. Drain that
        // queue before starting a separate exact worker; otherwise an older frame
        // can acquire the platform transaction after this landing and overwrite
        // its window, thumbnail and input state.
        if self
            .animation_worker_control
            .as_ref()
            .is_some_and(|worker| !worker.wait_for_barrier(std::time::Duration::from_millis(750)))
        {
            return Err(anyhow!(
                "Layout application skipped: animation worker barrier timed out"
            ));
        }
        self.applying_layout = true;

        // Commit any deferred min-size constraint clears scheduled by
        // composition changes (add_window / insert_at / remove_window). Done
        // here rather than eagerly at the mutation site so that a timed-out /
        // paused apply path cannot leave constraints cleared indefinitely.
        for ws_vec in self.workspaces.values_mut() {
            for ws in ws_vec.iter_mut() {
                ws.commit_pending_min_size_clears();
            }
        }

        // Safety net for scroll invariant #4: a structural change that shrank
        // the strip can leave the active workspace scrolled past its content,
        // which renders as the leftmost column clipped off-screen with blank
        // desktop at the right edge. Re-clamp each monitor's active workspace
        // before placement so a stale over-scroll can't reach the screen.
        let active_workspace = &self.active_workspace;
        let monitors = &self.monitors;
        for (monitor_id, ws_vec) in &mut self.workspaces {
            let viewport_width = monitors
                .get(monitor_id)
                .map(|monitor| monitor.work_area.width)
                .unwrap_or(FALLBACK_VIEWPORT_WIDTH);
            let active_idx = active_workspace.get(monitor_id).copied().unwrap_or(0);
            if let Some(workspace) = ws_vec.get_mut(active_idx) {
                workspace.clamp_scroll_to_bounds(viewport_width);
            }
        }

        let (mut all_placements, region_clips, preview_host_below) =
            self.collect_apply_placements();
        self.preview_host_below = preview_host_below;

        // Interpolate layout transitions (structural changes like move/expel).
        if let Some(ref transition) = self.layout_transition {
            Self::apply_transition_interpolation(transition, &mut all_placements);
        }

        // Filter out the dragged window and placeholder so SetWindowPos doesn't
        // fight the OS drag or try to position the sentinel.
        if let Some(ref drag) = self.drag_state {
            if drag.is_tiled {
                all_placements.retain(|p| {
                    p.window_id != drag.hwnd && p.window_id != crate::state::DRAG_PLACEHOLDER_HWND
                });
            }
        }

        // Fast path: if every placement matches the last applied rect (and
        // the visible-set is unchanged), there is nothing to do. Spawning
        // the worker thread, the BeginDeferWindowPos batch, the DwmFlush,
        // size-violation queries, and the sticky-compositor nudge each take
        // tens of milliseconds; under rapid focus presses within the
        // already-visible range no scroll animation starts (so the caller
        // does not get to bypass us via `is_animating()`) and these calls
        // serialize on the daemon mutex, leaving focus events draining for
        // seconds after the user stops pressing. Returning early lets the
        // event loop catch up at near-memory speed.
        // Region ownership is verified for every clipped landing. Do not let
        // an unchanged rectangle bypass recovery after an app replaces a region.
        let placements_unchanged =
            region_clips.is_empty() && self.placements_match_last_applied(&all_placements);
        // Tests that inject worker behavior need the worker to actually
        // run, so they opt out of the fast path.
        #[cfg(test)]
        let bypass_fast_path = self.injected_apply_placements_behavior.is_some();
        #[cfg(not(test))]
        let bypass_fast_path = false;
        let identities_current = all_placements.iter().all(|placement| {
            self.window_incarnations
                .get(&placement.window_id)
                .map(crate::events::WindowIncarnation::to_platform)
                .as_ref()
                == leopardwm_platform_win32::current_window_event_identity(placement.window_id)
                    .as_ref()
        });
        if !identities_current {
            for placement in &all_placements {
                self.last_placed_layout_rects.remove(&placement.window_id);
            }
        }
        if identities_current
            && can_skip_unchanged_layout(
                placements_unchanged,
                bypass_fast_path,
                self.post_animation_landing_pending,
            )
        {
            // "Unchanged" only describes what this daemon last *requested*.
            // Windows relocates managed windows on its own (display topology
            // changes, session unlock, RDP reconnect, resume from sleep) and that
            // feedback arrives inside the apply-layout suppression window, so a
            // window can be sitting on a neighboring monitor while the desired
            // layout still matches the cache. Verify containment before skipping
            // placement, otherwise nothing ever reclaims it.
            match self.drifted_off_monitor_window(&all_placements) {
                None => {
                    self.applying_layout = false;
                    return Ok(());
                }
                Some(window_id) => {
                    debug!(
                        "Re-placing unchanged layout: window {:#x} drifted off its monitor",
                        window_id
                    );
                    self.last_placed_layout_rects.remove(&window_id);
                }
            }
        }
        if placements_unchanged && self.post_animation_landing_pending {
            debug!("Forcing exact post-animation landing for unchanged placements");
        }

        let timeout_candidate_ids: Vec<u64> = all_placements
            .iter()
            .map(|placement| placement.window_id)
            .collect();

        self.arm_moved_or_resized_suppression(all_placements.iter().map(|p| p.window_id));

        self.record_last_placed_rects(&all_placements);

        // `spawn_apply_worker` consumes this flag only after a successful
        // thread spawn. Remember the request so any later placement failure or
        // timeout can re-arm the compositor repair for the next exact landing.
        let landing_repair_requested = self.post_animation_landing_pending;
        let timeout = self.layout_apply_timeout;
        let (rx, worker_handle) = match self.spawn_apply_worker(all_placements, region_clips) {
            Ok(worker) => worker,
            Err(error) => {
                self.last_placed_layout_rects.clear();
                self.moved_or_resized_suppression.clear();
                self.applying_layout = false;
                return Err(error);
            }
        };

        let result = match rx.recv_timeout(timeout) {
            Ok((result, width_violations, height_violations, geometry_mismatches)) => {
                let _ = worker_handle.join();
                if result.is_err() {
                    self.moved_or_resized_suppression.clear();
                } else {
                    // Start the quiet window at physical completion, not worker
                    // spawn. A slow synchronous landing can outlive the original
                    // deadline and leave its own queued LOCATIONCHANGE events
                    // eligible to trigger a redundant corrective apply.
                    self.arm_moved_or_resized_suppression(timeout_candidate_ids.iter().copied());
                }
                let constraints_changed = if result.is_ok() {
                    self.propagate_size_violations(&width_violations, &height_violations)
                } else {
                    false
                };
                let geometry_changed = result.is_ok() && !geometry_mismatches.is_empty();
                // A geometry-only correction has the same desired layout
                // rectangles, so evict just the mismatched HWNDs before the
                // guarded re-apply; otherwise the fast-path would skip the
                // SetWindowPos that needs to use freshly queried insets.
                if geometry_changed {
                    for hwnd in &geometry_mismatches {
                        self.last_placed_layout_rects.remove(hwnd);
                    }
                    debug!(
                        "Retrying {} tiled window(s) whose visible frame missed a requested edge",
                        geometry_mismatches.len()
                    );
                }
                // If a constraint was added or visible geometry missed an edge,
                // run a single guarded re-apply so the corrected layout lands on
                // the current frame instead of waiting for another user event.
                // The guard prevents an uncooperative app from recursing forever.
                if geometry_changed && self.reapplying_after_violation {
                    Err(anyhow!(
                        "Visible window(s) did not reach verified layout geometry after retry: {:?}",
                        geometry_mismatches
                    ))
                } else if (constraints_changed || geometry_changed)
                    && !self.reapplying_after_violation
                {
                    self.reapplying_after_violation = true;
                    self.applying_layout = false;
                    let reapply = self.apply_layout();
                    self.reapplying_after_violation = false;
                    if let Err(e) = reapply {
                        warn!("Re-apply after size-violation propagation failed: {}", e);
                        Err(e)
                    } else {
                        result
                    }
                } else {
                    result
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Fence both queued and already-returning worker paths before
                // releasing paused desktop ownership. A worker that is already
                // inside Win32 checks this epoch again on return and performs
                // recovery; its owned handle triggers a second paused cleanup
                // when reaped.
                self.apply_epoch.fetch_add(1, Ordering::SeqCst);
                self.pending_apply_workers.push(worker_handle);
                self.pause_cleanup_after_pending_apply = true;
                self.enter_paused_state("layout application timeout");
                self.moved_or_resized_suppression.clear();
                let msg = layout_apply_timeout_message(timeout);
                let report = LayoutApplyTimeoutReport {
                    timeout,
                    candidates: self
                        .collect_layout_apply_timeout_candidates(&timeout_candidate_ids),
                };
                warn!(
                    "{} Timed-out placement batch contained {} candidate window(s); batch membership does not prove which window blocked placement.",
                    msg,
                    report.candidates.len()
                );
                for candidate in &report.candidates {
                    warn!(
                        "Timed-out placement batch candidate (not a proven blocker): hwnd={:#x} class={:?} title={:?} executable={:?}",
                        candidate.hwnd,
                        candidate.class_name,
                        candidate.title,
                        candidate.executable
                    );
                }
                self.pending_layout_apply_timeout_report = Some(report);
                let managed_window_ids = self.all_managed_window_ids();
                run_layout_apply_recovery_pass(&managed_window_ids, "apply-timeout");
                Err(anyhow!(msg))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                let _ = worker_handle.join();
                self.moved_or_resized_suppression.clear();
                Err(anyhow!(
                    "Layout worker thread exited without returning a result"
                ))
            }
        };
        if result.is_err() {
            if landing_repair_requested {
                self.post_animation_landing_pending = true;
            }
            // Desired rectangles are recorded before the worker runs so the
            // normal success path can use the fast cache immediately. A
            // failed or timed-out batch did not reliably apply them, so an
            // identical next layout must retry instead of returning early.
            self.last_placed_layout_rects.clear();
        }
        self.applying_layout = false;

        // Reposition border to track the focused window after layout changes
        if result.is_ok() {
            self.finalize_layout_success();
        }

        result
    }

    /// Collect animated placements for every monitor's active workspace, with debug logging.
    fn collect_apply_placements(
        &self,
    ) -> (
        Vec<leopardwm_core_layout::WindowPlacement>,
        Vec<leopardwm_platform_win32::WindowRegionClip>,
        Option<u64>,
    ) {
        let mut all_placements = Vec::new();
        let mut region_clips = Vec::new();
        // Reuse one monitor-rect snapshot for every owner monitor in this
        // batch instead of allocating it again per monitor.
        let monitor_rects: Vec<_> = self.monitors.values().map(|monitor| monitor.rect).collect();
        // Overflow is prepared in a second pass so every owner sees the same
        // floating snapshot: a floating window may span monitors, so a float
        // owned elsewhere can still cover this monitor's edge strip.
        let mut owner_ranges = Vec::with_capacity(self.monitors.len());

        let mut seen_windows = std::collections::HashSet::new();
        for monitor_id in self.ordered_active_monitor_ids() {
            let idx = self.active_workspace_idx(monitor_id);
            if let Some(workspace) = self
                .workspaces
                .get(&monitor_id)
                .and_then(|workspaces| workspaces.get(idx))
            {
                // Use animated placements to support smooth scrolling.
                let viewport = self.layout_viewport(monitor_id);
                let mut placements = workspace.compute_placements_animated(viewport);
                placements.retain(|placement| {
                    if seen_windows.insert(placement.window_id) {
                        true
                    } else {
                        warn!(
                            "Ignoring duplicate active placement for HWND {:#x} on monitor {}",
                            placement.window_id, monitor_id
                        );
                        false
                    }
                });
                let focused_column =
                    (monitor_id == self.focused_monitor).then(|| workspace.focused_column_index());
                let start = all_placements.len();
                debug!(
                    "Monitor {}: {} placements for viewport {}x{} (animating: {}, scroll: {:.1}, minimized: {})",
                    monitor_id,
                    placements.len(),
                    viewport.width,
                    viewport.height,
                    workspace.is_animating(),
                    workspace.effective_scroll_offset(),
                    workspace.minimized_count()
                );
                for placement in &placements {
                    if placement.visibility == leopardwm_core_layout::Visibility::Visible {
                        debug!(
                            "  placement hwnd={:#x} col={} rect=({},{} {}x{}) vis={:?}",
                            placement.window_id,
                            placement.column_index,
                            placement.rect.x,
                            placement.rect.y,
                            placement.rect.width,
                            placement.rect.height,
                            placement.visibility,
                        );
                    }
                }
                all_placements.extend(placements);
                owner_ranges.push((monitor_id, focused_column, start, all_placements.len()));
            }
        }

        let occluders = visible_occluder_snapshot();
        let desktop = OverflowContext {
            monitors: &self.monitors,
            monitor_rects: &monitor_rects,
            occluders_known: occluders.is_some(),
        };
        let mut preview_strips = Vec::new();
        for (owner_id, focused_column, start, end) in owner_ranges {
            prepare_monitor_overflow(
                &mut all_placements[start..end],
                owner_id,
                focused_column,
                self.config.layout.monitor_overflow,
                &desktop,
                &mut region_clips,
                &mut preview_strips,
            );
        }
        let preview_host_below =
            resolve_preview_host_below(occluders.as_deref(), &preview_strips, &all_placements);

        (all_placements, region_clips, preview_host_below)
    }

    /// Fast-path guard: the first visible tiled window that is physically off
    /// its owning monitor and overlapping another output. Skipped under
    /// `cfg(test)`, where placeholder hwnds have no real geometry.
    fn drifted_off_monitor_window(
        &self,
        all_placements: &[leopardwm_core_layout::WindowPlacement],
    ) -> Option<u64> {
        #[cfg(test)]
        {
            let _ = all_placements;
            None
        }
        #[cfg(not(test))]
        drifted_off_monitor_window(
            all_placements,
            &self.monitors,
            |window_id| {
                self.find_window_workspace(window_id)
                    .map(|(monitor_id, _)| monitor_id)
            },
            leopardwm_platform_win32::get_window_chrome_rect,
        )
    }

    /// Fast-path check: every placement matches the last applied rect and the visible-set is unchanged.
    pub(crate) fn placements_match_last_applied(
        &self,
        all_placements: &[leopardwm_core_layout::WindowPlacement],
    ) -> bool {
        all_placements.iter().all(|p| {
            let expected = self.last_placed_layout_rects.get(&p.window_id);
            match p.visibility {
                leopardwm_core_layout::Visibility::Visible => expected == Some(&p.rect),
                _ => expected.is_none(),
            }
        }) && {
            let current_ids: std::collections::HashSet<u64> =
                all_placements.iter().map(|p| p.window_id).collect();
            !self
                .last_placed_layout_rects
                .keys()
                .any(|id| !current_ids.contains(id))
        }
    }

    /// Record layout rects for visible placements and drain stale entries.
    fn record_last_placed_rects(
        &mut self,
        all_placements: &[leopardwm_core_layout::WindowPlacement],
    ) {
        // Record the layout rect the engine chose for each window so the
        // MovedOrResized handler can short-circuit false-positive snap-backs
        // when Windows fires EVENT_OBJECT_LOCATIONCHANGE without an actual
        // position change. Visibility::Visible placements only — off-screen
        // parked windows aren't at their "layout" rect by design.
        //
        // Drain stale entries for windows that are no longer in the active
        // layout (workspace-switch leaves their hwnds alive but they stop
        // appearing in `all_placements`, which `apply_layout` builds from
        // active workspaces only). Without this drain the fast-path's
        // "no extra entries" guard fails forever after the first
        // Ctrl+Alt+1-9 — silently nullifying the rapid-Ctrl+Alt+Right/Left
        // perf fix for every multi-workspace user.
        let active_ids: std::collections::HashSet<u64> =
            all_placements.iter().map(|p| p.window_id).collect();
        self.last_placed_layout_rects
            .retain(|id, _| active_ids.contains(id));
        for p in all_placements {
            if matches!(p.visibility, leopardwm_core_layout::Visibility::Visible) {
                self.last_placed_layout_rects.insert(p.window_id, p.rect);
            } else {
                self.last_placed_layout_rects.remove(&p.window_id);
            }
        }
    }

    /// Spawn the apply-layout worker thread; returns its result channel and join handle.
    fn spawn_apply_worker(
        &mut self,
        all_placements: Vec<leopardwm_core_layout::WindowPlacement>,
        region_clips: Vec<leopardwm_platform_win32::WindowRegionClip>,
    ) -> Result<(
        std::sync::mpsc::Receiver<ApplyWorkerMsg>,
        std::thread::JoinHandle<()>,
    )> {
        let mut platform_config = self.platform_config.clone();
        platform_config.preview_lifecycle_epoch =
            leopardwm_platform_win32::thumbnail::preview_lifecycle_epoch();
        platform_config.preview_host_below = self.preview_host_below.map(|hwnd| hwnd as isize);
        let apply_worker_cancelled = self.apply_worker_cancelled.clone();
        let apply_epoch_ref = self.apply_epoch.clone();
        let apply_epoch = apply_epoch_ref.fetch_add(1, Ordering::SeqCst) + 1;
        let apply_window_ids: Vec<u64> = all_placements.iter().map(|p| p.window_id).collect();
        let expected_identities: std::collections::HashMap<_, _> = all_placements
            .iter()
            .filter_map(|placement| {
                self.window_incarnations
                    .get(&placement.window_id)
                    .map(|identity| (placement.window_id, identity.to_platform()))
            })
            .collect();
        // Adaptive frames are serialized for sensitive renderers, so their
        // exact landing needs no synthetic resize. Preserve the historical
        // `(w-1 → w)` repair only for the explicitly selected legacy async path.
        // Consume the landing flag only after worker creation succeeds.
        let post_animation_nudge = legacy_compositor_nudge_required(
            self.post_animation_landing_pending,
            self.config.behavior.compositor_safe_mode,
        );
        #[cfg(test)]
        let injected_behavior = self.injected_apply_placements_behavior;
        #[cfg(test)]
        let late_worker_recovery_count = self.late_worker_recovery_count.clone();

        #[cfg(test)]
        if matches!(
            injected_behavior,
            Some(TestApplyPlacementsBehavior::FailWorkerSpawn)
        ) {
            self.applying_layout = false;
            return Err(anyhow!("injected apply worker spawn failure"));
        }

        let (tx, rx) = std::sync::mpsc::channel::<ApplyWorkerMsg>();
        let spawn_result = std::thread::Builder::new()
            .name("leopardwm-apply-layout".to_string())
            .spawn(move || {
                let should_cancel = || {
                    apply_worker_cancelled.load(Ordering::SeqCst)
                        || apply_epoch_ref.load(Ordering::SeqCst) != apply_epoch
                };
                if should_cancel() {
                    let _ = tx.send((Ok(()), Vec::new(), Vec::new(), Vec::new()));
                    return;
                }

                #[cfg(test)]
                if let Some(behavior) = injected_behavior {
                    let result = match behavior {
                        TestApplyPlacementsBehavior::SleepAndSucceed(delay) => {
                            std::thread::sleep(delay);
                            Ok(())
                        }
                        TestApplyPlacementsBehavior::SleepAndFail(delay) => {
                            std::thread::sleep(delay);
                            Err(anyhow!("injected apply_placements failure"))
                        }
                        TestApplyPlacementsBehavior::FailWorkerSpawn => unreachable!(
                            "spawn-failure injection returns before creating the worker"
                        ),
                    };
                    if should_cancel() {
                        run_layout_apply_recovery_pass(
                            &apply_window_ids,
                            "apply-cancelled-late-worker",
                        );
                        #[cfg(test)]
                        late_worker_recovery_count.fetch_add(1, Ordering::SeqCst);
                        let _ = tx.send((Ok(()), Vec::new(), Vec::new(), Vec::new()));
                        return;
                    }
                    let _ = tx.send((result, Vec::new(), Vec::new(), Vec::new()));
                    return;
                }

                if should_cancel() {
                    let _ = tx.send((Ok(()), Vec::new(), Vec::new(), Vec::new()));
                    return;
                }
                let (result, width_violations, height_violations, geometry_mismatches) =
                    match leopardwm_platform_win32::apply_placements_with_regions_fenced(
                        &all_placements,
                        &region_clips,
                        &expected_identities,
                        &platform_config,
                        None,
                        post_animation_nudge,
                    ) {
                        Ok(r) => (
                            Ok(()),
                            r.width_violations,
                            r.height_violations,
                            r.geometry_mismatches,
                        ),
                        Err(e) => (
                            Err(anyhow!(e.to_string())),
                            Vec::new(),
                            Vec::new(),
                            Vec::new(),
                        ),
                    };
                if should_cancel() {
                    run_layout_apply_recovery_pass(
                        &apply_window_ids,
                        "apply-cancelled-late-worker",
                    );
                    #[cfg(test)]
                    late_worker_recovery_count.fetch_add(1, Ordering::SeqCst);
                    let _ = tx.send((Ok(()), Vec::new(), Vec::new(), Vec::new()));
                    return;
                }
                let _ = tx.send((
                    result,
                    width_violations,
                    height_violations,
                    geometry_mismatches,
                ));
            });

        match spawn_result {
            Ok(handle) => {
                self.post_animation_landing_pending = false;
                Ok((rx, handle))
            }
            Err(e) => {
                self.applying_layout = false;
                Err(anyhow!("Failed to spawn layout worker thread: {}", e))
            }
        }
    }

    /// Feed worker-reported size violations back to the layout engine; returns whether constraints changed.
    pub(crate) fn propagate_size_violations(
        &mut self,
        width_violations: &[leopardwm_platform_win32::WidthViolation],
        height_violations: &[leopardwm_platform_win32::HeightViolation],
    ) -> bool {
        // Values larger than the work area are physically impossible. At
        // exact equality, accept only windows that honored a requested axis
        // origin different from the work-area origin. Otherwise fullscreen and
        // a real minimum are indistinguishable—and already fit without clipping.
        let mut constraints_changed = false;
        let mut width_changed_workspaces = Vec::new();
        for violation in width_violations {
            let Some((monitor_id, workspace_idx)) = self.find_window_workspace(violation.window_id)
            else {
                continue;
            };
            let work_area = self.layout_viewport(monitor_id);
            let viewport_width = work_area.width;
            let equality_is_proven = viewport_equality_is_proven(
                violation.position_matches,
                violation.requested_left,
                work_area.x,
            );
            if violation.min_width > viewport_width
                || (violation.min_width == viewport_width && !equality_is_proven)
            {
                debug!(
                    "Ignoring impossible/fullscreen-like width violation for window {} ({}px vs {}px viewport, position_matches={}, requested_left={}, work_left={})",
                    violation.window_id,
                    violation.min_width,
                    viewport_width,
                    violation.position_matches,
                    violation.requested_left,
                    work_area.x
                );
                continue;
            }
            let changed = self
                .workspaces
                .get_mut(&monitor_id)
                .and_then(|workspaces| workspaces.get_mut(workspace_idx))
                .is_some_and(|workspace| {
                    workspace.set_window_min_width(violation.window_id, violation.min_width)
                });
            if changed {
                constraints_changed = true;
                let slot = (monitor_id, workspace_idx);
                if !width_changed_workspaces.contains(&slot) {
                    width_changed_workspaces.push(slot);
                }
            }
        }

        // Fold only newly changed minimums into their stored column widths,
        // then re-validate active-workspace scroll. This avoids rescanning all
        // monitors × workspaces for every landing violation.
        let dragging = self.drag_state.is_some();
        for (monitor_id, workspace_idx) in width_changed_workspaces {
            let viewport_width = self.viewport_width_for(monitor_id);
            let active_idx = self.active_workspace_idx(monitor_id);
            if let Some(workspace) = self
                .workspaces
                .get_mut(&monitor_id)
                .and_then(|workspaces| workspaces.get_mut(workspace_idx))
            {
                if workspace.apply_min_width_constraints() {
                    constraints_changed = true;
                    if workspace_idx == active_idx && !dragging && !workspace.is_animating() {
                        workspace.ensure_focused_visible(viewport_width);
                    }
                }
            }
        }

        for violation in height_violations {
            let Some((monitor_id, workspace_idx)) = self.find_window_workspace(violation.window_id)
            else {
                continue;
            };
            let work_area = self.layout_viewport(monitor_id);
            let viewport_height = work_area.height;
            let equality_is_proven = viewport_equality_is_proven(
                violation.position_matches,
                violation.requested_top,
                work_area.y,
            );
            if violation.min_height > viewport_height
                || (violation.min_height == viewport_height && !equality_is_proven)
            {
                debug!(
                    "Ignoring impossible/fullscreen-like height violation for window {} ({}px vs {}px viewport, position_matches={}, requested_top={}, work_top={})",
                    violation.window_id,
                    violation.min_height,
                    viewport_height,
                    violation.position_matches,
                    violation.requested_top,
                    work_area.y
                );
                continue;
            }
            if self
                .workspaces
                .get_mut(&monitor_id)
                .and_then(|workspaces| workspaces.get_mut(workspace_idx))
                .is_some_and(|workspace| {
                    workspace.set_window_min_height(violation.window_id, violation.min_height)
                })
            {
                constraints_changed = true;
            }
        }
        constraints_changed
    }

    /// Post-success bookkeeping: border, tab strip, and deduped LayoutChanged broadcast.
    fn finalize_layout_success(&mut self) {
        if let Some(hwnd) = self.previous_focused_hwnd {
            if self.config.appearance.active_border {
                self.show_border(hwnd);
            }
        }
        // Reposition tab strip overlay (mirror border lifecycle).
        self.update_tab_strip();

        // LayoutChanged broadcast with signature dedup. Animation
        // frames between two settled layouts produce identical
        // signatures so the dedup check suppresses them; only
        // structural changes (column added/removed/reordered, width
        // changed, focus moved between columns) emit.
        let sig = self.focused_layout_signature();
        if self.last_emitted_layout_sig != Some(sig) {
            self.last_emitted_layout_sig = Some(sig);
            let monitor = self.focused_monitor;
            let workspace_index = self.active_workspace_idx(monitor);
            let focused_column = self
                .workspaces
                .get(&monitor)
                .and_then(|list| list.get(workspace_index))
                .map(|ws| ws.focused_column_index());
            self.broadcast_event(leopardwm_ipc::IpcEvent::LayoutChanged {
                monitor: monitor as i64,
                workspace_index: workspace_index as u8,
                focused_column,
                columns: self.focused_layout_columns(),
            });
        }

        // Request a debounced persist if any PERSISTED field changed.
        // Signature dedup + the background debounce coalesce animation
        // frames and rapid structural changes into at most ~one write/sec.
        self.request_save_if_changed();
    }
}

#[cfg(test)]
mod park_tests {
    use super::offscreen_park_rect;
    use leopardwm_core_layout::Rect;

    // A window scrolled off the owning monitor keeps its size when re-parked.
    const WIN: Rect = Rect {
        x: 6000,
        y: 10,
        width: 800,
        height: 600,
    };

    #[test]
    fn parks_below_when_a_horizontal_neighbor_blocks_the_side() {
        // Ultrawide at origin, second monitor to its right. Nothing above/below,
        // so the nearest free edge is below the owner.
        let owner = Rect::new(0, 0, 5120, 1440);
        let right = Rect::new(5120, 0, 1920, 1080);
        let parked = offscreen_park_rect(WIN, owner, &[owner, right]);
        assert_eq!(
            parked.y,
            owner.y + owner.height + 4,
            "parked just below the owner"
        );
        assert_eq!((parked.width, parked.height), (WIN.width, WIN.height));
        assert!(
            ![owner, right].iter().any(|m| parked.intersects(m)),
            "clears every monitor"
        );
    }

    #[test]
    fn parks_to_the_side_when_stacked_vertically_boxes_top_and_bottom() {
        // Three monitors stacked; the owner is the middle one, so above and
        // below are both taken and the park falls to the right edge.
        let owner = Rect::new(0, 1080, 1920, 1080);
        let above = Rect::new(0, 0, 1920, 1080);
        let below = Rect::new(0, 2160, 1920, 1080);
        let parked = offscreen_park_rect(WIN, owner, &[owner, above, below]);
        assert_eq!(
            parked.x,
            owner.x + owner.width + 4,
            "parked just right of the owner"
        );
        assert!(
            ![owner, above, below].iter().any(|m| parked.intersects(m)),
            "clears every monitor"
        );
    }

    #[test]
    fn parks_to_the_left_when_below_above_and_right_are_all_taken() {
        // Owner boxed on below, above, and right, so the left edge is the only
        // one that clears every monitor.
        let owner = Rect::new(2000, 0, 1000, 1000);
        let below = Rect::new(2000, 1000, 1000, 1000);
        let above = Rect::new(2000, -1000, 1000, 1000);
        let right = Rect::new(3000, 0, 1000, 1000);
        let parked = offscreen_park_rect(WIN, owner, &[owner, below, above, right]);
        assert_eq!(
            parked.x,
            owner.x - WIN.width - 4,
            "parked just left of the owner"
        );
        assert!(
            ![owner, below, above, right]
                .iter()
                .any(|m| parked.intersects(m)),
            "clears every monitor"
        );
    }

    #[test]
    fn falls_back_to_the_far_sentinel_when_boxed_in_on_all_sides() {
        let owner = Rect::new(0, 0, 1000, 1000);
        let neighbors = [
            owner,
            Rect::new(-2000, 0, 2000, 1000), // left
            Rect::new(1000, 0, 2000, 1000),  // right
            Rect::new(0, -2000, 1000, 2000), // above
            Rect::new(0, 1000, 1000, 2000),  // below
        ];
        let parked = offscreen_park_rect(WIN, owner, &neighbors);
        let sentinel = leopardwm_platform_win32::MOVE_OFFSCREEN_SENTINEL_COORD;
        assert_eq!((parked.x, parked.y), (sentinel, sentinel));
    }

    use super::park_offscreen_avoiding_neighbors;
    use leopardwm_core_layout::{Visibility, WindowPlacement};
    use leopardwm_platform_win32::{MonitorId, MonitorInfo};
    use std::collections::HashMap;

    fn monitor(id: MonitorId, x: i32, y: i32, w: i32, h: i32) -> MonitorInfo {
        MonitorInfo {
            id,
            rect: Rect::new(x, y, w, h),
            work_area: Rect::new(x, y, w, h),
            is_primary: false,
            device_name: String::new(),
            scale_factor: 1.0,
        }
    }

    fn placement(wid: u64, rect: Rect, visibility: Visibility) -> WindowPlacement {
        WindowPlacement {
            window_id: wid,
            rect,
            visibility,
            column_index: 0,
        }
    }

    #[test]
    fn wrapper_reparks_bleeding_placements_and_preserves_safe_ones() {
        let owner = monitor(1, 0, 0, 5120, 1440);
        let right = monitor(2, 5120, 0, 1920, 1080);
        let monitors: HashMap<MonitorId, MonitorInfo> = [(1, owner.clone()), (2, right.clone())]
            .into_iter()
            .collect();

        let visible = Rect::new(4700, 10, 400, 400); // fully contained by owner
        let non_bleed = Rect::new(-500, 10, 400, 400); // off-screen, on owner only
        let bleeding = Rect::new(5300, 10, 400, 400); // off-screen, on neighbor
        let mut placements = vec![
            placement(10, visible, Visibility::Visible),
            placement(20, non_bleed, Visibility::OffScreenLeft),
            placement(30, bleeding, Visibility::OffScreenRight),
        ];

        let monitor_rects = [owner.rect, right.rect];
        park_offscreen_avoiding_neighbors(&mut placements, 1, None, &monitors, &monitor_rects);

        assert_eq!(
            placements[0].rect, visible,
            "safe visible placement untouched"
        );
        assert_eq!(
            placements[1].rect, non_bleed,
            "non-bleeding off-screen untouched"
        );
        assert_ne!(placements[2].rect, bleeding, "bleeding placement re-parked");
        assert!(
            ![owner.rect, right.rect]
                .iter()
                .any(|m| placements[2].rect.intersects(m)),
            "re-parked clear of every monitor"
        );
    }

    #[test]
    fn wrapper_is_a_no_op_when_the_owner_monitor_is_missing() {
        let right = monitor(2, 5120, 0, 1920, 1080);
        let monitors: HashMap<MonitorId, MonitorInfo> = [(2, right)].into_iter().collect();
        let orig = Rect::new(5300, 10, 400, 400);
        let mut placements = vec![placement(30, orig, Visibility::OffScreenRight)];
        // owner_id 1 isn't in the map -> early return, nothing changes.
        let monitor_rects = [monitors[&2].rect];
        park_offscreen_avoiding_neighbors(&mut placements, 1, None, &monitors, &monitor_rects);
        assert_eq!(placements[0].rect, orig);
    }
}

#[cfg(test)]
mod monitor_isolation_tests {
    use super::*;
    use leopardwm_core_layout::{Rect, Visibility, WindowPlacement};
    use leopardwm_platform_win32::{MonitorId, MonitorInfo};
    use std::collections::HashMap;

    fn monitor(id: MonitorId, x: i32) -> MonitorInfo {
        let rect = Rect::new(x, 0, 1920, 1080);
        MonitorInfo {
            id,
            rect,
            work_area: rect,
            is_primary: id == 1,
            device_name: format!("DISPLAY{id}"),
            scale_factor: 1.0,
        }
    }

    fn side_by_side_monitors() -> HashMap<MonitorId, MonitorInfo> {
        HashMap::from([(1, monitor(1, 0)), (2, monitor(2, 1920))])
    }

    fn isolate(placements: &mut [WindowPlacement], owner_id: MonitorId) {
        let monitors = side_by_side_monitors();
        let rects: Vec<_> = monitors.values().map(|monitor| monitor.rect).collect();
        park_offscreen_avoiding_neighbors(placements, owner_id, None, &monitors, &rects);
    }

    #[test]
    fn partially_visible_tiled_window_is_hidden_from_right_neighbor() {
        let mut placements = vec![WindowPlacement {
            window_id: 1,
            rect: Rect::new(1800, 40, 400, 800),
            visibility: Visibility::Visible,
            column_index: 0,
        }];

        isolate(&mut placements, 1);

        assert_eq!(placements[0].visibility, Visibility::OffScreenRight);
        assert!(side_by_side_monitors()
            .values()
            .all(|monitor| !placements[0].rect.intersects(&monitor.rect)));
    }

    #[test]
    fn partially_visible_tiled_window_is_hidden_from_left_neighbor() {
        let mut placements = vec![WindowPlacement {
            window_id: 2,
            rect: Rect::new(1800, 40, 400, 800),
            visibility: Visibility::Visible,
            column_index: 0,
        }];

        isolate(&mut placements, 2);

        assert_eq!(placements[0].visibility, Visibility::OffScreenLeft);
        assert!(side_by_side_monitors()
            .values()
            .all(|monitor| !placements[0].rect.intersects(&monitor.rect)));
    }

    #[test]
    fn fully_contained_tiled_window_remains_visible() {
        let original = Rect::new(100, 40, 800, 800);
        let mut placements = vec![WindowPlacement {
            window_id: 3,
            rect: original,
            visibility: Visibility::Visible,
            column_index: 0,
        }];

        isolate(&mut placements, 1);

        assert_eq!(placements[0].visibility, Visibility::Visible);
        assert_eq!(placements[0].rect, original);
    }

    #[test]
    fn floating_window_may_intentionally_span_monitors() {
        let original = Rect::new(1800, 40, 400, 800);
        let mut placements = vec![WindowPlacement {
            window_id: 4,
            rect: original,
            visibility: Visibility::Visible,
            column_index: usize::MAX,
        }];

        isolate(&mut placements, 1);

        assert_eq!(placements[0].visibility, Visibility::Visible);
        assert_eq!(placements[0].rect, original);
    }

    #[test]
    fn existing_offscreen_placement_is_reparked_clear_of_neighbors() {
        let mut placements = vec![WindowPlacement {
            window_id: 5,
            rect: Rect::new(1920, 40, 600, 800),
            visibility: Visibility::OffScreenRight,
            column_index: 1,
        }];

        isolate(&mut placements, 1);

        assert_eq!(placements[0].visibility, Visibility::OffScreenRight);
        assert!(side_by_side_monitors()
            .values()
            .all(|monitor| !placements[0].rect.intersects(&monitor.rect)));
    }
}

#[cfg(test)]
#[path = "layout_apply_edge_tests.rs"]
mod edge_safety_audit_tests;
#[cfg(test)]
#[path = "layout_apply_focus_modes.rs"]
mod focus_mode_preview_tests;
