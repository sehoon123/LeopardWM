//! Monitor topology reconciliation and cross-monitor window moves.

use crate::helpers::ScaledLayoutParams;
use crate::state::*;
use leopardwm_core_layout::{Rect, Workspace};
use leopardwm_platform_win32::{MonitorId, MonitorInfo};
use std::collections::{HashMap, HashSet};
use tracing::{info, warn};

impl AppState {
    /// Re-key state that stores volatile HMONITOR values without changing its
    /// physical-monitor ownership. Rebuild map keys from a single snapshot so
    /// two handles that happen to swap values cannot overwrite each other's
    /// workspace or floating-focus entry during a sequential move.
    fn rekey_monitor_state(&mut self, remap: &HashMap<MonitorId, MonitorId>) {
        let remap_id = |monitor_id| remap.get(&monitor_id).copied().unwrap_or(monitor_id);

        self.monitors = std::mem::take(&mut self.monitors)
            .into_iter()
            .map(|(monitor_id, mut monitor)| {
                let monitor_id = remap_id(monitor_id);
                monitor.id = monitor_id;
                (monitor_id, monitor)
            })
            .collect();
        self.workspaces = std::mem::take(&mut self.workspaces)
            .into_iter()
            .map(|(monitor_id, workspaces)| (remap_id(monitor_id), workspaces))
            .collect();
        self.active_workspace = std::mem::take(&mut self.active_workspace)
            .into_iter()
            .map(|(monitor_id, workspace_idx)| (remap_id(monitor_id), workspace_idx))
            .collect();
        self.floating_focus = std::mem::take(&mut self.floating_focus)
            .into_iter()
            .map(|((monitor_id, workspace_idx), hwnd)| {
                ((remap_id(monitor_id), workspace_idx), hwnd)
            })
            .collect();
        self.focused_monitor = remap_id(self.focused_monitor);

        for origin in self.move_origins.values_mut() {
            origin.monitor = remap_id(origin.monitor);
        }
        if let Some(pending) = self.pending_tab_focus.as_mut() {
            if pending.is_fresh() {
                pending.monitor = remap_id(pending.monitor);
            }
        }
        if let Some(drag) = self.drag_state.as_mut() {
            drag.source_monitor = remap_id(drag.source_monitor);
            if let Some(target) = drag.last_drop_target.as_mut() {
                target.monitor = remap_id(target.monitor);
            }
        }
    }

    /// Reconcile workspaces after monitor configuration change.
    ///
    /// This handles:
    /// - Removing workspaces for disconnected monitors (migrating windows to primary)
    /// - Adding workspaces for newly connected monitors
    pub(crate) fn reconcile_monitors(&mut self, new_monitors: Vec<MonitorInfo>) {
        // Windows repositions and rescales windows itself while a topology
        // change is in flight, and the moved/resized events it emits arrive
        // inside the apply-layout suppression window. The desired-rect fast path
        // only compares against what this daemon last *requested*, so returning
        // to a previously used arrangement (side-by-side -> stacked ->
        // side-by-side) can match the cache exactly and skip the corrective
        // placement entirely, stranding tiled windows wherever the OS left them
        // — including fully on a neighboring monitor, where the overflow policy
        // never gets a chance to hide or clip them. Every reconcile therefore
        // invalidates both caches so the following apply re-places every window.
        self.last_placed_layout_rects.clear();
        self.moved_or_resized_suppression.clear();
        // Preview click targets are positioned in screen coordinates, so every
        // one of them is stale the moment the topology changes. Drop them now
        // rather than risk an overlay absorbing clicks at an old rectangle if
        // the corrective apply below fails; a successful apply republishes them.
        leopardwm_platform_win32::preview_input::clear_preview_click_targets();

        let new_ids: HashSet<MonitorId> = new_monitors.iter().map(|m| m.id).collect();
        let new_id_by_name: HashMap<&str, MonitorId> = new_monitors
            .iter()
            .map(|monitor| (monitor.device_name.as_str(), monitor.id))
            .collect();

        // HMONITOR values are volatile and can be swapped or immediately reused
        // for a different device. Align surviving devices by stable name first.
        // If a disconnected device still occupies an ID now used by another
        // device, move its old state to a temporary key so the normal removal
        // path can still migrate/stash it without colliding with the survivor.
        let mut used_ids: HashSet<MonitorId> = self.monitors.keys().copied().collect();
        used_ids.extend(new_ids.iter().copied());
        let mut temporary_id = MonitorId::MIN;
        let mut remap = HashMap::new();
        for monitor in self.monitors.values() {
            if let Some(&new_id) = new_id_by_name.get(monitor.device_name.as_str()) {
                if monitor.id != new_id {
                    remap.insert(monitor.id, new_id);
                }
            } else if new_ids.contains(&monitor.id) {
                while used_ids.contains(&temporary_id) {
                    temporary_id = temporary_id.saturating_add(1);
                }
                remap.insert(monitor.id, temporary_id);
                used_ids.insert(temporary_id);
                temporary_id = temporary_id.saturating_add(1);
            }
        }
        if !remap.is_empty() {
            info!(
                "Re-keying state for {} monitor handle(s) by stable device name",
                remap.len()
            );
            self.rekey_monitor_state(&remap);
        }
        let old_ids: HashSet<MonitorId> = self.monitors.keys().copied().collect();

        // With every surviving device already aligned to its current handle, an
        // unchanged device-name set needs only metric/DPI refresh and refitting.
        let same_physical_topology = new_monitors.len() == self.monitors.len()
            && self
                .monitors
                .values()
                .all(|monitor| new_id_by_name.contains_key(monitor.device_name.as_str()));
        if same_physical_topology {
            self.monitors = new_monitors.into_iter().map(|m| (m.id, m)).collect();
            for (&monitor_id, ws_vec) in self.workspaces.iter_mut() {
                let scale = self
                    .monitors
                    .get(&monitor_id)
                    .map(|m| m.scale_factor)
                    .unwrap_or(1.0);
                let work_area = self
                    .monitors
                    .get(&monitor_id)
                    .map(|m| m.work_area)
                    .unwrap_or(Rect::new(
                        0,
                        0,
                        FALLBACK_VIEWPORT_WIDTH,
                        FALLBACK_WORK_AREA_HEIGHT,
                    ));
                let viewport_width = work_area.width;
                let params = ScaledLayoutParams::from_config(
                    &self.config.layout,
                    &self.config.appearance,
                    scale,
                    viewport_width,
                );
                for workspace in ws_vec.iter_mut() {
                    let old_gap = workspace.gap();
                    let (old_ol, old_or, _, _) = workspace.outer_gaps();
                    params.apply_to(workspace);
                    workspace.rescale_column_widths(old_gap, old_ol, old_or, viewport_width);
                    workspace.clamp_floating_to(work_area);
                }
            }
            return;
        }

        // Find primary monitor in new config (or first available)
        let primary_id = new_monitors
            .iter()
            .find(|m| m.is_primary)
            .or_else(|| new_monitors.first())
            .map(|m| m.id);

        // Handle added monitors - create new workspaces FIRST so migration
        // targets exist even when all old monitors are replaced with new ones.
        for monitor in &new_monitors {
            if !old_ids.contains(&monitor.id) {
                // A monitor whose stable device_name was stashed on disconnect is
                // returning (e.g. it woke from sleep or was re-docked). Restore its
                // saved layout instead of a fresh one, and pull its windows back off
                // whatever monitor they were migrated to while it was gone.
                if let Some((stashed_ws, active_idx)) =
                    self.stashed_monitor_layouts.remove(&monitor.device_name)
                {
                    let returning: HashSet<u64> =
                        stashed_ws.iter().flat_map(|w| w.all_window_ids()).collect();
                    for ws_vec in self.workspaces.values_mut() {
                        for ws in ws_vec.iter_mut() {
                            for &wid in &returning {
                                let _ = ws.remove_window(wid);
                                ws.remove_floating(wid);
                            }
                        }
                    }
                    let clamped = active_idx.min(stashed_ws.len().saturating_sub(1));
                    info!(
                        "Restored {} stashed workspace(s) for reconnected monitor {} ({})",
                        stashed_ws.len(),
                        monitor.id,
                        monitor.device_name
                    );
                    self.workspaces.insert(monitor.id, stashed_ws);
                    self.active_workspace.insert(monitor.id, clamped);
                    continue;
                }
                let params = ScaledLayoutParams::from_config(
                    &self.config.layout,
                    &self.config.appearance,
                    monitor.scale_factor,
                    monitor.work_area.width,
                );
                let mut workspace = Workspace::with_directional_gaps(
                    params.gap,
                    params.outer_gap_left,
                    params.outer_gap_right,
                    params.outer_gap_top,
                    params.outer_gap_bottom,
                );
                workspace.set_default_column_width(params.default_column_width);
                workspace.set_tab_strip_reserve_px(params.tab_strip_reserve_px);
                workspace.set_centering_mode(self.config.layout.centering_mode.into());
                workspace.set_center_past_edges(self.config.layout.center_past_edges);
                workspace.set_reduce_motion(self.reduce_motion);
                workspace.set_scroll_animation(
                    self.config.animation.scroll_duration_ms,
                    self.config.animation.easing,
                );
                self.workspaces.insert(monitor.id, vec![workspace]);
                self.active_workspace.insert(monitor.id, 0);
                info!("Created workspace for new monitor {}", monitor.id);
            }
        }

        // Handle removed monitors - migrate ALL workspaces' windows to primary's active workspace
        for removed_id in old_ids.difference(&new_ids) {
            // The stable device_name and active index, captured before the
            // monitor info is dropped, so a reconnect can restore this layout.
            let device_name = self.monitors.get(removed_id).map(|m| m.device_name.clone());
            let active_idx = self.active_workspace.get(removed_id).copied().unwrap_or(0);
            if let Some(old_ws_vec) = self.workspaces.remove(removed_id) {
                // Collect tiled and floating windows separately to preserve their type
                let mut tiled_window_ids = Vec::new();
                let mut floating_windows = Vec::new();
                let mut minimized_ids = Vec::new();
                for old_workspace in &old_ws_vec {
                    for col in old_workspace.columns() {
                        for &wid in col.windows() {
                            tiled_window_ids.push(wid);
                            if old_workspace.is_minimized(wid) {
                                minimized_ids.push(wid);
                            }
                        }
                    }
                    for fw in old_workspace.floating_windows() {
                        floating_windows.push((fw.id, fw.rect, fw.pinned));
                        if old_workspace.is_minimized(fw.id) {
                            minimized_ids.push(fw.id);
                        }
                    }
                }
                if let Some(primary) = primary_id {
                    let primary_active_idx = self.active_workspace_idx(primary);
                    // Source monitor info is still available (removed from self.monitors later).
                    let source_wa = self.monitors.get(removed_id).map(|m| m.work_area);
                    let target_wa = self.monitors.get(&primary).map(|m| m.work_area);
                    if let Some(primary_ws) = self
                        .workspaces
                        .get_mut(&primary)
                        .and_then(|v| v.get_mut(primary_active_idx))
                    {
                        for window_id in &tiled_window_ids {
                            if let Err(e) = primary_ws.insert_window(*window_id, None) {
                                warn!("Failed to migrate tiled window {}: {}", window_id, e);
                            }
                        }
                        for (wid, rect, pinned) in &floating_windows {
                            let translated = match (source_wa, target_wa) {
                                (Some(src), Some(tgt)) => Rect::new(
                                    rect.x.saturating_add(tgt.x - src.x),
                                    rect.y.saturating_add(tgt.y - src.y),
                                    rect.width,
                                    rect.height,
                                )
                                .clamped_inside(tgt),
                                _ => *rect,
                            };
                            if let Err(e) = primary_ws.add_floating(*wid, translated) {
                                warn!("Failed to migrate floating window {}: {}", wid, e);
                            } else {
                                primary_ws.set_floating_pinned(*wid, *pinned);
                            }
                        }
                        // Restore minimized state for migrated windows
                        for wid in &minimized_ids {
                            primary_ws.mark_minimized(*wid);
                        }
                        info!(
                            "Migrated {} tiled + {} floating windows from removed monitor {} to primary",
                            tiled_window_ids.len(),
                            floating_windows.len(),
                            removed_id
                        );
                    }
                }
                // Stash this monitor's layout by stable device_name so a
                // reconnect restores it; the windows migrated above stay on
                // primary meanwhile and are pulled back when it returns.
                if let Some(name) = device_name {
                    if !tiled_window_ids.is_empty() || !floating_windows.is_empty() {
                        self.stashed_monitor_layouts
                            .insert(name, (old_ws_vec, active_idx));
                    }
                }
            }
            self.active_workspace.remove(removed_id);
            self.monitors.remove(removed_id);
        }

        // Update monitor info
        self.monitors = new_monitors.into_iter().map(|m| (m.id, m)).collect();

        // Re-apply scaled gaps to ALL existing workspaces — monitor DPI or work
        // area may have changed even if the monitor ID stayed the same (e.g.,
        // Windows scaling change, or work area resize from taskbar move).
        for (&monitor_id, ws_vec) in self.workspaces.iter_mut() {
            let scale = self
                .monitors
                .get(&monitor_id)
                .map(|m| m.scale_factor)
                .unwrap_or(1.0);
            let work_area = self
                .monitors
                .get(&monitor_id)
                .map(|m| m.work_area)
                .unwrap_or(Rect::new(
                    0,
                    0,
                    FALLBACK_VIEWPORT_WIDTH,
                    FALLBACK_WORK_AREA_HEIGHT,
                ));
            let viewport_width = work_area.width;
            let params = ScaledLayoutParams::from_config(
                &self.config.layout,
                &self.config.appearance,
                scale,
                viewport_width,
            );

            for workspace in ws_vec.iter_mut() {
                let old_gap = workspace.gap();
                let (old_ol, old_or, _, _) = workspace.outer_gaps();
                params.apply_to(workspace);
                workspace.rescale_column_widths(old_gap, old_ol, old_or, viewport_width);
                workspace.clamp_floating_to(work_area);
            }
        }

        // Update focused monitor if it was removed
        if !self.monitors.contains_key(&self.focused_monitor) {
            self.focused_monitor = primary_id.unwrap_or(0);
        }
    }

    /// Reconcile every managed window's minimized flag with the OS.
    ///
    /// A monitor sleep/wake cycle desyncs this: a stashed workspace restored on
    /// wake carries the pre-sleep minimized state, but Windows un-minimizes the
    /// windows when the monitor returns and those restore events land on the
    /// migrated copies (on the primary), which are then discarded. The result is
    /// a window that is visible on screen but still flagged minimized, so it is
    /// never tiled. The stale flag also persists in saved layout state across a
    /// restart, so this runs both after a display-change reconcile and once at
    /// startup, before the first layout is applied.
    pub(crate) fn resync_minimized_from_os(&mut self) {
        self.resync_minimized_with(leopardwm_platform_win32::window_minimized_state);
    }

    /// Managed windows that no monitor's *active* workspace owns, excluding
    /// minimized ones (whose restore geometry must not be rewritten) and the
    /// drag placeholder sentinel.
    pub(crate) fn windows_outside_active_layout(&self) -> Vec<u64> {
        let mut active: HashSet<u64> = HashSet::new();
        let mut minimized: HashSet<u64> = HashSet::new();
        for (monitor_id, ws_vec) in &self.workspaces {
            let active_idx = self.active_workspace_idx(*monitor_id);
            for (idx, workspace) in ws_vec.iter().enumerate() {
                for window_id in workspace.all_window_ids() {
                    if idx == active_idx {
                        active.insert(window_id);
                    }
                    if workspace.is_minimized(window_id) {
                        minimized.insert(window_id);
                    }
                }
            }
        }
        let mut outside: Vec<u64> = self
            .all_managed_window_ids()
            .into_iter()
            .filter(|window_id| {
                !active.contains(window_id)
                    && !minimized.contains(window_id)
                    && *window_id != crate::state::DRAG_PLACEHOLDER_HWND
            })
            .collect();
        outside.sort_unstable();
        outside.dedup();
        outside
    }

    /// Re-park every managed window that the next `apply_layout()` will not
    /// place.
    ///
    /// `apply_layout` only places each monitor's active workspace, and windows
    /// that left the layout are uncloaked again by `sync_cloak_state`, so an
    /// inactive workspace's window is kept invisible purely by its off-screen
    /// parking coordinates. Windows drags such windows back onto the desktop
    /// when the display topology changes, where they would stay visible on an
    /// arbitrary monitor with no overflow policy ever running for them.
    /// No-op while paused: a paused daemon must not move windows at all.
    pub(crate) fn repark_windows_outside_active_layout(&mut self) {
        if self.paused {
            return;
        }
        for window_id in self.windows_outside_active_layout() {
            // Placeholder hwnds in unit tests would otherwise reach real
            // desktop windows through this call.
            #[cfg(not(test))]
            let _ = leopardwm_platform_win32::move_window_offscreen(window_id);
            #[cfg(test)]
            let _ = window_id;
        }
    }

    /// Core of [`resync_minimized_from_os`], parameterized on the per-window OS
    /// state (`None` = dead window, left alone; `Some(minimized)` otherwise) so
    /// the flag-correction logic is testable without real window handles.
    pub(crate) fn resync_minimized_with(&mut self, os_minimized: impl Fn(u64) -> Option<bool>) {
        for ws_vec in self.workspaces.values_mut() {
            for ws in ws_vec.iter_mut() {
                for wid in ws.all_window_ids() {
                    match os_minimized(wid) {
                        Some(true) if !ws.is_minimized(wid) => {
                            ws.mark_minimized(wid);
                        }
                        Some(false) if ws.is_minimized(wid) => {
                            ws.mark_restored(wid);
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// Move the focused window to another monitor using an all-or-nothing update.
    ///
    /// The source and target workspaces are mutated on cloned snapshots first and
    /// committed only if both remove and insert operations succeed.
    pub(crate) fn move_focused_window_to_monitor_transactional(
        &mut self,
        target_monitor: MonitorId,
    ) -> std::result::Result<Option<u64>, String> {
        let source_monitor = self.focused_monitor;
        if source_monitor == target_monitor {
            return Ok(None);
        }

        // Prefer the OS-foreground window so floating windows can be moved too.
        let src_idx = self.active_workspace_idx(source_monitor);
        let tgt_idx = self.active_workspace_idx(target_monitor);

        let os_focus = self.previous_focused_hwnd.and_then(|hwnd| {
            self.workspaces
                .get(&source_monitor)
                .and_then(|v| v.get(src_idx))
                .filter(|ws| ws.contains_window(hwnd))
                .map(|_| hwnd)
        });
        let tiled_focus = self.focused_workspace().and_then(|ws| ws.focused_window());
        let Some(window_id) = os_focus.or(tiled_focus) else {
            return Ok(None);
        };

        let Some(source_ws_vec) = self.workspaces.get(&source_monitor) else {
            return Err(format!(
                "Source workspace missing for monitor {}",
                source_monitor
            ));
        };
        let Some(mut source_workspace) = source_ws_vec.get(src_idx).cloned() else {
            return Err(format!(
                "Source workspace index {} missing for monitor {}",
                src_idx, source_monitor
            ));
        };
        let Some(target_ws_vec) = self.workspaces.get(&target_monitor) else {
            return Err(format!(
                "Target workspace missing for monitor {}",
                target_monitor
            ));
        };
        let Some(mut target_workspace) = target_ws_vec.get(tgt_idx).cloned() else {
            return Err(format!(
                "Target workspace index {} missing for monitor {}",
                tgt_idx, target_monitor
            ));
        };

        let is_floating = source_workspace.is_floating(window_id);
        if is_floating {
            let floating = source_workspace
                .floating_windows()
                .iter()
                .find(|floating| floating.id == window_id)
                .cloned()
                .ok_or_else(|| format!("Floating window {} missing from source", window_id))?;
            let rect = floating.rect;
            // Translate floating coordinates from source to target monitor work area
            let translated_rect = match (
                self.monitors.get(&source_monitor),
                self.monitors.get(&target_monitor),
            ) {
                (Some(src_mon), Some(tgt_mon)) => Rect::new(
                    rect.x
                        .saturating_add(tgt_mon.work_area.x - src_mon.work_area.x),
                    rect.y
                        .saturating_add(tgt_mon.work_area.y - src_mon.work_area.y),
                    rect.width,
                    rect.height,
                )
                .clamped_inside(tgt_mon.work_area),
                _ => rect,
            };
            source_workspace.remove_floating(window_id);
            target_workspace
                .add_floating(window_id, translated_rect)
                .map_err(|e| format!("Failed to add floating window to target: {}", e))?;
            target_workspace.set_floating_pinned(window_id, floating.pinned);
        } else {
            source_workspace
                .remove_window(window_id)
                .map_err(|e| format!("Failed to remove window: {}", e))?;
            target_workspace
                .insert_window(window_id, None)
                .map_err(|e| format!("Failed to add window to target: {}", e))?;
        }

        let target_viewport = self.viewport_width_for(target_monitor);
        if !is_floating {
            target_workspace.ensure_focused_visible(target_viewport);
        }

        self.workspaces.get_mut(&source_monitor).unwrap()[src_idx] = source_workspace;
        self.workspaces.get_mut(&target_monitor).unwrap()[tgt_idx] = target_workspace;
        self.focused_monitor = target_monitor;
        Ok(Some(window_id))
    }

    /// The layout viewport for a monitor: its full work area. Single source of
    /// truth for the rect fed into `compute_placements*`; columns fill the work
    /// area edge to edge with no shared-edge inset.
    pub(crate) fn layout_viewport(&self, monitor_id: MonitorId) -> Rect {
        self.monitors
            .get(&monitor_id)
            .map(|m| m.work_area)
            .unwrap_or_else(|| Rect::new(0, 0, FALLBACK_VIEWPORT_WIDTH, FALLBACK_VIEWPORT_HEIGHT))
    }
}
