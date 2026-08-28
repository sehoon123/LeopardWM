//! Animation transitions: layout/workspace-switch transitions, the ghost-thumbnail path, and motion settings.

use crate::animation_worker;
use crate::state::*;
use std::collections::HashMap;
use tracing::{debug, info};

fn ghost_frame_barrier_timeout() -> std::time::Duration {
    #[cfg(test)]
    {
        std::time::Duration::from_millis(50)
    }
    #[cfg(not(test))]
    {
        std::time::Duration::from_millis(1500)
    }
}

pub(crate) fn reduce_motion_enabled(
    animations_enabled: bool,
    on_battery_or_saver: bool,
    reduce_motion_on_battery: bool,
) -> bool {
    !animations_enabled || (on_battery_or_saver && reduce_motion_on_battery)
}

/// Whether an animation must be collapsed into one exact landing.
///
/// Position-only movement remains smooth under compositor-safe mode. Only a
/// structural transition that interpolates live window dimensions without a
/// safe DWM ghost is collapsed, because repeated live resize is the part no
/// renderer-agnostic Win32 sequence can make reliable.
pub(crate) fn compositor_safe_snap_required(
    compositor_safe_mode: bool,
    has_active_animation: bool,
    transition_requires_snap: bool,
) -> bool {
    compositor_safe_mode && has_active_animation && transition_requires_snap
}

fn transition_requires_compositor_safe_snap(
    compositor_safe_mode: bool,
    start_rects: &HashMap<u64, leopardwm_core_layout::Rect>,
    targets: &HashMap<
        u64,
        (
            leopardwm_core_layout::Rect,
            leopardwm_platform_win32::MonitorId,
        ),
    >,
    ghosted_wids: &std::collections::HashSet<u64>,
    compositor_sensitive_wids: &std::collections::HashSet<u64>,
) -> bool {
    compositor_safe_mode
        && start_rects.iter().any(|(wid, start)| {
            targets.get(wid).is_some_and(|(target, _)| {
                !ghosted_wids.contains(wid)
                    && (start.width != target.width
                        || start.height != target.height
                        || compositor_sensitive_wids.contains(wid))
            })
        })
}

impl AppState {
    /// Check if any workspace has an active animation or layout transition.
    pub(crate) fn is_animating(&self) -> bool {
        self.layout_transition.is_some()
            || self
                .workspaces
                .values()
                .any(|ws_vec| ws_vec.iter().any(|w| w.is_animating()))
    }

    /// Cancel the structural/workspace transition without stranding its exiting
    /// HWNDs, and force the next apply through the exact-landing path.
    ///
    /// Every caller that clears `layout_transition` must use this operation. The
    /// transition is the only owner of `exit_rects`; dropping it directly leaves
    /// old-workspace windows at their last interpolated positions, and the normal
    /// active-workspace apply has no way to discover or park them.
    pub(crate) fn cancel_layout_transition_for_exact_landing(&mut self) -> anyhow::Result<bool> {
        let Some(transition) = self.layout_transition.as_ref() else {
            return Ok(false);
        };

        // This method is the sole transition-ownership release operation, so the
        // ordering edge belongs here rather than at selected callers. A frame
        // that passed its worker-side epoch check can otherwise move an exit
        // back on-screen after we park it and drop its only ownership record.
        self.apply_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let worker = self.animation_worker_control.clone();
        if worker
            .as_ref()
            .is_some_and(|worker| !worker.wait_for_barrier(std::time::Duration::from_millis(750)))
        {
            return Err(anyhow::anyhow!(
                "animation worker barrier timed out before transition ownership release"
            ));
        }

        let exit_windows: Vec<u64> = transition.exit_rects.keys().copied().collect();

        let mut failures = Vec::new();
        for window_id in exit_windows {
            if let Err(error) = leopardwm_platform_win32::move_window_offscreen(window_id) {
                // A destroyed HWND has no pixels to strand and is therefore
                // already a safe terminal state.
                if leopardwm_platform_win32::is_valid_window(window_id) {
                    failures.push(format!("{window_id:#x}: {error}"));
                }
            }
        }
        if !failures.is_empty() {
            return Err(anyhow::anyhow!(
                "could not park transition exit window(s): {}",
                failures.join(", ")
            ));
        }
        self.abort_active_ghost_transition();
        self.layout_transition = None;
        self.post_animation_landing_pending = true;
        self.last_placed_layout_rects.clear();
        if let Some(worker) = worker {
            worker.clear_cache();
        }
        Ok(true)
    }

    /// Collapse an unsafe size-changing transition to its final state.
    ///
    /// Pure scroll and position-only workspace/layout transitions stay smooth.
    /// The decision remains at the central frame-dispatch choke point so no
    /// event source can bypass it.
    pub(crate) fn settle_animations_for_compositor_safety(&mut self) -> bool {
        let transition_requires_snap = self
            .layout_transition
            .as_ref()
            .is_some_and(|transition| transition.requires_compositor_safe_snap);
        if !compositor_safe_snap_required(
            self.config.behavior.compositor_safe_mode,
            self.is_animating(),
            transition_requires_snap,
        ) {
            return false;
        }

        for workspaces in self.workspaces.values_mut() {
            for workspace in workspaces {
                workspace.stop_animation();
            }
        }

        // Park transition-owned exit windows, release ghosts, and force the
        // exact synchronous landing even when desired rectangles match cache.
        match self.cancel_layout_transition_for_exact_landing() {
            Ok(true) => {
                debug!("Collapsed unsafe size-changing transition into an exact landing");
                true
            }
            Ok(false) => false,
            Err(error) => {
                tracing::warn!("Could not collapse unsafe layout transition: {error}");
                false
            }
        }
    }

    /// Tick all active animations by the given delta time.
    /// Returns true if any animation is still running.
    /// Land every in-flight scroll animation immediately.
    ///
    /// Used when the pointer is about to act on a window's real geometry: a
    /// half-finished scroll would hand over a window that is still moving.
    pub(crate) fn settle_scroll_animations(&mut self) {
        for ws_vec in self.workspaces.values_mut() {
            for workspace in ws_vec.iter_mut() {
                if workspace.is_animating() {
                    workspace.stop_animation();
                }
            }
        }
    }

    pub(crate) fn tick_animations(&mut self, delta_ms: u64) -> bool {
        let mut still_animating = false;
        let mut scroll_anims_settled = false;
        for ws_vec in self.workspaces.values_mut() {
            for workspace in ws_vec.iter_mut() {
                let was_animating = workspace.is_animating();
                if workspace.tick_animation(delta_ms) {
                    still_animating = true;
                } else if was_animating {
                    // A scroll animation just finished: scroll_offset is now the
                    // target, so viewport visibility (and taskbar buttons) can be
                    // reconciled against the settled position. An animated focus
                    // scroll leaves scroll_offset stale until exactly this point.
                    scroll_anims_settled = true;
                }
            }
        }
        if scroll_anims_settled {
            self.sync_taskbar_buttons();
        }
        if let Some(ref mut transition) = self.layout_transition {
            if transition.tick(delta_ms) {
                still_animating = true;
            } else {
                // Transition ownership ends only after every live exit is parked.
                let failures: Vec<_> = transition
                    .exit_rects
                    .keys()
                    .filter_map(|window_id| {
                        leopardwm_platform_win32::move_window_offscreen(*window_id)
                            .err()
                            .filter(|_| leopardwm_platform_win32::is_valid_window(*window_id))
                            .map(|error| (*window_id, error))
                    })
                    .collect();
                if failures.is_empty() {
                    self.layout_transition = None;
                    self.sync_taskbar_buttons();
                } else {
                    transition.exit_park_failures = transition.exit_park_failures.saturating_add(1);
                    tracing::warn!(
                        "Transition exit parking attempt {}/3 failed: {:?}",
                        transition.exit_park_failures,
                        failures
                            .iter()
                            .map(|(window_id, _)| format!("{window_id:#x}"))
                            .collect::<Vec<_>>()
                    );
                    if transition.exit_park_failures >= 3 {
                        // Retain ownership for an explicit resume/recovery, but
                        // stop the frame-cadence retry loop.
                        self.paused = true;
                        still_animating = false;
                    } else {
                        still_animating = true;
                    }
                }
                if self.layout_transition.is_none() {
                    // Exact landing follows successful ownership release.
                    still_animating = true;
                }
            }
        }
        still_animating
    }

    /// Snapshot the current placement rects for all tiled windows.
    /// Call this *before* a structural layout change.
    pub(crate) fn snapshot_layout(
        &self,
    ) -> std::collections::HashMap<u64, leopardwm_core_layout::Rect> {
        let mut rects = std::collections::HashMap::new();
        for (monitor_id, ws_vec) in &self.workspaces {
            let idx = self.active_workspace_idx(*monitor_id);
            if let Some(workspace) = ws_vec.get(idx) {
                if self.monitors.contains_key(monitor_id) {
                    let viewport = self.layout_viewport(*monitor_id);
                    for p in workspace.compute_placements_animated(viewport) {
                        rects.insert(p.window_id, p.rect);
                    }
                }
            }
        }
        rects
    }

    /// Start a layout transition animation from a pre-change snapshot.
    /// Call this *after* the structural change and ensure_focused_visible_animated.
    /// No-op when reduce_motion is active.
    pub(crate) fn start_layout_transition(
        &mut self,
        start_rects: std::collections::HashMap<u64, leopardwm_core_layout::Rect>,
    ) {
        if self.reduce_motion {
            return;
        }
        let duration = self.config.animation.layout_duration_ms;
        self.start_layout_transition_with_duration(start_rects, duration);
    }

    pub(crate) fn start_layout_transition_with_duration(
        &mut self,
        start_rects: std::collections::HashMap<u64, leopardwm_core_layout::Rect>,
        duration_ms: u64,
    ) {
        // Any prior ghost transition or in-flight crossfade is invalidated
        // by a new transition starting. Drops handles via Drop, uncloaks
        // sources, and tells the worker to abort the fade.
        self.abort_active_ghost_transition();
        if !self.ghost_handles.is_empty() {
            // The abort fence timed out and paused tiling. Never overlap a new
            // registration with frame-owned registrations for the same source.
            return;
        }

        let targets = self.collect_transition_targets();

        // In safe mode thumbnails are attempted only for size-changing
        // transitions; position-only movement is already safe on the adaptive
        // synchronous path. Legacy mode keeps its broader experimental ghosting.
        let mut ghosted_wids = std::collections::HashSet::new();
        if self.config.behavior.swap_chain_ghost_animation
            && !self.config.behavior.compositor_safe_mode
        {
            self.register_ghosts_for_transition(&start_rects, &targets, false, &mut ghosted_wids);
        }
        let compositor_sensitive_wids: std::collections::HashSet<_> = targets
            .keys()
            .copied()
            .filter(|window_id| {
                leopardwm_platform_win32::thumbnail::is_compositor_sensitive_class(*window_id)
            })
            .collect();
        let requires_compositor_safe_snap = transition_requires_compositor_safe_snap(
            self.config.behavior.compositor_safe_mode,
            &start_rects,
            &targets,
            &ghosted_wids,
            &compositor_sensitive_wids,
        );

        // Start with one frame (~16ms) already elapsed so the first
        // apply_layout/send_animation_frame shows visible movement.
        self.layout_transition = Some(LayoutTransition {
            start_rects,
            exit_rects: HashMap::new(),
            exit_column_indices: HashMap::new(),
            elapsed_ms: 16,
            duration_ms,
            easing: self.config.animation.easing,
            requires_compositor_safe_snap,
            ghosted_wids,
            exit_park_failures: 0,
        });
    }

    /// Start a workspace switch transition that animates both entering and
    /// exiting windows simultaneously (continuous vertical scroll effect).
    /// No-op when reduce_motion is active.
    ///
    /// Workspace-switch transitions never use the ghost path: every window
    /// either slides off-screen (exit_rects) or slides in from off-screen,
    /// neither of which is the rapid-async-burst-while-visible scenario
    /// that the swap-chain bug exhibits.
    pub(crate) fn start_workspace_switch_transition(
        &mut self,
        start_rects: std::collections::HashMap<u64, leopardwm_core_layout::Rect>,
        exit_rects: std::collections::HashMap<u64, leopardwm_core_layout::Rect>,
        duration_ms: u64,
    ) {
        if self.reduce_motion {
            return;
        }
        self.abort_active_ghost_transition();
        if !self.ghost_handles.is_empty() {
            return;
        }
        let requires_compositor_safe_snap = self.config.behavior.compositor_safe_mode
            && start_rects
                .keys()
                .chain(exit_rects.keys())
                .any(|window_id| {
                    leopardwm_platform_win32::thumbnail::is_compositor_sensitive_class(*window_id)
                });
        let exit_column_indices = exit_rects
            .keys()
            .map(|window_id| {
                let is_floating = self.workspaces.values().any(|workspaces| {
                    workspaces.iter().any(|workspace| {
                        workspace.contains_window(*window_id) && workspace.is_floating(*window_id)
                    })
                });
                (*window_id, if is_floating { usize::MAX } else { 0 })
            })
            .collect();
        self.layout_transition = Some(LayoutTransition {
            start_rects,
            exit_rects,
            exit_column_indices,
            elapsed_ms: 16,
            duration_ms,
            easing: self.config.animation.easing,
            requires_compositor_safe_snap,
            ghosted_wids: std::collections::HashSet::new(),
            exit_park_failures: 0,
        });
    }

    /// Drop any in-flight ghost-animation handles and uncloak their
    /// sources, then signal the worker to abort any running crossfade.
    ///
    /// Routed through by every code path that mutates or clears
    /// `layout_transition`. No-op when no ghost state is alive.
    pub(crate) fn abort_active_ghost_transition(&mut self) {
        if !self.ghost_handles.is_empty() {
            // A queued frame owns Arc clones of these registrations. Invalidate
            // its desired epoch and drain the worker before clearing daemon
            // ownership, uncloaking sources, or permitting same-source
            // re-registration. Arc prevents UAF; this fence prevents overlap.
            self.apply_epoch
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self
                .animation_worker_control
                .as_ref()
                .is_some_and(|worker| !worker.wait_for_barrier(ghost_frame_barrier_timeout()))
            {
                self.paused = true;
                tracing::warn!(
                    "Ghost frame barrier timed out; retaining registrations and pausing tiling"
                );
                return;
            }
        }
        let wids: Vec<u64> = self.ghost_handles.keys().copied().collect();
        self.ghost_handles.clear();

        // Uncloak the (formerly) ghosted sources through apply_cloak_state so a
        // window also in GLOBAL_CLOAKED (off-screen parked) stays cloaked.
        for wid in &wids {
            leopardwm_platform_win32::unmark_ghost_cloaked(*wid);
        }

        // Clear ghosted_wids on any still-live transition so
        // partition_for_animation no longer routes frames for them. In safe
        // mode every registered ghost represents a size-changing window; losing
        // that protection makes the transition snap-only.
        let compositor_safe_mode = self.config.behavior.compositor_safe_mode;
        if let Some(ref mut transition) = self.layout_transition {
            if compositor_safe_mode && !transition.ghosted_wids.is_empty() {
                transition.requires_compositor_safe_snap = true;
            }
            transition.ghosted_wids.clear();
        }

        // Signal any in-flight crossfade to abort. The worker acks via
        // DaemonEvent::CrossfadeComplete { epoch }; that epoch's entry in
        // crossfade_sources is removed then.
        self.abort_active_crossfade();
    }

    /// Send `AbortCrossfade { epoch }` to the worker if a fade is in
    /// flight. The worker checks between fade iterations and exits
    /// early; CrossfadeComplete arrives within ~16ms (one DwmFlush).
    ///
    /// Daemon-side `active_crossfade` is cleared immediately so any
    /// subsequent `should_ghost` evaluation doesn't see stale state,
    /// but `crossfade_sources` stays populated until CrossfadeComplete
    /// confirms the worker has stopped using the entries. This avoids
    /// re-registering a thumbnail for the same source HWND while the
    /// worker may still be updating the old one (Microsoft Q&A 3229922).
    pub(crate) fn abort_active_crossfade(&mut self) {
        if let Some(state) = self.active_crossfade.take() {
            if let Some(ref ctrl) = self.animation_worker_control {
                ctrl.send_abort_crossfade(state.epoch);
            }
            // crossfade_sources[epoch] intentionally left populated until
            // the worker acks via CrossfadeComplete. The main-loop handler
            // removes that epoch's entry then. Per-epoch tracking is what
            // makes this safe under overlapping aborts.
        }
    }

    /// Drop re-registration barriers whose `CrossfadeComplete` never
    /// arrived (worker died/stuck), so their source wids aren't stranded
    /// out of the ghost path forever. A crossfade can't legitimately
    /// outlive `CROSSFADE_BARRIER_MAX_AGE`. Run at the top of every ghost
    /// registration pass.
    pub(crate) fn sweep_stale_crossfade_barriers(&mut self) {
        self.crossfade_sources
            .retain(|_, (_, at)| at.elapsed() < crate::state::CROSSFADE_BARRIER_MAX_AGE);
    }

    fn collect_transition_targets(
        &self,
    ) -> std::collections::HashMap<
        u64,
        (
            leopardwm_core_layout::Rect,
            leopardwm_platform_win32::MonitorId,
        ),
    > {
        let mut targets = std::collections::HashMap::new();
        for (monitor_id, ws_vec) in &self.workspaces {
            let idx = self.active_workspace_idx(*monitor_id);
            if let Some(workspace) = ws_vec.get(idx) {
                if self.monitors.contains_key(monitor_id) {
                    let viewport = self.layout_viewport(*monitor_id);
                    for placement in workspace.compute_placements_animated(viewport) {
                        if placement.visibility == leopardwm_core_layout::Visibility::Visible {
                            targets.insert(placement.window_id, (placement.rect, *monitor_id));
                        }
                    }
                }
            }
        }
        targets
    }

    /// Register safe DWM ghosts for changing compositor-sensitive windows.
    /// `size_changes_only` is true for adaptive safe mode, where ordinary
    /// position-only movement remains on the synchronized live-HWND path.
    fn register_ghosts_for_transition(
        &mut self,
        start_rects: &std::collections::HashMap<u64, leopardwm_core_layout::Rect>,
        targets: &std::collections::HashMap<
            u64,
            (
                leopardwm_core_layout::Rect,
                leopardwm_platform_win32::MonitorId,
            ),
        >,
        size_changes_only: bool,
        ghosted_wids: &mut std::collections::HashSet<u64>,
    ) {
        self.sweep_stale_crossfade_barriers();

        if !leopardwm_platform_win32::thumbnail::host().is_available() {
            return;
        }
        let host_origin = leopardwm_platform_win32::thumbnail::host().origin();
        let focused = self.previous_focused_hwnd;

        // Identify which monitor the focused window is on (used to gate
        // cross-monitor moves out of the ghost path).
        let focused_monitor = focused
            .and_then(|wid| targets.get(&wid).map(|(_, mon)| *mon))
            .unwrap_or(self.focused_monitor);

        for (&wid, &start_rect) in start_rects {
            let Some(&(target_rect, monitor_id)) = targets.get(&wid) else {
                continue;
            };
            if start_rect == target_rect {
                continue;
            }
            let size_changed =
                start_rect.width != target_rect.width || start_rect.height != target_rect.height;
            if size_changes_only && !size_changed {
                continue;
            }
            // Cross-monitor moves use the legacy nudge path — the
            // thumbnail host covers the virtual screen but cross-monitor
            // animation is rare (drag-only) and rarely hits the
            // rapid-async-burst case.
            if monitor_id != focused_monitor {
                continue;
            }
            // Skip the focused window: SetForegroundWindow on a cloaked
            // HWND is undocumented behavior. The focused window still
            // gets the (w-1 → w) nudge at landing.
            if focused == Some(wid) {
                continue;
            }
            // Same-source re-registration barrier (Microsoft Q&A 3229922):
            // refuse if ANY pending-ack crossfade epoch still owns this wid.
            if self
                .crossfade_sources
                .values()
                .any(|(set, _)| set.contains(&wid))
            {
                continue;
            }
            let class = leopardwm_platform_win32::thumbnail::class_name(wid);
            if !leopardwm_platform_win32::thumbnail::is_ghost_animation_class_str(&class) {
                continue;
            }
            match leopardwm_platform_win32::thumbnail::register(wid) {
                Ok(handle) => {
                    // A thumbnail is safe only when its live source is actually
                    // hidden. External application HWNDs normally reject
                    // DWMWA_CLOAK; fall back to live placement rather than
                    // compositing a moving thumbnail over an uncloaked source.
                    if !leopardwm_platform_win32::try_mark_ghost_cloaked(wid) {
                        tracing::debug!(
                            "ghost: physical cloak unavailable for {wid}; using live placement"
                        );
                        continue;
                    }
                    let final_dest = leopardwm_platform_win32::thumbnail::screen_to_host_client(
                        target_rect,
                        host_origin,
                    );
                    let entry =
                        crate::state::GhostEntry::new(handle.into_isize(), class, final_dest);
                    self.ghost_handles.insert(wid, entry);
                    ghosted_wids.insert(wid);
                }
                Err(e) => {
                    tracing::warn!("ghost register failed for {wid}: {e}");
                }
            }
        }
        if !ghosted_wids.is_empty() {
            tracing::debug!(
                "ghost: registered {} thumbnail(s), balance={}",
                ghosted_wids.len(),
                leopardwm_platform_win32::thumbnail::current_register_balance()
            );
        }
    }

    /// Apply layout transition interpolation to placements, including exit windows.
    pub(crate) fn apply_transition_interpolation(
        transition: &LayoutTransition,
        placements: &mut Vec<leopardwm_core_layout::WindowPlacement>,
    ) {
        let t = transition.eased_progress();
        // Interpolate entering/morphing windows.
        for p in placements.iter_mut() {
            if let Some(start) = transition.start_rects.get(&p.window_id) {
                p.rect = leopardwm_core_layout::Rect::new(
                    start.x + ((p.rect.x - start.x) as f64 * t).round() as i32,
                    start.y + ((p.rect.y - start.y) as f64 * t).round() as i32,
                    start.width + ((p.rect.width - start.width) as f64 * t).round() as i32,
                    start.height + ((p.rect.height - start.height) as f64 * t).round() as i32,
                );
            }
        }
        // Interpolate exiting windows (e.g., old workspace sliding out).
        for (wid, target) in &transition.exit_rects {
            if let Some(start) = transition.start_rects.get(wid) {
                placements.push(leopardwm_core_layout::WindowPlacement {
                    window_id: *wid,
                    rect: leopardwm_core_layout::Rect::new(
                        start.x + ((target.x - start.x) as f64 * t).round() as i32,
                        start.y + ((target.y - start.y) as f64 * t).round() as i32,
                        start.width + ((target.width - start.width) as f64 * t).round() as i32,
                        start.height + ((target.height - start.height) as f64 * t).round() as i32,
                    ),
                    visibility: leopardwm_core_layout::Visibility::Visible,
                    column_index: transition
                        .exit_column_indices
                        .get(wid)
                        .copied()
                        .unwrap_or(0),
                });
            }
        }
    }

    /// Split animated placements into (live, ghost) streams. Ghosted wids
    /// — those in `LayoutTransition.ghosted_wids` with a matching entry in
    /// `ghost_handles` — get a `GhostFrame` per frame instead of a per-
    /// frame SetWindowPos on the live HWND.
    ///
    /// Pure function: no Win32 calls, no mutation. Unit-testable with
    /// stub `GhostEntry` values.
    pub(crate) fn partition_for_animation(
        placements: Vec<leopardwm_core_layout::WindowPlacement>,
        transition: Option<&LayoutTransition>,
        ghost_handles: &std::collections::HashMap<u64, crate::state::GhostEntry>,
        region_clips: &[leopardwm_platform_win32::WindowRegionClip],
    ) -> (
        Vec<leopardwm_core_layout::WindowPlacement>,
        Vec<animation_worker::GhostFrame>,
    ) {
        let mut live: Vec<leopardwm_core_layout::WindowPlacement> =
            Vec::with_capacity(placements.len());
        let mut ghosts: Vec<animation_worker::GhostFrame> =
            Vec::with_capacity(placements.len().min(ghost_handles.len()));

        let host_origin = leopardwm_platform_win32::thumbnail::host().origin();
        let ghosted_wids = transition.map(|t| &t.ghosted_wids);

        for p in placements {
            let is_ghost = ghosted_wids
                .map(|set| set.contains(&p.window_id))
                .unwrap_or(false);
            if is_ghost {
                if let Some(entry) = ghost_handles.get(&p.window_id) {
                    let clipped = region_clips
                        .iter()
                        .find(|clip| clip.window_id == p.window_id)
                        .and_then(|clip| {
                            let left = p.rect.x.max(clip.clip_bounds.x);
                            let top = p.rect.y.max(clip.clip_bounds.y);
                            let right = p.rect.right().min(clip.clip_bounds.right());
                            let bottom = p.rect.bottom().min(clip.clip_bounds.bottom());
                            (right > left && bottom > top).then(|| {
                                let destination = leopardwm_core_layout::Rect::new(
                                    left,
                                    top,
                                    right - left,
                                    bottom - top,
                                );
                                let source = leopardwm_core_layout::Rect::new(
                                    left - p.rect.x,
                                    top - p.rect.y,
                                    right - left,
                                    bottom - top,
                                );
                                (source, destination)
                            })
                        });
                    let (source_crop, destination) = if let Some((source, destination)) = clipped {
                        (
                            Some((source, (p.rect.width.max(1), p.rect.height.max(1)))),
                            destination,
                        )
                    } else if region_clips
                        .iter()
                        .any(|clip| clip.window_id == p.window_id)
                    {
                        // The planned clip contains no pixels for this frame.
                        // Keep the handle hidden rather than drawing a full ghost.
                        ghosts.push(animation_worker::GhostFrame {
                            window_id: p.window_id,
                            registration: entry.shared_registration(),
                            source_crop: None,
                            dest_client_rect:
                                leopardwm_platform_win32::thumbnail::screen_to_host_client(
                                    p.rect,
                                    host_origin,
                                ),
                            opacity: 0,
                            visible: false,
                        });
                        continue;
                    } else {
                        (None, p.rect)
                    };
                    let dest = leopardwm_platform_win32::thumbnail::screen_to_host_client(
                        destination,
                        host_origin,
                    );
                    ghosts.push(animation_worker::GhostFrame {
                        window_id: p.window_id,
                        registration: entry.shared_registration(),
                        source_crop,
                        dest_client_rect: dest,
                        opacity: 255,
                        visible: true,
                    });
                }
                // If transition has the wid but ghost_handles doesn't —
                // e.g., registration failed earlier — drop the placement
                // entirely. The window will land at its target rect via
                // the post-animation landing pass.
            } else {
                live.push(p);
            }
        }

        (live, ghosts)
    }

    /// Recompute `reduce_motion` from the accessibility setting and power state,
    /// propagating to all workspaces when the value changes.
    pub(crate) fn refresh_reduce_motion(&mut self) {
        let should_reduce = reduce_motion_enabled(
            leopardwm_platform_win32::are_animations_enabled(),
            self.on_battery_or_saver,
            self.config.animation.reduce_motion_on_battery,
        );
        if should_reduce != self.reduce_motion {
            self.reduce_motion = should_reduce;
            for ws_vec in self.workspaces.values_mut() {
                for ws in ws_vec.iter_mut() {
                    ws.set_reduce_motion(should_reduce);
                }
            }
            info!(
                "Animations {}",
                if should_reduce { "disabled" } else { "enabled" }
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        compositor_safe_snap_required, reduce_motion_enabled,
        transition_requires_compositor_safe_snap,
    };
    use leopardwm_core_layout::Rect;
    use std::collections::{HashMap, HashSet};

    #[test]
    fn compositor_safe_policy_preserves_position_only_animation() {
        assert!(compositor_safe_snap_required(true, true, true));
        assert!(!compositor_safe_snap_required(true, true, false));
        assert!(!compositor_safe_snap_required(true, false, true));
        assert!(!compositor_safe_snap_required(false, true, true));
    }

    #[test]
    fn transition_policy_snaps_unprotected_resize_and_sensitive_position_changes() {
        let start = HashMap::from([(1, Rect::new(0, 0, 800, 600))]);
        let moved = HashMap::from([(1, (Rect::new(100, 0, 800, 600), 1))]);
        let resized = HashMap::from([(1, (Rect::new(100, 0, 900, 600), 1))]);

        assert!(!transition_requires_compositor_safe_snap(
            true,
            &start,
            &moved,
            &HashSet::new(),
            &HashSet::new(),
        ));
        assert!(transition_requires_compositor_safe_snap(
            true,
            &start,
            &moved,
            &HashSet::new(),
            &HashSet::from([1]),
        ));
        assert!(transition_requires_compositor_safe_snap(
            true,
            &start,
            &resized,
            &HashSet::new(),
            &HashSet::new(),
        ));
        assert!(!transition_requires_compositor_safe_snap(
            true,
            &start,
            &resized,
            &HashSet::from([1]),
            &HashSet::from([1]),
        ));
        assert!(!transition_requires_compositor_safe_snap(
            false,
            &start,
            &resized,
            &HashSet::new(),
            &HashSet::from([1]),
        ));
    }

    #[test]
    fn reduce_motion_policy_honors_accessibility_and_battery_preference() {
        assert!(!reduce_motion_enabled(true, true, false));
        assert!(reduce_motion_enabled(true, true, true));
        assert!(!reduce_motion_enabled(true, false, true));
        assert!(reduce_motion_enabled(false, false, false));
        assert!(reduce_motion_enabled(false, true, false));
    }
}
