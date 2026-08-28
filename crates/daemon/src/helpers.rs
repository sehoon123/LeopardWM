//! Shared helper methods on AppState: window lookup, config application, snap suppression, pause.

use crate::config;
use crate::state::*;
use anyhow::Result;
use leopardwm_core_layout::{centered_rect_for_size, FloatingSize, Rect, Workspace};
#[cfg(not(test))]
use leopardwm_platform_win32::{is_excluded_tool_window_hwnd, is_window_alive_and_visible};
use leopardwm_platform_win32::{scale_px, MonitorId};
use tracing::{debug, info, warn};

/// Pre-scaled layout parameters for a specific monitor's DPI.
///
/// Config values are in logical pixels (96 DPI). This struct holds the
/// scaled values for a specific monitor, avoiding repeated scaling in
/// multiple call sites.
pub(crate) struct ScaledLayoutParams {
    pub gap: i32,
    pub outer_gap_left: i32,
    pub outer_gap_right: i32,
    pub outer_gap_top: i32,
    pub outer_gap_bottom: i32,
    pub default_column_width: i32,
    pub tab_strip_reserve_px: i32,
}

impl ScaledLayoutParams {
    /// Compute scaled layout parameters from config + monitor DPI + viewport width.
    pub fn from_config(
        layout: &config::LayoutConfig,
        appearance: &config::AppearanceConfig,
        scale_factor: f64,
        viewport_width: i32,
    ) -> Self {
        let gap = scale_px(layout.gap, scale_factor);
        let outer_gap_left = scale_px(layout.outer_gap_left, scale_factor);
        let outer_gap_right = scale_px(layout.outer_gap_right, scale_factor);
        let outer_gap_top = scale_px(layout.outer_gap_top, scale_factor);
        let outer_gap_bottom = scale_px(layout.outer_gap_bottom, scale_factor);
        // Reserve room for the strip PLUS the inter-element gap below
        // it, so the strip's bottom edge sits `gap` pixels above the
        // active tab — same spacing as between adjacent columns and
        // within a Vertical column. Reusing `layout.gap` keeps the
        // visual rhythm consistent across the workspace.
        let strip_with_gap = appearance.tab_strip_height as i32 + layout.gap.max(0);
        let tab_strip_reserve_px = scale_px(strip_with_gap, scale_factor);

        // Compute default column width using scaled gap values (mirrors LayoutConfig::default_column_width_px)
        let base = viewport_width
            .saturating_sub(outer_gap_left.max(0))
            .saturating_sub(outer_gap_right.max(0))
            .saturating_add(gap.max(0));
        let frac = layout.default_width_fraction();
        let default_column_width = (base as f64 * frac - gap as f64).floor().max(100.0) as i32;

        Self {
            gap,
            outer_gap_left,
            outer_gap_right,
            outer_gap_top,
            outer_gap_bottom,
            default_column_width,
            tab_strip_reserve_px,
        }
    }

    /// Apply scaled gap and width values to a workspace.
    pub fn apply_to(&self, workspace: &mut Workspace) {
        workspace.set_gap(self.gap);
        workspace.set_outer_gaps(
            self.outer_gap_left,
            self.outer_gap_right,
            self.outer_gap_top,
            self.outer_gap_bottom,
        );
        workspace.set_default_column_width(self.default_column_width);
        workspace.set_tab_strip_reserve_px(self.tab_strip_reserve_px);
    }
}

impl AppState {
    /// Look up window info for a given window handle.
    ///
    /// In production, calls `enumerate_windows()` and finds the matching entry.
    /// In tests, returns from the injected window info map if available.
    pub(crate) fn lookup_window_info(
        &self,
        hwnd: u64,
    ) -> Option<leopardwm_platform_win32::WindowInfo> {
        #[cfg(test)]
        {
            if let Some(info) = self.injected_window_info.get(&hwnd) {
                return Some(info.clone());
            }
        }
        leopardwm_platform_win32::get_window_info(hwnd)
    }

    /// Default size for a manually floated window, in logical pixels.
    pub(crate) fn default_floating_size(&self) -> FloatingSize {
        FloatingSize::new(
            self.config.layout.default_floating_width,
            self.config.layout.default_floating_height,
        )
    }

    /// Default size for a scratchpad window, in logical pixels.
    pub(crate) fn default_scratchpad_size(&self) -> FloatingSize {
        FloatingSize::new(
            self.config.layout.default_scratchpad_width,
            self.config.layout.default_scratchpad_height,
        )
    }

    /// Return a usable DPI scale for a monitor, falling back to 100% for a
    /// missing or malformed monitor record.
    fn monitor_scale_factor(&self, monitor_id: MonitorId) -> f64 {
        self.monitors
            .get(&monitor_id)
            .map(|monitor| monitor.scale_factor)
            .filter(|scale| scale.is_finite() && *scale > 0.0)
            .unwrap_or(1.0)
    }

    /// Preserve a physical dimension's logical size across monitor DPI.
    pub(crate) fn scale_px_between_monitors(
        &self,
        pixels: i32,
        source_monitor: MonitorId,
        target_monitor: MonitorId,
    ) -> i32 {
        let source_scale = self.monitor_scale_factor(source_monitor);
        let target_scale = self.monitor_scale_factor(target_monitor);
        ((pixels.max(1) as f64 / source_scale) * target_scale)
            .round()
            .clamp(1.0, i32::MAX as f64) as i32
    }

    /// Resolve the monitor containing a screen point without cloning or
    /// allocating monitor records. Drag mouse-move events call this at up to
    /// 60 Hz, so it deliberately walks the small monitor map in place.
    pub(crate) fn monitor_for_point(&self, x: i32, y: i32) -> Option<MonitorId> {
        self.monitors
            .values()
            .find(|monitor| monitor.contains_point(x, y))
            .map(|monitor| monitor.id)
    }

    /// Resolve the cursor-selected monitor for a title-bar drag, honoring the
    /// user setting that can keep every drop on its source monitor.
    pub(crate) fn monitor_for_drag_point(
        &self,
        x: i32,
        y: i32,
        source_monitor: MonitorId,
    ) -> MonitorId {
        if self.config.behavior.cross_monitor_drag {
            self.monitor_for_point(x, y).unwrap_or(source_monitor)
        } else {
            source_monitor
        }
    }

    /// Resolve the monitor containing a floating rectangle's center, falling
    /// back to the primary monitor when it lies outside every display.
    fn monitor_for_floating_rect(&self, rect: Rect) -> Option<MonitorId> {
        let center_x = rect.x.saturating_add(rect.width / 2);
        let center_y = rect.y.saturating_add(rect.height / 2);
        self.monitor_for_point(center_x, center_y).or_else(|| {
            self.monitors
                .values()
                .find(|monitor| monitor.is_primary)
                .map(|m| m.id)
        })
    }

    /// Convert a physical floating rectangle to a logical size using the DPI
    /// of the monitor that actually contains it, not its workspace owner.
    pub(crate) fn logical_floating_size_for_rect(&self, rect: Rect) -> FloatingSize {
        let scale = self
            .monitor_for_floating_rect(rect)
            .map(|monitor| self.monitor_scale_factor(monitor))
            .unwrap_or(1.0);
        FloatingSize::new(
            ((rect.width.max(1) as f64 / scale).round() as i32).max(1),
            ((rect.height.max(1) as f64 / scale).round() as i32).max(1),
        )
    }

    /// Scale a logical floating size for `monitor_id`, clamp it to that
    /// monitor's work area, and center it. Only size is carried across
    /// monitors; callers deliberately receive newly calculated coordinates.
    pub(crate) fn centered_rect_for_logical_floating_size(
        &self,
        monitor_id: MonitorId,
        logical_size: FloatingSize,
        margin: i32,
    ) -> Rect {
        let scale = self.monitor_scale_factor(monitor_id);
        let physical_size = FloatingSize::new(
            scale_px(logical_size.width, scale),
            scale_px(logical_size.height, scale),
        );
        let viewport = self
            .monitors
            .get(&monitor_id)
            .map(|monitor| monitor.work_area)
            .unwrap_or_else(|| Rect::new(0, 0, FALLBACK_VIEWPORT_WIDTH, FALLBACK_VIEWPORT_HEIGHT));
        centered_rect_for_size(viewport, physical_size, margin)
    }

    /// Return the saved physical rectangle for a managed floating window.
    pub(crate) fn floating_rect_for_window(&self, hwnd: u64) -> Option<Rect> {
        let (monitor_id, ws_idx) = self.find_window_workspace(hwnd)?;
        self.workspaces
            .get(&monitor_id)
            .and_then(|workspaces| workspaces.get(ws_idx))
            .and_then(|workspace| workspace.floating_rect(hwnd))
    }

    /// Update a managed floating rectangle and remember only its logical size.
    /// A scratchpad-owned float uses its own size memory; ordinary floats use
    /// the per-HWND session history.
    pub(crate) fn update_floating_geometry(&mut self, hwnd: u64, rect: Rect) -> bool {
        let Some((monitor_id, ws_idx)) = self.find_window_workspace(hwnd) else {
            return false;
        };
        let updated = self
            .workspaces
            .get_mut(&monitor_id)
            .and_then(|workspaces| workspaces.get_mut(ws_idx))
            .map(|workspace| workspace.update_floating(hwnd, rect))
            .unwrap_or(false);
        if !updated {
            return false;
        }
        let logical_size = self.logical_floating_size_for_rect(rect);

        if self
            .scratchpad
            .is_some_and(|scratchpad| scratchpad.window_id == hwnd)
        {
            if self.config.layout.remember_scratchpad_size {
                if let Some(scratchpad) = self.scratchpad.as_mut() {
                    scratchpad.last_size = Some(logical_size);
                }
            }
        } else if self.config.layout.remember_floating_sizes {
            self.floating_size_history.insert(hwnd, logical_size);
        }
        true
    }

    /// Capture the latest geometry for a managed floating window before it is
    /// removed from its workspace. A stored floating entry is required before
    /// probing DWM, so tiled windows can never seed floating-size history.
    pub(crate) fn snapshot_managed_floating_geometry(&mut self, hwnd: u64) -> Option<Rect> {
        // A hide/show, float/unfloat, or scratchpad transition must snapshot
        // LeopardWM's managed geometry, not DWM's asynchronously updated frame.
        // User-confirmed resizing is learned separately by the MoveSizeEnd path.
        let rect = self.floating_rect_for_window(hwnd)?;

        if self.update_floating_geometry(hwnd, rect) {
            Some(rect)
        } else {
            None
        }
    }

    /// Check whether a window ID is known to this state (managed or injected).
    ///
    /// Used by event validation to skip `is_valid_window` for windows we have
    /// info about, even if they aren't yet managed (e.g., during Created events).
    pub(crate) fn is_known_window(&self, wid: u64) -> bool {
        if self.find_window_workspace(wid).is_some() {
            return true;
        }
        #[cfg(test)]
        {
            if self.injected_window_info.contains_key(&wid) {
                return true;
            }
        }
        false
    }

    /// Apply configuration to all workspaces.
    pub(crate) fn apply_config(&mut self, config: config::Config) {
        // Config reload may turn off swap_chain_ghost_animation, change
        // monitor geometry assumptions, or simply re-evaluate behavior.
        // Cleanest contract: any in-flight ghost animation dies on
        // reload — the next transition starts from a clean slate.
        self.abort_active_ghost_transition();
        let old_border_on = self.config.appearance.active_border;
        self.compiled_rules = config.compile_window_rules();

        self.config = config;
        // Turning memory off is immediate and irreversible for the current
        // session; re-enabling starts learning from the next user resize.
        if !self.config.layout.remember_floating_sizes {
            self.floating_size_history.clear();
        }
        if !self.config.layout.remember_scratchpad_size {
            if let Some(scratchpad) = self.scratchpad.as_mut() {
                scratchpad.last_size = None;
            }
        }

        // Update scroll modifier for the gesture hook
        leopardwm_platform_win32::set_scroll_modifier(&self.config.hotkeys.scroll_modifier);

        self.refresh_high_contrast();

        // Handle border transitions with new config values
        if let Some(hwnd) = self.previous_focused_hwnd {
            if self.config.appearance.active_border {
                self.show_border(hwnd);
            } else if old_border_on {
                self.hide_border();
            }
        } else if !self.config.appearance.active_border && old_border_on {
            self.hide_border();
        }
        self.update_tab_strip();

        for (&monitor_id, ws_vec) in self.workspaces.iter_mut() {
            let scale = self
                .monitors
                .get(&monitor_id)
                .map(|m| m.scale_factor)
                .unwrap_or(1.0);
            let viewport_width = self
                .monitors
                .get(&monitor_id)
                .map(|m| m.work_area.width)
                .unwrap_or(FALLBACK_VIEWPORT_WIDTH);
            let params = ScaledLayoutParams::from_config(
                &self.config.layout,
                &self.config.appearance,
                scale,
                viewport_width,
            );

            for workspace in ws_vec.iter_mut() {
                // Read the previously-applied scaled gap values from the workspace
                // (not the raw config values) so rescale_column_widths works correctly.
                let old_gap = workspace.gap();
                let (old_ol, old_or, _, _) = workspace.outer_gaps();

                params.apply_to(workspace);

                // Rescale column widths to preserve fractions under new gap values
                workspace.rescale_column_widths(old_gap, old_ol, old_or, viewport_width);

                workspace.set_centering_mode(self.config.layout.centering_mode.into());
                workspace.set_center_past_edges(self.config.layout.center_past_edges);
                workspace.set_scroll_animation(
                    self.config.animation.scroll_duration_ms,
                    self.config.animation.easing,
                );

                // Recalculate scroll offset for new gap values so all columns
                // are positioned correctly (not just the rightmost ones).
                workspace.ensure_focused_visible_animated(viewport_width);
            }
        }

        // Re-evaluate window rules for already-managed windows so that
        // newly added/changed rules take effect without restart.
        self.reapply_window_rules();

        // Pick up previously-ignored windows that should now be tiled/floated.
        // Hermetic in tests: `enumerate_and_add_windows` reads injected fixtures
        // there rather than the live desktop.
        if let Ok(added) = self.enumerate_and_add_windows() {
            if added > 0 {
                info!("Config reload: tiled {} previously-ignored windows", added);
            }
        }

        // Re-check animation state (accessibility setting + power state)
        self.refresh_reduce_motion();

        // Handle snap layout config change (skip when paused — pause already restored all)
        if !self.paused {
            if self.config.behavior.disable_snap_layouts {
                self.disable_snap_for_all_tiled_windows();
            } else {
                self.restore_snap_for_all_windows();
            }
        }

        // Apply a toggled hide_offscreen_taskbar_buttons setting live (restores
        // all buttons when turned off, re-hides off-view ones when turned on).
        self.sync_taskbar_buttons();

        info!(
            "Configuration applied to all {} workspaces",
            self.workspaces.len()
        );
    }

    /// Collect all managed window IDs across all workspaces.
    ///
    /// Returns tiled and floating window IDs from every monitor's workspace.
    pub(crate) fn all_managed_window_ids(&self) -> Vec<u64> {
        let mut ids = Vec::new();
        for ws_vec in self.workspaces.values() {
            for workspace in ws_vec {
                ids.extend(workspace.all_window_ids());
            }
        }
        ids
    }

    /// Keep a window's taskbar button iff it's actually visible in a viewport:
    /// hidden when on an inactive workspace OR scrolled off-viewport on the
    /// active one; shown when visible (and always for floating/minimized
    /// windows, which the user still reaches via the taskbar). External windows
    /// can't be hidden from the taskbar by cloaking or off-screen position, so
    /// this drives `ITaskbarList` directly. Idempotent and change-gated in the
    /// controller, so it's cheap to call after any layout/scroll change.
    pub(crate) fn sync_taskbar_buttons(&self) {
        use leopardwm_core_layout::Visibility;
        use leopardwm_platform_win32::taskbar::{taskbar_hide, taskbar_show};
        // Disabled: make sure no button stays hidden (restores any we hid before
        // the user turned the option off), then leave the taskbar alone.
        if !self.config.behavior.hide_offscreen_taskbar_buttons {
            for ws_vec in self.workspaces.values() {
                for workspace in ws_vec {
                    for wid in workspace.all_window_ids() {
                        taskbar_show(wid);
                    }
                }
            }
            return;
        }
        for (&monitor, ws_vec) in &self.workspaces {
            let active = self.active_workspace_idx(monitor);
            let viewport = self.layout_viewport(monitor);
            for (idx, workspace) in ws_vec.iter().enumerate() {
                if idx != active {
                    for wid in workspace.all_window_ids() {
                        taskbar_hide(wid);
                    }
                    continue;
                }
                // Active workspace: a tiled window keeps its button only while
                // it's visible in the viewport; floating and minimized windows
                // always keep theirs.
                let visible: std::collections::HashSet<u64> = workspace
                    .compute_placements(viewport)
                    .iter()
                    .filter(|p| p.visibility == Visibility::Visible)
                    .map(|p| p.window_id)
                    .collect();
                for wid in workspace.all_window_ids() {
                    let keep = workspace.is_floating(wid)
                        || workspace.is_minimized(wid)
                        || visible.contains(&wid);
                    if keep {
                        taskbar_show(wid);
                    } else {
                        taskbar_hide(wid);
                    }
                }
            }
        }
    }

    /// Remove managed windows that are no longer valid or visible.
    ///
    /// Some apps (e.g., Electron close-to-tray) hide windows without firing
    /// Win32 destroy/hide events. This reconciliation pass detects and removes them.
    ///
    /// Skipped in test builds because test window IDs are not real Win32 handles.
    pub(crate) fn prune_stale_windows(&mut self) {
        #[cfg(test)]
        return;

        #[cfg(not(test))]
        {
            let mut stale: Vec<(MonitorId, usize, u64)> = Vec::new();
            for (&monitor_id, ws_vec) in &self.workspaces {
                for (ws_idx, workspace) in ws_vec.iter().enumerate() {
                    for &wid in &workspace.all_window_ids() {
                        let alive_visible = is_window_alive_and_visible(wid);
                        let gone = !alive_visible && !workspace.is_minimized(wid);
                        let unmanageable = alive_visible && is_excluded_tool_window_hwnd(wid);
                        if gone || unmanageable {
                            stale.push((monitor_id, ws_idx, wid));
                        }
                    }
                }
            }
            for (monitor_id, ws_idx, wid) in &stale {
                if let Some(workspace) = self
                    .workspaces
                    .get_mut(monitor_id)
                    .and_then(|v| v.get_mut(*ws_idx))
                {
                    let was_floating = workspace.remove_floating(*wid);
                    if !was_floating {
                        let _ = workspace.remove_window(*wid);
                    }
                    self.restore_snap_for_window(*wid);
                    self.window_managed_at.remove(wid);
                    info!("Pruned stale window {} from monitor {}", wid, monitor_id);
                }
            }

            // Evict orphaned entries from window_managed_at whose HWNDs are
            // no longer managed in any workspace (catches all removal paths).
            if !self.window_managed_at.is_empty() || !self.window_last_maximized_at.is_empty() {
                let managed: std::collections::HashSet<u64> = self
                    .workspaces
                    .values()
                    .flat_map(|ws_vec| ws_vec.iter().flat_map(|ws| ws.all_window_ids()))
                    .collect();
                self.window_managed_at
                    .retain(|hwnd, _| managed.contains(hwnd));
                self.window_last_maximized_at
                    .retain(|hwnd, _| managed.contains(hwnd));
            }
        }
    }

    /// Find which workspace contains a window.
    /// Returns `(monitor_id, workspace_index)` so callers can index into the correct workspace.
    ///
    /// A managed window lives in exactly one workspace, so the first match is
    /// authoritative. The focused monitor's active workspace is checked first
    /// because the hot callers (border/focus/layout for the focused window)
    /// almost always hit it, avoiding a linear `contains_window` scan across
    /// every monitor's nine workspaces on each animation frame.
    pub(crate) fn find_window_workspace(&self, window_id: u64) -> Option<(MonitorId, usize)> {
        let focused_active_idx = self.active_workspace_idx(self.focused_monitor);
        if self
            .workspaces
            .get(&self.focused_monitor)
            .and_then(|ws_vec| ws_vec.get(focused_active_idx))
            .is_some_and(|workspace| workspace.contains_window(window_id))
        {
            return Some((self.focused_monitor, focused_active_idx));
        }
        for (monitor_id, ws_vec) in &self.workspaces {
            for (idx, workspace) in ws_vec.iter().enumerate() {
                if *monitor_id == self.focused_monitor && idx == focused_active_idx {
                    continue;
                }
                if workspace.contains_window(window_id) {
                    return Some((*monitor_id, idx));
                }
            }
        }
        None
    }

    /// Pixel width of the tiled column currently holding `window_id` on
    /// `(monitor, ws_idx)`, or `None` if the window isn't tiled there. Used to
    /// carry a window's chosen width across a workspace move so it re-tiles at
    /// the same width instead of snapping back to the default column width.
    pub(crate) fn tiled_column_width(
        &self,
        monitor: MonitorId,
        ws_idx: usize,
        window_id: u64,
    ) -> Option<i32> {
        let ws = self.workspaces.get(&monitor)?.get(ws_idx)?;
        let (col, _) = ws.find_window_location(window_id)?;
        ws.column(col).map(|c| c.width())
    }

    /// The column index `window_id` occupies on `(monitor, ws_idx)` plus one
    /// same-column sibling (any other window sharing it), if it is tiled there.
    /// The sibling anchors the column so a later restore survives index shifts
    /// from columns added or removed in the meantime.
    pub(crate) fn tiled_column_origin(
        &self,
        monitor: MonitorId,
        ws_idx: usize,
        window_id: u64,
    ) -> Option<(usize, Option<u64>)> {
        let ws = self.workspaces.get(&monitor)?.get(ws_idx)?;
        let (col, _) = ws.find_window_location(window_id)?;
        let sibling = ws
            .column(col)?
            .windows()
            .iter()
            .copied()
            .find(|&w| w != window_id);
        Some((col, sibling))
    }

    /// Get the rectangle of the focused column for snap hint display.
    ///
    /// Returns the absolute screen position of the focused column.
    pub(crate) fn get_focused_column_rect(&self) -> Option<Rect> {
        let workspace = self.focused_workspace()?;
        self.monitors.get(&self.focused_monitor)?;
        let placements = workspace.compute_placements(self.layout_viewport(self.focused_monitor));

        // Find the placement for the focused window
        let focused_hwnd = workspace.focused_window()?;
        placements
            .iter()
            .find(|p| p.window_id == focused_hwnd)
            .map(|p| p.rect)
    }

    // =========================================================================
    // Snap layout suppression helpers
    // =========================================================================

    /// Remove WS_MAXIMIZEBOX from a tiled window to disable Snap Layouts.
    /// Only acts if `disable_snap_layouts` is enabled and the window isn't already tracked.
    pub(crate) fn disable_snap_for_window(&mut self, hwnd: u64) {
        if !self.config.behavior.disable_snap_layouts {
            return;
        }
        if self.snap_disabled_hwnds.contains(&hwnd) {
            return;
        }
        match leopardwm_platform_win32::remove_maximizebox(hwnd) {
            Ok(true) => {
                self.snap_disabled_hwnds.insert(hwnd);
                debug!("Removed WS_MAXIMIZEBOX from window {}", hwnd);
            }
            Ok(false) => {
                debug!("Window {} already lacks WS_MAXIMIZEBOX, skipping", hwnd);
            }
            Err(e) => {
                warn!("Failed to remove WS_MAXIMIZEBOX for window {}: {}", hwnd, e);
            }
        }
    }

    /// Restore WS_MAXIMIZEBOX on a window when it leaves tiled management.
    pub(crate) fn restore_snap_for_window(&mut self, hwnd: u64) {
        if !self.snap_disabled_hwnds.remove(&hwnd) {
            return;
        }
        match leopardwm_platform_win32::restore_maximizebox(hwnd) {
            Ok(_) => {}
            Err(e) => {
                debug!(
                    "Failed to restore WS_MAXIMIZEBOX for window {}: {}",
                    hwnd, e
                );
            }
        }
    }

    /// Restore WS_MAXIMIZEBOX on all tracked windows (bulk).
    pub(crate) fn restore_snap_for_all_windows(&mut self) {
        let hwnds: Vec<u64> = self.snap_disabled_hwnds.drain().collect();
        if !hwnds.is_empty() {
            leopardwm_platform_win32::restore_maximizebox_all(&hwnds);
            info!("Restored WS_MAXIMIZEBOX for {} window(s)", hwnds.len());
        }
    }

    /// Apply snap layout suppression to all currently tiled (non-floating) windows.
    pub(crate) fn disable_snap_for_all_tiled_windows(&mut self) {
        if !self.config.behavior.disable_snap_layouts {
            return;
        }
        let mut tiled_ids = Vec::new();
        for ws_vec in self.workspaces.values() {
            for workspace in ws_vec {
                for col in workspace.columns() {
                    for &wid in col.windows() {
                        tiled_ids.push(wid);
                    }
                }
            }
        }
        for hwnd in tiled_ids {
            self.disable_snap_for_window(hwnd);
        }
    }

    /// Toggle paused state for tiling operations.
    ///
    /// When resuming, this immediately reapplies layout so windows snap back
    /// without waiting for another command/event. If resume reapply fails,
    /// paused state is restored to avoid claiming a healthy resumed mode.
    pub(crate) fn toggle_pause(&mut self, source: &str) -> Result<()> {
        // Establish an ordering edge before changing lifecycle state. Without a
        // barrier, an already-running frame can re-park sources and republish
        // previews after pause has restored the desktop.
        if self
            .animation_worker_control
            .as_ref()
            .is_some_and(|worker| !worker.wait_for_barrier(std::time::Duration::from_millis(750)))
        {
            return Err(anyhow::anyhow!(
                "Could not pause: animation worker barrier timed out"
            ));
        }
        self.settle_scroll_animations();
        self.cancel_layout_transition_for_exact_landing()?;
        self.abort_active_ghost_transition();
        let was_paused = self.paused;
        self.paused = !was_paused;
        info!(
            "Tiling {} via {}",
            if self.paused { "paused" } else { "resumed" },
            source
        );
        if self.paused {
            // Restore WS_MAXIMIZEBOX so windows behave normally while paused
            self.restore_snap_for_all_windows();
            // Release monitor-overflow clipping too. apply_layout no-ops while
            // paused, so a boundary window would otherwise keep a region that
            // hides part of it (and swallows clicks there) for as long as the
            // pause lasts. Resuming re-applies the layout, which re-installs
            // exactly the clips the current geometry needs.
            leopardwm_platform_win32::restore_all_window_regions();
            // Preview is optional to true tiling cleanup. Revoke/hide it first;
            // a later handle/input acknowledgement failure is degraded health,
            // not a reason to report that pause failed after state already
            // changed or to skip border/tab cleanup.
            leopardwm_platform_win32::thumbnail::invalidate_persistent_preview_surface();
            match leopardwm_platform_win32::thumbnail::clear_persistent_previews_best_effort() {
                Ok(true) => {}
                Ok(false) => warn!("Pause preview cleanup deferred behind active placement"),
                Err(error) => warn!("Pause preview cleanup degraded: {error}"),
            }
            self.hide_border();
            self.hide_tab_strip();
            // Hide any visible drag ghost overlay
            self.pending_drag_hint = Some(crate::state::DragHintAction::Hide);
        } else {
            self.pending_layout_apply_timeout_report = None;
            if let Err(err) = self.apply_layout() {
                self.paused = was_paused;
                warn!(
                    "Resume apply failed via {}; restoring paused state: {}",
                    source, err
                );
                return Err(err);
            }
            // Re-apply snap suppression after resuming
            self.disable_snap_for_all_tiled_windows();
            self.sync_foreground_window();
        }
        Ok(())
    }
}

#[cfg(test)]
#[cfg(test)]
mod floating_capture_tests {
    #[test]
    fn transition_snapshot_uses_only_managed_geometry() {
        let source = include_str!("helpers.rs");
        let start = source
            .find("pub(crate) fn snapshot_managed_floating_geometry")
            .expect("snapshot helper must exist");
        let tail = &source[start..];
        let end = tail
            .find("\n    }\n")
            .map_or(tail.len(), |idx| idx + "\n    }\n".len());
        let body = &tail[..end];
        let forbidden_probe = ["get_window_", "visible_rect"].concat();

        assert!(body.contains("floating_rect_for_window(hwnd)"));
        assert!(!body.contains(&forbidden_probe));
    }
}
