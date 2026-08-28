//! Window rule evaluation and application: tile/float/ignore decisions and rule-driven enumeration.

use crate::config;
use crate::state::*;
use anyhow::Result;
use leopardwm_core_layout::Rect;
use leopardwm_platform_win32::{
    find_monitor_for_rect, get_process_executable, scale_px, MonitorId,
};
use tracing::{debug, info, warn};

impl AppState {
    /// Re-evaluate window rules for all managed windows.
    ///
    /// Moves windows between tiled/floating/ignored states based on current rules.
    pub(crate) fn reapply_window_rules(&mut self) {
        // Rules can change ownership, so no transition frame may still be
        // placing the pre-rule model when the new model is committed.
        if let Err(error) = self.prepare_workspace_ownership_change() {
            warn!("Deferring rule reapplication until layout ownership is safe: {error}");
            return;
        }

        // Collect all managed windows with their current state
        let mut transitions: Vec<(u64, MonitorId, usize, config::WindowAction, bool)> = Vec::new();

        for (&monitor_id, ws_vec) in &self.workspaces {
            for (ws_idx, workspace) in ws_vec.iter().enumerate() {
                for wid in workspace.all_window_ids() {
                    let is_floating = workspace.is_floating(wid);
                    if let Some(win_info) = self.lookup_window_info(wid) {
                        let executable =
                            get_process_executable(win_info.process_id).unwrap_or_default();
                        let action = self.evaluate_window_rules(
                            &win_info.class_name,
                            &win_info.title,
                            &executable,
                        );
                        transitions.push((wid, monitor_id, ws_idx, action, is_floating));
                    }
                }
            }
        }

        // Pre-compute floating rects before mutating workspaces (avoids borrow conflicts)
        let float_rects: std::collections::HashMap<u64, Rect> = transitions
            .iter()
            .filter(|(_, _, _, action, is_floating)| {
                *action == config::WindowAction::Float && !is_floating
            })
            .filter_map(|(wid, monitor_id, _, _, _)| {
                let win_info = self.lookup_window_info(*wid)?;
                let executable = get_process_executable(win_info.process_id).unwrap_or_default();
                let rect = self.get_floating_rect_from_rules(
                    &win_info.class_name,
                    &win_info.title,
                    &executable,
                    &win_info.rect,
                    Some(*monitor_id),
                );
                Some((*wid, rect))
            })
            .collect();

        for (wid, monitor_id, ws_idx, action, is_floating) in transitions {
            match action {
                config::WindowAction::Float if !is_floating => {
                    let viewport = self
                        .monitors
                        .get(&monitor_id)
                        .map(|m| m.work_area)
                        .unwrap_or_else(|| {
                            Rect::new(0, 0, FALLBACK_VIEWPORT_WIDTH, FALLBACK_VIEWPORT_HEIGHT)
                        });
                    let rect = float_rects.get(&wid).copied().unwrap_or_else(|| {
                        Rect::new(
                            viewport.x + (viewport.width - 800) / 2,
                            viewport.y + (viewport.height - 600) / 2,
                            800,
                            600,
                        )
                    });
                    if self.rule_tile_to_float_transaction(monitor_id, ws_idx, wid, rect) {
                        // `toggle_floating` recorded the core float origin before
                        // detaching, so a later Tile rule restores the chosen
                        // column width/position rather than a default insertion.
                        self.restore_snap_for_window(wid);
                        info!("Rule change: moved window {} to floating", wid);
                    } else {
                        warn!("Rule change could not move window {} to floating", wid);
                    }
                }
                config::WindowAction::Tile if is_floating => {
                    if self.rule_float_to_tile_transaction(monitor_id, ws_idx, wid) {
                        self.disable_snap_for_window(wid);
                        info!("Rule change: moved window {} to tiled", wid);
                    } else {
                        warn!("Rule change could not move window {} to tiled", wid);
                    }
                }
                config::WindowAction::Ignore => {
                    // Release every physical receipt while model ownership still
                    // exists. Dropping the workspace entry first made a parked /
                    // cloaked / clipped HWND permanently unreachable by layout
                    // recovery and shutdown.
                    if let Err(error) = self.release_window_for_ignore(wid) {
                        warn!(
                            "Rule change retained management for {} because physical release failed: {}",
                            wid, error
                        );
                        continue;
                    }
                    self.restore_snap_for_window(wid);
                    leopardwm_platform_win32::taskbar::taskbar_show(wid);
                    let removed = self
                        .workspaces
                        .get_mut(&monitor_id)
                        .and_then(|v| v.get_mut(ws_idx))
                        .is_some_and(|workspace| {
                            if is_floating {
                                workspace.remove_floating(wid)
                            } else {
                                workspace.remove_window(wid).is_ok()
                            }
                        });
                    if removed {
                        self.window_managed_at.remove(&wid);
                        self.window_last_maximized_at.remove(&wid);
                        info!("Rule change: unmanaged window {} (ignore)", wid);
                    } else {
                        warn!("Rule change retained bookkeeping for {} after removal failed", wid);
                    }
                }
                _ => {} // No change needed
            }
        }
    }

    /// Move a tiled window to floating on a cloned workspace. The core
    /// `toggle_floating` transaction captures its origin before removal; direct
    /// remove/add calls cannot reconstruct that information later.
    fn rule_tile_to_float_transaction(
        &mut self,
        monitor_id: MonitorId,
        workspace_idx: usize,
        wid: u64,
        rect: Rect,
    ) -> bool {
        let Some(mut candidate) = self
            .workspaces
            .get(&monitor_id)
            .and_then(|workspaces| workspaces.get(workspace_idx))
            .cloned()
        else {
            return false;
        };
        let prior_focus = candidate.focused_window();
        if candidate.focus_window(wid).is_err() || candidate.toggle_floating(rect) != Some(wid) {
            return false;
        }
        if let Some(prior_focus) = prior_focus.filter(|focused| *focused != wid) {
            let _ = candidate.focus_window(prior_focus);
        }
        if let Some(workspaces) = self.workspaces.get_mut(&monitor_id) {
            if let Some(workspace) = workspaces.get_mut(workspace_idx) {
                *workspace = candidate;
                return true;
            }
        }
        false
    }

    /// Reverse a rule-driven float using the origin recorded by the matching
    /// tiled-to-float transaction, while preserving unrelated tiled focus.
    fn rule_float_to_tile_transaction(
        &mut self,
        monitor_id: MonitorId,
        workspace_idx: usize,
        wid: u64,
    ) -> bool {
        let Some(mut candidate) = self
            .workspaces
            .get(&monitor_id)
            .and_then(|workspaces| workspaces.get(workspace_idx))
            .cloned()
        else {
            return false;
        };
        let prior_focus = candidate.focused_window();
        if !candidate.unfloat_window(wid) {
            return false;
        }
        if let Some(prior_focus) = prior_focus.filter(|focused| *focused != wid) {
            let _ = candidate.focus_window(prior_focus);
        }
        if let Some(workspaces) = self.workspaces.get_mut(&monitor_id) {
            if let Some(workspace) = workspaces.get_mut(workspace_idx) {
                *workspace = candidate;
                return true;
            }
        }
        false
    }

    /// Physically release a window before an Ignore rule drops the model record
    /// that owns its recovery receipts. `restore_window_moved_offscreen` also
    /// clears a LeopardWM-owned region for this HWND.
    fn release_window_for_ignore(&self, wid: u64) -> Result<()> {
        #[cfg(test)]
        {
            if self.injected_scratchpad_park_failure {
                return Err(anyhow::anyhow!("injected ignore release failure"));
            }
            let _ = wid;
            return Ok(());
        }
        #[cfg(not(test))]
        {
            leopardwm_platform_win32::dwm_uncloak_window(wid);
            leopardwm_platform_win32::restore_window_moved_offscreen(wid)
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            leopardwm_platform_win32::show_window_no_activate(wid)
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            Ok(())
        }
    }

    /// Windows the OS reports as manageable.
    ///
    /// Unit tests inject their fixtures through `injected_window_info` instead of
    /// enumerating the developer's live desktop: an `AppState` in a test is a
    /// fixture, and pulling real windows into it made assertions depend on
    /// whatever happened to be open (and let config reload strip
    /// `WS_MAXIMIZEBOX` from them).
    fn manageable_windows(&self) -> Result<Vec<leopardwm_platform_win32::WindowInfo>> {
        #[cfg(test)]
        {
            let mut windows: Vec<_> = self.injected_window_info.values().cloned().collect();
            windows.sort_by_key(|info| info.hwnd);
            Ok(windows)
        }
        #[cfg(not(test))]
        leopardwm_platform_win32::enumerate_windows().map_err(anyhow::Error::from)
    }

    /// Enumerate windows and add them to the appropriate workspace based on position.
    pub(crate) fn enumerate_and_add_windows(&mut self) -> Result<usize> {
        let windows = self.manageable_windows()?;
        let monitors: Vec<_> = self.monitors.values().cloned().collect();
        let mut added = 0;

        for win_info in windows {
            let executable = get_process_executable(win_info.process_id).unwrap_or_default();

            let action =
                self.evaluate_window_rules(&win_info.class_name, &win_info.title, &executable);
            let rule_matched = self
                .matched_rule(&win_info.class_name, &win_info.title, &executable)
                .is_some();

            if action == config::WindowAction::Ignore {
                debug!(
                    "Ignoring window by rule: {} ({})",
                    win_info.title, win_info.class_name
                );
                continue;
            }

            // Windows restored from the persisted snapshot are already managed
            // (placed by restore_workspace_structure before this enumerate) and
            // get skipped below. Genuinely-new windows land on the monitor their
            // current on-screen position maps to.
            let monitor_id = find_monitor_for_rect(&monitors, &win_info.rect)
                .map(|m| m.id)
                .unwrap_or(self.focused_monitor);

            // Get floating rect before borrowing workspace mutably (to avoid borrow conflict)
            let floating_rect = if action == config::WindowAction::Float {
                Some(self.get_floating_rect_from_rules(
                    &win_info.class_name,
                    &win_info.title,
                    &executable,
                    &win_info.rect,
                    Some(monitor_id),
                ))
            } else {
                None
            };

            // Skip windows already managed on any workspace (including inactive ones)
            // to prevent duplicates during config reload re-enumeration.
            if self.find_window_workspace(win_info.hwnd).is_some() {
                continue;
            }

            // Elevated window the non-elevated daemon can't reposition (UIPI):
            // skip + notify instead of reserving a column we can't fill. Mirrors
            // the live-create path; covers windows already open at startup and
            // any seen via `lwm refresh`.
            #[cfg(not(test))]
            if self.skip_if_elevation_blocked(
                win_info.hwnd,
                win_info.process_id,
                &win_info.title,
                &win_info.class_name,
            ) {
                continue;
            }

            // No user rule matched and the window has a classic dialog shape (a
            // title bar but no minimize or maximize button): leave it floating
            // instead of reserving a column. Mirrors the live-create path so a
            // dialog already open at startup or seen via `lwm refresh` is treated
            // the same. A user Tile/Float rule overrides this.
            if !rule_matched && leopardwm_platform_win32::is_dialog_like_window(win_info.hwnd) {
                debug!(
                    "Leaving dialog-like window unmanaged: {} ({})",
                    win_info.title, win_info.class_name
                );
                continue;
            }

            // New windows land on the monitor's active workspace.
            let target_idx = self.active_workspace_idx(monitor_id);
            let _ = self.ensure_workspace_exists(monitor_id, target_idx);
            if let Some(workspace) = self
                .workspaces
                .get_mut(&monitor_id)
                .and_then(|v| v.get_mut(target_idx))
            {
                match action {
                    config::WindowAction::Float => {
                        // Use rule dimensions or default to centered 800x600 window
                        let rule_rect = floating_rect.unwrap_or_else(|| {
                            let viewport = self
                                .monitors
                                .get(&monitor_id)
                                .map(|m| m.work_area)
                                .unwrap_or_else(|| {
                                    Rect::new(
                                        0,
                                        0,
                                        FALLBACK_VIEWPORT_WIDTH,
                                        FALLBACK_VIEWPORT_HEIGHT,
                                    )
                                });
                            Rect::new(
                                viewport.x + (viewport.width - 800) / 2,
                                viewport.y + (viewport.height - 600) / 2,
                                800,
                                600,
                            )
                        });

                        match workspace.add_floating(win_info.hwnd, rule_rect) {
                            Ok(()) => {
                                self.window_managed_at
                                    .insert(win_info.hwnd, std::time::Instant::now());
                                info!(
                                    "Added floating window: {} ({}) to monitor {} - {}x{}",
                                    win_info.title,
                                    win_info.class_name,
                                    monitor_id,
                                    rule_rect.width,
                                    rule_rect.height
                                );
                                added += 1;
                            }
                            Err(e) => {
                                warn!("Failed to add floating window {}: {}", win_info.hwnd, e);
                            }
                        }
                    }
                    config::WindowAction::Tile => {
                        match workspace.insert_window(win_info.hwnd, None) {
                            Ok(()) => {
                                self.window_managed_at
                                    .insert(win_info.hwnd, std::time::Instant::now());
                                self.disable_snap_for_window(win_info.hwnd);
                                info!(
                                    "Added tiled window: {} ({}) to monitor {} - {}x{}",
                                    win_info.title,
                                    win_info.class_name,
                                    monitor_id,
                                    win_info.rect.width,
                                    win_info.rect.height
                                );
                                added += 1;
                            }
                            Err(e) => {
                                warn!("Failed to add window {}: {}", win_info.hwnd, e);
                            }
                        }
                    }
                    config::WindowAction::Ignore => unreachable!(), // Handled above
                }
            }
        }

        Ok(added)
    }

    /// Evaluate window rules and return the action for a window.
    pub(crate) fn evaluate_window_rules(
        &self,
        class_name: &str,
        title: &str,
        executable: &str,
    ) -> config::WindowAction {
        self.matched_rule(class_name, title, executable)
            .map(|r| r.action)
            .unwrap_or(config::WindowAction::Tile)
    }

    /// The first window rule matching this window, if any (first match wins).
    pub(crate) fn matched_rule(
        &self,
        class_name: &str,
        title: &str,
        executable: &str,
    ) -> Option<&config::CompiledWindowRule> {
        self.compiled_rules
            .iter()
            .find(|rule| rule.matches(class_name, title, executable))
    }

    /// Get the floating rect for a window based on rules.
    ///
    /// Rule-defined `width`/`height` are config values (logical pixels) and
    /// are scaled by the monitor's DPI factor. The `monitor_id` parameter
    /// is used to look up the scale factor; pass `None` to skip scaling.
    pub(crate) fn get_floating_rect_from_rules(
        &self,
        class_name: &str,
        title: &str,
        executable: &str,
        original_rect: &leopardwm_core_layout::Rect,
        monitor_id: Option<MonitorId>,
    ) -> leopardwm_core_layout::Rect {
        let scale = monitor_id
            .and_then(|id| self.monitors.get(&id))
            .map(|m| m.scale_factor)
            .unwrap_or(1.0);
        for rule in &self.compiled_rules {
            if rule.matches(class_name, title, executable) {
                // Only scale rule-provided dimensions (config logical pixels).
                // If a dimension is not specified, use the original rect value
                // which is already in physical pixels from the OS.
                let width = rule
                    .width
                    .map(|w| scale_px(w, scale))
                    .unwrap_or(original_rect.width);
                let height = rule
                    .height
                    .map(|h| scale_px(h, scale))
                    .unwrap_or(original_rect.height);
                return leopardwm_core_layout::Rect::new(
                    original_rect.x,
                    original_rect.y,
                    width,
                    height,
                );
            }
        }
        *original_rect
    }
}

#[cfg(test)]
mod transaction_tests {
    use super::*;
    use crate::config::Config;
    use leopardwm_platform_win32::MonitorInfo;

    fn monitor() -> MonitorInfo {
        MonitorInfo {
            id: 1,
            rect: Rect::new(0, 0, 1920, 1080),
            work_area: Rect::new(0, 0, 1920, 1040),
            is_primary: true,
            device_name: "DISPLAY1".into(),
            scale_factor: 1.0,
        }
    }

    fn rule(action: config::WindowAction) -> config::WindowRule {
        config::WindowRule {
            match_class: Some("RuleTarget".into()),
            match_title: None,
            match_executable: None,
            action,
            width: Some(800),
            height: Some(600),
            corner_style: None,
            open_on_workspace: None,
            open_maximized: false,
            column_width: None,
            open_in_column: None,
            sticky: false,
        }
    }

    fn inject_target(state: &mut AppState) {
        state.injected_window_info.insert(
            10,
            leopardwm_platform_win32::WindowInfo {
                hwnd: 10,
                title: "target".into(),
                class_name: "RuleTarget".into(),
                process_id: 10,
                rect: Rect::new(0, 0, 700, 500),
                visible: true,
            },
        );
    }

    #[test]
    fn rule_float_then_tile_uses_origin_and_preserves_other_focus() {
        let mut state = AppState::new_with_config(Config::default(), vec![monitor()]);
        {
            let workspace = &mut state.workspaces.get_mut(&1).unwrap()[0];
            workspace.insert_window(10, Some(777)).unwrap();
            workspace.insert_window_in_column(11, 0).unwrap();
            workspace.focus_window(11).unwrap();
        }
        inject_target(&mut state);
        state.config.window_rules = vec![rule(config::WindowAction::Float)];
        state.compiled_rules = state.config.compile_window_rules();
        state.reapply_window_rules();
        assert!(state.workspaces[&1][0].is_floating(10));

        state.config.window_rules = vec![rule(config::WindowAction::Tile)];
        state.compiled_rules = state.config.compile_window_rules();
        state.reapply_window_rules();
        let workspace = &state.workspaces[&1][0];
        let (column, _) = workspace.find_window_location(10).unwrap();
        assert_eq!(workspace.columns()[column].width(), 777);
        assert_eq!(workspace.focused_window(), Some(11));
    }

    #[test]
    fn failed_ignore_release_retains_model_ownership() {
        let mut state = AppState::new_with_config(Config::default(), vec![monitor()]);
        state.workspaces.get_mut(&1).unwrap()[0]
            .insert_window(10, Some(700))
            .unwrap();
        inject_target(&mut state);
        state.config.window_rules = vec![rule(config::WindowAction::Ignore)];
        state.compiled_rules = state.config.compile_window_rules();
        state.injected_scratchpad_park_failure = true;

        state.reapply_window_rules();
        assert!(state.workspaces[&1][0].contains_window(10));
    }
}
