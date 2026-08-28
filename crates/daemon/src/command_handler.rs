//! IPC command handling for AppState.

use crate::config::Config;
use crate::state::{validate_set_width_fraction, AppState, FLOATING_TOTAL_MARGIN};
use leopardwm_core_layout::{Rect, Workspace};
use leopardwm_ipc::{IpcCommand, IpcResponse};
use leopardwm_platform_win32::{
    enumerate_windows, get_process_executable, monitor_above, monitor_below, monitor_to_left,
    monitor_to_right, move_window_offscreen, MonitorId, MonitorInfo,
};
use std::collections::HashMap;
use tracing::{debug, info};

/// How a command interacts with a workspace that is currently fullscreen.
enum FullscreenPolicy {
    /// Drop fullscreen first, then run the command against the visible strip.
    /// For structural commands that would otherwise mutate the hidden layout
    /// invisibly and dispatch focus to cloaked windows.
    Exit,
    /// Move focus but stay fullscreen, carrying fullscreen to the newly focused
    /// window (monocle mode), so focus changes cycle which window is fullscreen
    /// instead of dropping out of fullscreen.
    FollowFocus,
    /// Ignore while fullscreen — pure sizing/scroll commands have no visible
    /// effect on a single fullscreen window, and a stray scroll gesture must
    /// not silently drop fullscreen.
    Suppress,
    /// Run unchanged (fullscreen toggle, queries, cross-monitor/workspace moves).
    Allow,
}

/// Whether `cmd` navigates focus to a (possibly different) window, so the
/// cursor should follow it when `mouse_follows_focus` is on. Covers in-column
/// and cross-column focus, cross-monitor focus, and cross-monitor window moves
/// (focus travels with the window). The explicit workspace-switch commands are
/// excluded here; the caller only warps when the focused window actually
/// changed, so an edge-wrap focus command that crosses into another workspace
/// still warps onto the window it lands on.
fn is_focus_navigation(cmd: &IpcCommand) -> bool {
    use IpcCommand::*;
    matches!(
        cmd,
        FocusLeft
            | FocusRight
            | FocusUp
            | FocusDown
            | FocusNext
            | FocusPrev
            | FocusStart
            | FocusEnd
            | FocusMonitorLeft
            | FocusMonitorRight
            | FocusMonitorUp
            | FocusMonitorDown
            | MoveWindowToMonitorLeft
            | MoveWindowToMonitorRight
            | MoveWindowToMonitorUp
            | MoveWindowToMonitorDown
    )
}

/// Classify how `cmd` should behave when the focused workspace is fullscreen.
fn fullscreen_policy(cmd: &IpcCommand) -> FullscreenPolicy {
    use IpcCommand::*;
    match cmd {
        FocusLeft | FocusRight | FocusUp | FocusDown | FocusNext | FocusPrev | FocusStart
        | FocusEnd => FullscreenPolicy::FollowFocus,
        MoveColumnLeft | MoveColumnRight | MoveColumnToStart | MoveColumnToEnd | MoveWindowLeft
        | MoveWindowRight | MoveWindowUp | MoveWindowDown | ExpelToLeft | ExpelToRight
        | ConsumeFromLeft | ConsumeFromRight | ToggleTabbed => FullscreenPolicy::Exit,
        Resize { .. }
        | Scroll { .. }
        | SetColumnWidth { .. }
        | CycleWidthUp
        | CycleWidthDown
        | CycleHeightUp
        | CycleHeightDown
        | CenterColumn
        | MaximizeColumn
        | EqualizeColumnWidths
        | EqualizeColumnHeights => FullscreenPolicy::Suppress,
        _ => FullscreenPolicy::Allow,
    }
}

impl AppState {
    /// Snapshot a workspace's current animated placements as `(window_id, rect)` pairs.
    fn workspace_placements(
        &self,
        monitor: leopardwm_platform_win32::MonitorId,
        ws_idx: usize,
    ) -> Vec<(u64, Rect)> {
        let viewport = self.layout_viewport(monitor);
        self.workspaces
            .get(&monitor)
            .and_then(|v| v.get(ws_idx))
            .filter(|_| self.monitors.contains_key(&monitor))
            .map(|ws| {
                ws.compute_placements_animated(viewport)
                    .into_iter()
                    .map(|p| (p.window_id, p.rect))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Execute a command on the focused workspace, handling snapshot/transition
    /// and layout application boilerplate.
    ///
    /// - `animated`: if true, snapshots before and starts a layout transition after
    /// - `sync_focus`: if true, syncs the OS foreground window after layout apply
    /// - `f`: receives the focused workspace and viewport width
    fn execute_workspace_command(
        &mut self,
        animated: bool,
        sync_focus: bool,
        f: impl FnOnce(&mut Workspace, i32),
    ) -> IpcResponse {
        let viewport_width = self.focused_viewport().width;
        let snapshot = if animated {
            Some(self.snapshot_layout())
        } else {
            None
        };
        if let Some(workspace) = self.focused_workspace_mut() {
            f(workspace, viewport_width);
        }
        let focused_live_animation_is_unsafe =
            sync_focus && self.settle_focused_compositor_scroll();
        if !focused_live_animation_is_unsafe {
            if let Some(snapshot) = snapshot {
                self.start_layout_transition(snapshot);
            }
        }
        if let Err(e) = self.apply_layout() {
            return IpcResponse::error(format!("Failed to apply layout: {}", e));
        }
        // The command may have scrolled a column off-viewport (or back in);
        // update taskbar buttons to match the new visibility.
        self.sync_taskbar_buttons();
        if sync_focus {
            self.sync_foreground_window();
        }
        IpcResponse::Ok
    }

    /// Stop an active focused-workspace scroll when its target renderer cannot
    /// reliably follow per-frame live-HWND movement. This is shared by command,
    /// preview and physical `EVENT_SYSTEM_FOREGROUND` focus paths so input origin
    /// cannot select a different placement algorithm.
    pub(crate) fn settle_focused_compositor_scroll(&mut self) -> bool {
        let should_stop = self
            .focused_workspace()
            .is_some_and(|workspace| workspace.is_animating())
            && self.focused_window_uses_sticky_compositor();
        if should_stop {
            if let Some(workspace) = self.focused_workspace_mut() {
                workspace.stop_animation();
            }
        }
        should_stop
    }

    /// Whether the focused tiled window uses a compositor that cannot reliably
    /// follow per-frame asynchronous SetWindowPos. Called only for commands that
    /// already started a scroll animation; unknown classes keep the normal path.
    pub(crate) fn focused_window_uses_sticky_compositor(&self) -> bool {
        self.focused_workspace()
            .and_then(|workspace| workspace.focused_visible_window())
            .is_some_and(|hwnd| {
                let class = self
                    .lookup_window_info(hwnd)
                    .map(|info| info.class_name)
                    .unwrap_or_default();
                leopardwm_platform_win32::thumbnail::is_ghost_animation_class_str(&class)
            })
    }

    /// Exit fullscreen on the focused workspace if active, restoring the tiled
    /// strip (un-cloaking the other windows) and the focus border. Returns
    /// whether fullscreen was cleared. Called before focus/structural commands
    /// so they apply to the visible layout instead of the hidden one.
    fn exit_fullscreen_if_active(&mut self) -> bool {
        let Some(fs_wid) = self
            .focused_workspace()
            .and_then(|ws| ws.fullscreen_window_id())
        else {
            return false;
        };
        // Clear unconditionally — `toggle_fullscreen` would re-enter on the
        // focused window when the fullscreen pointer is stale.
        if let Some(ws) = self.focused_workspace_mut() {
            ws.clear_fullscreen_if_window(fs_wid);
        }
        let _ = self.apply_layout();
        self.sync_foreground_window();
        true
    }

    /// Handle a focus command while fullscreen: move focus and carry fullscreen
    /// to the newly focused window (monocle mode) rather than dropping it. The
    /// focused window becomes the fullscreen one, so focus cycles through
    /// windows fullscreen.
    fn focus_in_fullscreen(&mut self, cmd: &IpcCommand) -> IpcResponse {
        let resp = self.execute_workspace_command(false, true, |ws, vw| {
            match cmd {
                IpcCommand::FocusLeft => ws.focus_left(),
                IpcCommand::FocusRight => ws.focus_right(),
                IpcCommand::FocusUp => ws.focus_up(),
                IpcCommand::FocusDown => ws.focus_down(),
                IpcCommand::FocusNext => ws.focus_next(),
                IpcCommand::FocusPrev => ws.focus_prev(),
                IpcCommand::FocusStart => ws.focus_start(),
                IpcCommand::FocusEnd => ws.focus_end(),
                _ => {}
            }
            ws.fullscreen_follow_focus();
            // Keep scroll tracking the focused column even though the fullscreen
            // window hides it, so toggling fullscreen off later lands on the
            // focused window instead of a stale off-screen scroll position.
            ws.ensure_focused_visible_animated(vw);
        });
        // Fullscreen hides the focus border; keep it hidden after retargeting.
        self.hide_border();
        resp
    }

    /// Focus an explicit column (and window within it) on `monitor`, exactly as a
    /// focus command would: the column scrolls into view with the same animation,
    /// transition, taskbar and foreground handling.
    ///
    /// Used by monitor-edge preview clicks, whose thumbnail cannot receive the
    /// click itself. Deliberately does not touch `active_workspace`: a preview
    /// only ever belongs to the workspace already on screen, and switching one
    /// here would skip the parking and cloaking a real workspace switch performs,
    /// leaving both workspaces' windows visible at once.
    pub(crate) fn focus_column_in_active_workspace(
        &mut self,
        monitor: MonitorId,
        column_idx: usize,
        window_in_column: usize,
    ) -> IpcResponse {
        if !self.monitors.contains_key(&monitor) {
            return IpcResponse::error(format!("Monitor {monitor} is no longer connected"));
        }
        let active_idx = self.active_workspace_idx(monitor);
        let Some(workspace) = self
            .workspaces
            .get(&monitor)
            .and_then(|workspaces| workspaces.get(active_idx))
        else {
            return IpcResponse::error(format!("Monitor {monitor} has no active workspace"));
        };
        if workspace.is_fullscreen() {
            // A fullscreen workspace cloaks the rest of the strip, so it cannot
            // own an edge preview: such a click is stale. Refusing keeps column
            // focus and the fullscreen target from disagreeing, which is what
            // the focus commands' fullscreen policy exists to prevent.
            return IpcResponse::error(format!(
                "Monitor {monitor}'s active workspace is fullscreen"
            ));
        }
        let addressable = workspace
            .column(column_idx)
            .is_some_and(|column| window_in_column < column.windows().len());
        if !addressable {
            return IpcResponse::error(format!(
                "Column {column_idx} window {window_in_column} is not in monitor {monitor}'s active workspace"
            ));
        }

        let previous_focused_monitor = self.focused_monitor;
        let previous_floating_focus = self.previous_focused_hwnd;
        let previous_workspace = self
            .workspaces
            .get(&monitor)
            .and_then(|workspaces| workspaces.get(active_idx))
            .cloned()
            .expect("active preview workspace was validated above");

        self.focused_monitor = monitor;
        // Pointer-driven focus is explicit intent, so drop any floating-window
        // preference first. `sync_foreground_window` otherwise keeps foregrounding
        // a floating window that still holds `previous_focused_hwnd` (a dialog, a
        // summoned scratchpad, a sticky pin), and the strip would scroll while
        // keystrokes and the border stayed on the float — the same reason
        // focus-follows-mouse clears it before syncing.
        self.previous_focused_hwnd = None;
        let mut focus_error = None;
        let response =
            self.execute_workspace_command(
                false,
                true,
                |workspace, viewport_width| match workspace.set_focus(column_idx, window_in_column)
                {
                    Ok(()) => workspace.ensure_focused_visible_animated(viewport_width),
                    Err(error) => focus_error = Some(error),
                },
            );
        if let Some(error) = focus_error {
            if let Some(workspaces) = self.workspaces.get_mut(&monitor) {
                workspaces[active_idx] = previous_workspace;
            }
            self.focused_monitor = previous_focused_monitor;
            self.previous_focused_hwnd = previous_floating_focus;
            return IpcResponse::error(format!("Failed to focus column: {error}"));
        }
        if let IpcResponse::Error { message } = &response {
            // The focus/scroll mutation happened before the physical apply. Roll
            // it back so an unrelated later event cannot suddenly apply a click
            // that appeared to fail now.
            if let Some(workspaces) = self.workspaces.get_mut(&monitor) {
                workspaces[active_idx] = previous_workspace;
            }
            self.focused_monitor = previous_focused_monitor;
            self.previous_focused_hwnd = previous_floating_focus;
            self.settle_scroll_animations();
            self.last_placed_layout_rects.clear();
            if self.paused {
                return IpcResponse::error(format!(
                    "{message}; logical focus rolled back but physical rollback is pending paused recovery"
                ));
            }
            if let Err(rollback_error) = self.apply_layout() {
                return IpcResponse::error(format!(
                    "{message}; focus rollback also failed: {rollback_error}"
                ));
            }
            return response;
        }

        // Focus and z-order are independent in Windows. A floating window sits
        // above the tiled band by design, so without this the clicked column
        // takes keyboard focus while the float that was in front stays in front
        // and looks focused. Raising is reversible: clicking the float, or
        // summoning the scratchpad again, brings it back to the top.
        #[cfg(not(test))]
        if let Some(hwnd) = self
            .focused_workspace()
            .and_then(|workspace| workspace.focused_visible_window())
        {
            if let Err(error) = leopardwm_platform_win32::raise_window(hwnd) {
                debug!("Could not raise clicked window {hwnd:#x}: {error}");
            }
            if leopardwm_platform_win32::get_foreground_window() == Some(hwnd) {
                if let Err(error) =
                    leopardwm_platform_win32::thumbnail::reanchor_persistent_previews()
                {
                    debug!("Could not re-anchor preview layer after focus: {error}");
                }
            }
        }

        response
    }

    /// Focus or move the focused column to the start/end of the strip.
    fn handle_strip_end_command(&mut self, cmd: IpcCommand) -> IpcResponse {
        match cmd {
            IpcCommand::FocusStart => self.execute_workspace_command(false, true, |ws, vw| {
                ws.focus_start();
                ws.ensure_focused_visible_animated(vw);
                info!("Focus start -> column {}", ws.focused_column_index());
            }),
            IpcCommand::FocusEnd => self.execute_workspace_command(false, true, |ws, vw| {
                ws.focus_end();
                ws.ensure_focused_visible_animated(vw);
                info!("Focus end -> column {}", ws.focused_column_index());
            }),
            IpcCommand::MoveColumnToStart => {
                self.execute_workspace_command(true, false, |ws, vw| {
                    ws.move_column_to_start();
                    ws.ensure_focused_visible_animated(vw);
                    info!("Moved column to start");
                })
            }
            IpcCommand::MoveColumnToEnd => self.execute_workspace_command(true, false, |ws, vw| {
                ws.move_column_to_end();
                ws.ensure_focused_visible_animated(vw);
                info!("Moved column to end");
            }),
            _ => unreachable!("handle_strip_end_command called with a non-strip-end command"),
        }
    }

    /// Apply the fullscreen interaction policy for `cmd`. Returns `Some` if the
    /// command was fully handled here (suppressed, or focus moved within
    /// fullscreen), or `None` to continue normal dispatch (possibly after
    /// exiting fullscreen). A fullscreen window cloaks the rest of the strip, so
    /// structural commands must drop fullscreen first to act on the visible
    /// layout, focus commands carry fullscreen along, and sizing/scroll commands
    /// are ignored.
    fn apply_fullscreen_policy(&mut self, cmd: &IpcCommand) -> Option<IpcResponse> {
        if !self
            .focused_workspace()
            .is_some_and(|ws| ws.is_fullscreen())
        {
            return None;
        }
        match fullscreen_policy(cmd) {
            FullscreenPolicy::Exit => {
                self.exit_fullscreen_if_active();
                None
            }
            FullscreenPolicy::FollowFocus => {
                if self.config.behavior.fullscreen_follows_focus {
                    Some(self.focus_in_fullscreen(cmd))
                } else {
                    // Monocle-follow disabled: navigating away drops fullscreen
                    // and applies the focus command to the visible tiled layout,
                    // so fullscreen only ever affects the one window.
                    self.exit_fullscreen_if_active();
                    None
                }
            }
            FullscreenPolicy::Suppress => {
                debug!("Ignoring {:?} while fullscreen", cmd);
                Some(IpcResponse::Ok)
            }
            FullscreenPolicy::Allow => None,
        }
    }

    pub(crate) fn handle_command(&mut self, cmd: IpcCommand) -> IpcResponse {
        if let Some(resp) = self.apply_fullscreen_policy(&cmd) {
            return resp;
        }
        // "Mouse follows focus" (#43): warp the cursor onto the focused window
        // after a focus-navigation command, but only if focus actually moved to
        // a different window. Snapshot the focused HWND before `cmd` is moved
        // into the match; `sync_foreground_window` refreshes it during dispatch.
        let focus_before = (self.config.behavior.mouse_follows_focus && is_focus_navigation(&cmd))
            .then_some(self.previous_focused_hwnd);
        let resp = match cmd {
            IpcCommand::FocusLeft => self.execute_workspace_command(false, true, |ws, vw| {
                ws.focus_left();
                ws.ensure_focused_visible_animated(vw);
                info!("Focus left -> column {}", ws.focused_column_index());
            }),
            IpcCommand::FocusRight => self.execute_workspace_command(false, true, |ws, vw| {
                ws.focus_right();
                ws.ensure_focused_visible_animated(vw);
                info!("Focus right -> column {}", ws.focused_column_index());
            }),
            IpcCommand::FocusUp => self.focus_vertical(true),
            IpcCommand::FocusDown => self.focus_vertical(false),
            IpcCommand::FocusNext => self.execute_workspace_command(false, true, |ws, vw| {
                ws.focus_next();
                ws.ensure_focused_visible_animated(vw);
                info!(
                    "Focus next -> column {} window {}",
                    ws.focused_column_index(),
                    ws.focused_window_index_in_column()
                );
            }),
            IpcCommand::FocusPrev => self.execute_workspace_command(false, true, |ws, vw| {
                ws.focus_prev();
                ws.ensure_focused_visible_animated(vw);
                info!(
                    "Focus prev -> column {} window {}",
                    ws.focused_column_index(),
                    ws.focused_window_index_in_column()
                );
            }),
            IpcCommand::FocusStart
            | IpcCommand::FocusEnd
            | IpcCommand::MoveColumnToStart
            | IpcCommand::MoveColumnToEnd => self.handle_strip_end_command(cmd),
            IpcCommand::MoveColumnLeft => self.execute_workspace_command(true, false, |ws, vw| {
                ws.move_column_left();
                ws.ensure_focused_visible_animated(vw);
                info!("Moved column left");
            }),
            IpcCommand::MoveColumnRight => self.execute_workspace_command(true, false, |ws, vw| {
                ws.move_column_right();
                ws.ensure_focused_visible_animated(vw);
                info!("Moved column right");
            }),
            IpcCommand::MoveWindowLeft => self.execute_workspace_command(true, true, |ws, vw| {
                ws.move_window_left();
                ws.ensure_focused_visible_animated(vw);
                info!("Moved window left");
            }),
            IpcCommand::MoveWindowRight => self.execute_workspace_command(true, true, |ws, vw| {
                ws.move_window_right();
                ws.ensure_focused_visible_animated(vw);
                info!("Moved window right");
            }),
            IpcCommand::ExpelToLeft => self.execute_workspace_command(true, true, |ws, vw| {
                ws.expel_to_left();
                ws.ensure_focused_visible_animated(vw);
                info!("Expelled window to left");
            }),
            IpcCommand::ExpelToRight => self.execute_workspace_command(true, true, |ws, vw| {
                ws.expel_to_right();
                ws.ensure_focused_visible_animated(vw);
                info!("Expelled window to right");
            }),
            IpcCommand::ConsumeFromLeft => self.execute_workspace_command(true, true, |ws, vw| {
                ws.consume_from_left();
                ws.ensure_focused_visible_animated(vw);
                info!("Consumed window from left");
            }),
            IpcCommand::ConsumeFromRight => self.execute_workspace_command(true, true, |ws, vw| {
                ws.consume_from_right();
                ws.ensure_focused_visible_animated(vw);
                info!("Consumed window from right");
            }),
            IpcCommand::MoveWindowUp => self.move_window_vertical(true),
            IpcCommand::MoveWindowDown => self.move_window_vertical(false),
            IpcCommand::FocusMonitorLeft => self.focus_monitor(monitor_to_left, "left"),
            IpcCommand::FocusMonitorRight => self.focus_monitor(monitor_to_right, "right"),
            IpcCommand::FocusMonitorUp => self.focus_monitor(monitor_above, "up"),
            IpcCommand::FocusMonitorDown => self.focus_monitor(monitor_below, "down"),
            IpcCommand::MoveWindowToMonitorLeft => self.move_to_monitor(monitor_to_left, "left"),
            IpcCommand::MoveWindowToMonitorRight => self.move_to_monitor(monitor_to_right, "right"),
            IpcCommand::MoveWindowToMonitorUp => self.move_to_monitor(monitor_above, "up"),
            IpcCommand::MoveWindowToMonitorDown => self.move_to_monitor(monitor_below, "down"),
            IpcCommand::Resize { delta } => {
                self.execute_workspace_command(true, false, |ws, vw| {
                    ws.resize_focused_column(delta);
                    ws.ensure_focused_visible_animated(vw);
                    info!("Resized column by {}", delta);
                })
            }
            IpcCommand::Scroll { delta } => {
                self.execute_workspace_command(false, false, |ws, vw| {
                    ws.scroll_by(delta, vw);
                    info!("Scrolled by {}", delta);
                })
            }
            IpcCommand::QueryWorkspace => self.handle_query_workspace(),
            IpcCommand::QueryFocused => self.handle_query_focused(),
            IpcCommand::Refresh => self.handle_refresh(),
            IpcCommand::Apply => {
                if let Err(e) = self.apply_layout() {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                info!("Applied layout");
                IpcResponse::Ok
            }
            IpcCommand::Reload => self.handle_reload(),
            IpcCommand::TogglePause => {
                if let Err(e) = self.toggle_pause("IPC toggle") {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                IpcResponse::Ok
            }
            IpcCommand::SetGhostAnimation { enabled } => self.handle_set_ghost_animation(enabled),
            IpcCommand::Stop => {
                // This is handled specially in the event loop
                IpcResponse::Ok
            }
            IpcCommand::PanicRevert => {
                // This is handled specially in the event loop
                IpcResponse::Ok
            }
            IpcCommand::QueryAllWindows => self.handle_query_all_windows(),
            IpcCommand::CloseWindow => self.handle_close_window(),
            IpcCommand::ToggleFloating => self.handle_toggle_floating(),
            IpcCommand::ScratchpadStash => {
                self.scratchpad_stash();
                IpcResponse::Ok
            }
            IpcCommand::ScratchpadToggle => {
                self.scratchpad_toggle();
                IpcResponse::Ok
            }
            IpcCommand::ToggleSticky => {
                self.toggle_sticky();
                IpcResponse::Ok
            }
            IpcCommand::ToggleNewWindowPlacement => self.handle_toggle_new_window_placement(),
            IpcCommand::ToggleFullscreen => self.handle_toggle_fullscreen(),
            IpcCommand::SetColumnWidth { fraction } => {
                if let Err(message) = validate_set_width_fraction(fraction) {
                    return IpcResponse::error(message);
                }
                self.execute_workspace_command(true, false, |ws, vw| {
                    ws.set_focused_column_width_fraction(fraction, vw);
                    ws.ensure_focused_visible_animated(vw);
                    info!("Set column width fraction to {:.3}", fraction);
                })
            }
            IpcCommand::CenterColumn => self.execute_workspace_command(false, false, |ws, vw| {
                ws.center_focused_column_animated(vw);
                info!("Centered focused column");
            }),
            IpcCommand::MaximizeColumn => self.execute_workspace_command(true, false, |ws, vw| {
                let entering = ws.toggle_maximize_column(vw);
                ws.center_focused_column_animated(vw);
                info!("Maximize column: {}", if entering { "on" } else { "off" });
            }),
            IpcCommand::EqualizeColumnWidths => {
                self.execute_workspace_command(true, false, |ws, vw| {
                    ws.equalize_column_widths(vw);
                    ws.ensure_focused_visible_animated(vw);
                    info!("Equalized column widths");
                })
            }
            IpcCommand::CycleWidthUp => {
                let presets = self.config.layout.width_presets.clone();
                self.execute_workspace_command(true, false, |ws, vw| {
                    ws.cycle_width_up(&presets, vw);
                    ws.ensure_focused_visible_animated(vw);
                    info!("Cycled column width up");
                })
            }
            IpcCommand::CycleWidthDown => {
                let presets = self.config.layout.width_presets.clone();
                self.execute_workspace_command(true, false, |ws, vw| {
                    ws.cycle_width_down(&presets, vw);
                    ws.ensure_focused_visible_animated(vw);
                    info!("Cycled column width down");
                })
            }
            IpcCommand::CycleHeightUp => {
                let presets = self.config.layout.height_presets.clone();
                self.execute_workspace_command(true, false, |ws, _vw| {
                    ws.cycle_height_up(&presets);
                    info!("Cycled window height up");
                })
            }
            IpcCommand::CycleHeightDown => {
                let presets = self.config.layout.height_presets.clone();
                self.execute_workspace_command(true, false, |ws, _vw| {
                    ws.cycle_height_down(&presets);
                    info!("Cycled window height down");
                })
            }
            IpcCommand::EqualizeColumnHeights => {
                self.execute_workspace_command(true, false, |ws, _vw| {
                    ws.equalize_focused_column_heights();
                    info!("Equalized column heights");
                })
            }
            IpcCommand::QueryStatus => self.handle_query_status(),
            IpcCommand::WorkspacePrev | IpcCommand::WorkspaceNext => {
                self.handle_workspace_prev_next(cmd)
            }
            IpcCommand::MoveToWorkspacePrev => self.handle_move_to_workspace_relative(false),
            IpcCommand::MoveToWorkspaceNext => self.handle_move_to_workspace_relative(true),
            IpcCommand::SwitchWorkspace { index } => self.handle_switch_workspace(index),
            IpcCommand::MoveToWorkspace { index } => self.handle_move_to_workspace(index),
            IpcCommand::HealthCheck => self.handle_health_check(),
            IpcCommand::GetAutoStart => {
                match leopardwm_platform_win32::autostart::get_autostart() {
                    Ok(enabled) => IpcResponse::AutoStartState { enabled },
                    Err(e) => IpcResponse::error(format!("Failed to read auto-start state: {}", e)),
                }
            }
            IpcCommand::SetAutoStart { enabled } => self.handle_set_auto_start(enabled),
            IpcCommand::Subscribe { .. } => {
                // Subscribe is handled out-of-band by ipc_server.rs
                // (per-client task acquires AppState directly so subscribe
                // + snapshot are atomic). Reaching this arm means the IPC
                // server accidentally routed a Subscribe through the
                // command path — it's a bug, not a user error.
                IpcResponse::error(
                    "Subscribe must be handled in stream mode by the IPC server, not the main \
                     command loop — this is an internal routing bug.",
                )
            }
            IpcCommand::ToggleOverview => {
                self.toggle_overview();
                IpcResponse::Ok
            }
            IpcCommand::ToggleTabbed => self.execute_workspace_command(true, false, |ws, _vw| {
                ws.toggle_focused_column_tabbed_mode();
                info!("Toggled tabbed mode on focused column");
            }),
            IpcCommand::SetActiveTab { column, tab } => self.handle_set_active_tab(column, tab),
        };
        if let Some(before) = focus_before {
            if let Some(hwnd) = self.previous_focused_hwnd {
                if Some(hwnd) != before {
                    leopardwm_platform_win32::warp_cursor_to_window(hwnd);
                }
            }
        }
        resp
    }

    /// Focus the monitor picked by `select` (left/right/above/below), if any.
    /// `dir` labels the direction for logging.
    fn focus_monitor(
        &mut self,
        select: fn(&[MonitorInfo], MonitorId) -> Option<&MonitorInfo>,
        dir: &str,
    ) -> IpcResponse {
        let monitors: Vec<_> = self.monitors.values().cloned().collect();
        if let Some(target) = select(&monitors, self.focused_monitor) {
            let target_id = target.id;
            self.focused_monitor = target_id;
            info!("Focused monitor {} -> {}", dir, target_id);
            if let Err(e) = self.apply_layout() {
                return IpcResponse::error(format!("Failed to apply layout: {}", e));
            }
            self.sync_foreground_window();
        } else {
            info!("No monitor {}", dir);
        }
        IpcResponse::Ok
    }

    /// Handle `IpcCommand::QueryFocused`.
    fn handle_query_focused(&mut self) -> IpcResponse {
        if let Some(workspace) = self.focused_workspace() {
            IpcResponse::FocusedWindow {
                window_id: workspace.focused_window(),
                column_index: workspace.focused_column_index(),
                window_index: workspace.focused_window_index_in_column(),
            }
        } else {
            IpcResponse::error("No focused workspace")
        }
    }

    /// Handle `IpcCommand::Refresh`.
    fn handle_refresh(&mut self) -> IpcResponse {
        match self.enumerate_and_add_windows() {
            Ok(added) => {
                self.prune_stale_windows();
                info!("Refreshed: added {} new windows across all monitors", added);
                // An explicit refresh is the user's recovery path when windows
                // have drifted away from the layout (OS-driven repositioning
                // after a topology change, an app that fought placement). Drop
                // the desired-rect cache and its feedback suppression so
                // placement actually runs instead of short-circuiting on an
                // unchanged desired layout, and re-park anything the apply below
                // will not place.
                self.last_placed_layout_rects.clear();
                self.moved_or_resized_suppression.clear();
                self.repark_windows_outside_active_layout();
                if let Err(e) = self.apply_layout() {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                IpcResponse::Ok
            }
            Err(e) => IpcResponse::error(format!("Failed to enumerate windows: {}", e)),
        }
    }

    /// Handle `IpcCommand::Reload`.
    fn handle_reload(&mut self) -> IpcResponse {
        match Config::load() {
            Ok(new_config) => {
                self.apply_config(new_config);
                if let Err(e) = self.apply_layout() {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                self.broadcast_event(leopardwm_ipc::IpcEvent::ConfigReloaded);
                IpcResponse::Ok
            }
            Err(e) => IpcResponse::error(format!("Failed to reload config: {}", e)),
        }
    }

    /// Handle `IpcCommand::SetGhostAnimation`.
    fn handle_set_ghost_animation(&mut self, enabled: Option<bool>) -> IpcResponse {
        if let Some(new_value) = enabled {
            // Aborts any active ghost transition first — the flag
            // flip mid-flight would otherwise leak handles.
            self.abort_active_ghost_transition();
            self.config.behavior.swap_chain_ghost_animation = new_value;
        }
        IpcResponse::BoolValue {
            value: self.config.behavior.swap_chain_ghost_animation,
        }
    }

    /// Handle `IpcCommand::CloseWindow`.
    fn handle_close_window(&mut self) -> IpcResponse {
        if let Some(hwnd) = self.focused_workspace().and_then(|ws| ws.focused_window()) {
            if let Err(e) = leopardwm_platform_win32::close_window(hwnd) {
                return IpcResponse::error(format!("Failed to close window: {}", e));
            }
            info!("Closed window {}", hwnd);
        } else {
            info!("No focused window to close");
        }
        IpcResponse::Ok
    }

    /// Handle `IpcCommand::ToggleNewWindowPlacement`.
    fn handle_toggle_new_window_placement(&mut self) -> IpcResponse {
        use crate::config::NewWindowPlacement;
        let next = match self.config.behavior.new_window_placement {
            NewWindowPlacement::NewColumn => NewWindowPlacement::InColumn,
            NewWindowPlacement::InColumn => NewWindowPlacement::NewColumn,
        };
        self.config.behavior.new_window_placement = next;
        let _ = self.config.save();
        info!("New-window placement set to {:?}", next);
        IpcResponse::Ok
    }

    /// Handle `IpcCommand::ToggleFullscreen`.
    fn handle_toggle_fullscreen(&mut self) -> IpcResponse {
        let resp = self.execute_workspace_command(true, false, |ws, _vw| {
            let entering = ws.toggle_fullscreen();
            info!("Fullscreen: {}", if entering { "on" } else { "off" });
        });
        if self
            .focused_workspace()
            .is_some_and(|ws| ws.is_fullscreen())
        {
            self.hide_border();
        } else {
            self.sync_foreground_window();
        }
        resp
    }

    /// Handle `IpcCommand::QueryStatus`.
    fn handle_query_status(&mut self) -> IpcResponse {
        let uptime = self.start_time.elapsed().as_secs();
        let total_windows: usize = self
            .workspaces
            .values()
            .flat_map(|ws_vec| ws_vec.iter())
            .map(|ws| ws.window_count() + ws.floating_count())
            .sum();
        IpcResponse::StatusInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            monitors: self.monitors.len(),
            total_windows,
            uptime_seconds: uptime,
        }
    }

    /// Handle `IpcCommand::WorkspacePrev` and `IpcCommand::WorkspaceNext`.
    fn handle_workspace_prev_next(&mut self, cmd: IpcCommand) -> IpcResponse {
        const COUNT: usize = 9;
        let monitor = self.focused_monitor;
        let current = self.active_workspace_idx(monitor);
        let target = match cmd {
            IpcCommand::WorkspacePrev => (current + COUNT - 1) % COUNT,
            IpcCommand::WorkspaceNext => (current + 1) % COUNT,
            _ => unreachable!(),
        };
        self.handle_command(IpcCommand::SwitchWorkspace {
            index: (target + 1) as u8,
        })
    }

    /// Move the focused window to the previous (`forward = false`) or next
    /// (`forward = true`) workspace, wrapping 1 ↔ 9. Handles
    /// `IpcCommand::MoveToWorkspacePrev` / `MoveToWorkspaceNext`.
    fn handle_move_to_workspace_relative(&mut self, forward: bool) -> IpcResponse {
        const COUNT: usize = 9;
        let monitor = self.focused_monitor;
        let current = self.active_workspace_idx(monitor);
        let target = if forward {
            (current + 1) % COUNT
        } else {
            (current + COUNT - 1) % COUNT
        };
        self.handle_move_to_workspace((target + 1) as u8)
    }

    /// Vertical focus (`focus_up`/`focus_down`). With `workspace_edge_wrap` on,
    /// a press at the column's top/bottom edge switches to the adjacent
    /// workspace instead of no-oping. `up` selects up vs down.
    fn focus_vertical(&mut self, up: bool) -> IpcResponse {
        if self.config.behavior.workspace_edge_wrap && self.focus_at_vertical_edge(up) {
            return self.handle_workspace_prev_next(if up {
                IpcCommand::WorkspacePrev
            } else {
                IpcCommand::WorkspaceNext
            });
        }
        self.execute_workspace_command(false, true, |ws, _vw| {
            if up {
                ws.focus_up();
            } else {
                ws.focus_down();
            }
            info!(
                "Focus {} -> window {}",
                if up { "up" } else { "down" },
                ws.focused_window_index_in_column()
            );
        })
    }

    /// Vertical move (`move_window_up`/`move_window_down` within a column). With
    /// `workspace_edge_wrap` on, a press at the column's top/bottom edge moves
    /// the focused window to the adjacent workspace instead of no-oping.
    fn move_window_vertical(&mut self, up: bool) -> IpcResponse {
        if self.config.behavior.workspace_edge_wrap && self.focus_at_vertical_edge(up) {
            return self.handle_move_to_workspace_relative(!up);
        }
        self.execute_workspace_command(true, true, |ws, _vw| {
            if up {
                ws.move_window_up_in_column();
            } else {
                ws.move_window_down_in_column();
            }
            info!("Moved window {} in column", if up { "up" } else { "down" });
        })
    }

    /// Whether vertical focus/move is pinned at the top (`up = true`) or bottom
    /// (`up = false`) of the focused column, so it can't move within it.
    fn focus_at_vertical_edge(&self, up: bool) -> bool {
        self.focused_workspace().is_some_and(|ws| {
            if up {
                ws.at_column_top()
            } else {
                ws.at_column_bottom()
            }
        })
    }

    /// Move the focused window to the monitor picked by `select`
    /// (left/right/above/below), if any. `dir` labels the direction for logging.
    fn move_to_monitor(
        &mut self,
        select: fn(&[MonitorInfo], MonitorId) -> Option<&MonitorInfo>,
        dir: &str,
    ) -> IpcResponse {
        let monitors: Vec<_> = self.monitors.values().cloned().collect();
        if let Some(target) = select(&monitors, self.focused_monitor) {
            let target_id = target.id;
            match self.move_focused_window_to_monitor_transactional(target_id) {
                Ok(Some(hwnd)) => {
                    info!("Moved window {} to monitor {}", hwnd, target_id);
                    if let Err(e) = self.apply_layout() {
                        return IpcResponse::error(format!("Failed to apply layout: {}", e));
                    }
                    self.sync_foreground_window();
                }
                Ok(None) => info!("No focused window to move"),
                Err(message) => return IpcResponse::error(message),
            }
        } else {
            info!("No monitor {}", dir);
        }
        IpcResponse::Ok
    }

    /// Handle `IpcCommand::QueryWorkspace`.
    fn handle_query_workspace(&mut self) -> IpcResponse {
        let active_idx = self.active_workspace_idx(self.focused_monitor);
        let active_workspace_name = self.config.workspaces.name_for(active_idx);
        if let Some(workspace) = self.focused_workspace() {
            IpcResponse::WorkspaceState {
                columns: workspace.column_count(),
                windows: workspace.window_count(),
                focused_column: workspace.focused_column_index(),
                focused_window: workspace.focused_window_index_in_column(),
                scroll_offset: workspace.scroll_offset(),
                total_width: workspace.total_width(),
                active_workspace: active_idx as u8 + 1,
                active_workspace_name,
            }
        } else {
            IpcResponse::error("No focused workspace")
        }
    }

    /// Handle `IpcCommand::QueryAllWindows`.
    fn handle_query_all_windows(&mut self) -> IpcResponse {
        let mut windows = Vec::new();

        let focused_hwnd = self.focused_workspace().and_then(|ws| ws.focused_window());

        let win_info_map: HashMap<u64, (String, String, u32)> = match enumerate_windows() {
            Ok(wins) => wins
                .into_iter()
                .map(|w| (w.hwnd, (w.title, w.class_name, w.process_id)))
                .collect(),
            Err(_) => HashMap::new(),
        };

        for (monitor_id, ws_vec) in &self.workspaces {
            for workspace in ws_vec {
                // Tiled windows
                for (col_idx, column) in workspace.columns().iter().enumerate() {
                    for (win_idx, &window_id) in column.windows().iter().enumerate() {
                        let (title, class_name, process_id) = win_info_map
                            .get(&window_id)
                            .cloned()
                            .unwrap_or_else(|| ("Unknown".to_string(), "Unknown".to_string(), 0));

                        let executable = get_process_executable(process_id).unwrap_or_default();

                        // Get rect from computed placements
                        let rect = self
                            .monitors
                            .contains_key(monitor_id)
                            .then(|| {
                                workspace.compute_placements(self.layout_viewport(*monitor_id))
                            })
                            .and_then(|placements| {
                                placements
                                    .into_iter()
                                    .find(|p| p.window_id == window_id)
                                    .map(|p| p.rect)
                            })
                            .unwrap_or_else(|| Rect::new(0, 0, 0, 0));

                        windows.push(leopardwm_ipc::WindowInfo {
                            window_id,
                            title,
                            class_name,
                            process_id,
                            executable,
                            rect: leopardwm_ipc::IpcRect::new(
                                rect.x,
                                rect.y,
                                rect.width,
                                rect.height,
                            ),
                            column_index: Some(col_idx),
                            window_index: Some(win_idx),
                            monitor_id: *monitor_id as i64,
                            is_floating: false,
                            is_focused: Some(window_id) == focused_hwnd,
                        });
                    }
                }

                // Floating windows
                for floating in workspace.floating_windows() {
                    let (title, class_name, process_id) = win_info_map
                        .get(&floating.id)
                        .cloned()
                        .unwrap_or_else(|| ("Unknown".to_string(), "Unknown".to_string(), 0));

                    let executable = get_process_executable(process_id).unwrap_or_default();

                    windows.push(leopardwm_ipc::WindowInfo {
                        window_id: floating.id,
                        title,
                        class_name,
                        process_id,
                        executable,
                        rect: leopardwm_ipc::IpcRect::new(
                            floating.rect.x,
                            floating.rect.y,
                            floating.rect.width,
                            floating.rect.height,
                        ),
                        column_index: None,
                        window_index: None,
                        monitor_id: *monitor_id as i64,
                        is_floating: true,
                        is_focused: Some(floating.id) == focused_hwnd,
                    });
                }
            }
        }

        IpcResponse::WindowList { windows }
    }

    /// Handle `IpcCommand::ToggleFloating`.
    fn handle_toggle_floating(&mut self) -> IpcResponse {
        let floating_hwnd = self.previous_focused_hwnd.filter(|hwnd| {
            self.focused_workspace()
                .is_some_and(|workspace| workspace.is_floating(*hwnd))
        });

        if let Some(hwnd) = floating_hwnd {
            // The managed rect is the last user-confirmed/requested floating
            // geometry. DWM extended frame bounds can lag a rapid
            // SetWindowPos/toggle, so querying DWM here would re-learn a
            // stale, slightly smaller size on every cycle.
            if let Some(rect) = self.floating_rect_for_window(hwnd) {
                let _ = self.update_floating_geometry(hwnd, rect);
            }
            if self
                .focused_workspace_mut()
                .is_some_and(|workspace| workspace.unfloat_window(hwnd))
            {
                self.disable_snap_for_window(hwnd);
                info!("Unfloated window {} back to tiling", hwnd);
            }
        } else if let Some(wid) = self
            .focused_workspace()
            .and_then(|workspace| workspace.focused_window())
        {
            let logical_size = if self.config.layout.remember_floating_sizes {
                self.floating_size_history
                    .get(&wid)
                    .copied()
                    .unwrap_or_else(|| self.default_floating_size())
            } else {
                self.default_floating_size()
            };
            let rect = self.centered_rect_for_logical_floating_size(
                self.focused_monitor,
                logical_size,
                FLOATING_TOTAL_MARGIN,
            );
            if self
                .focused_workspace_mut()
                .and_then(|workspace| workspace.toggle_floating(rect))
                .is_some()
            {
                self.restore_snap_for_window(wid);
                info!("Toggled window {} to floating", wid);
            }
        }
        if let Err(e) = self.apply_layout() {
            return IpcResponse::error(format!("Failed to apply layout: {}", e));
        }
        self.sync_foreground_window();
        IpcResponse::Ok
    }

    /// Handle `IpcCommand::SwitchWorkspace`.
    fn handle_switch_workspace(&mut self, index: u8) -> IpcResponse {
        if !(1..=9).contains(&index) {
            return IpcResponse::error("Workspace index must be 1-9");
        }
        // A switch initiated outside the overlay (hotkey, CLI) dismisses
        // an open overview; overlay-initiated switches hid it already.
        if self.overview_open {
            self.hide_overview_animated(Some((index - 1) as usize));
        }
        let idx = (index - 1) as usize;
        let monitor = self.focused_monitor;
        let current_idx = self.active_workspace_idx(monitor);
        if idx == current_idx {
            return IpcResponse::Ok;
        }

        // Remember the floating window focused on the workspace we
        // are leaving, so returning re-focuses it. If the last focus
        // was tiled, forget any prior floating focus for it (the
        // column state already restores tiled focus). Prefer the live
        // OS foreground over the cached focus so a missed focus event
        // can't record the wrong window. Under cfg(test) there is no
        // meaningful OS foreground; tests drive previous_focused_hwnd.
        #[cfg(not(test))]
        let leaving_focus =
            leopardwm_platform_win32::get_foreground_window().or(self.previous_focused_hwnd);
        #[cfg(test)]
        let leaving_focus = self.previous_focused_hwnd;
        // A focused sticky (pinned) window keeps focus across the switch:
        // capture that BEFORE the workspace changes. Any stale pending
        // refocus from a previous (aborted) switch is dropped here.
        self.pending_sticky_refocus = None;
        let sticky_focus = leaving_focus.filter(|hwnd| self.sticky_windows.contains(hwnd));
        if let Some(hwnd) = leaving_focus {
            if self
                .focused_workspace()
                .is_some_and(|ws| ws.is_floating(hwnd))
            {
                self.floating_focus.insert((monitor, current_idx), hwnd);
            } else {
                self.floating_focus.remove(&(monitor, current_idx));
            }
        }

        // Cancel any in-progress drag before switching ownership. This
        // restores both a detached plain-drag HWND and a Shift live reorder.
        if let Some(drag_hwnd) = self.drag_state.as_ref().map(|drag| drag.hwnd) {
            let restore_window = leopardwm_platform_win32::is_valid_window(drag_hwnd);
            if let Some(aborted_hwnd) = self.abort_active_drag(restore_window) {
                leopardwm_platform_win32::set_dwm_transitions_disabled(aborted_hwnd, false);
            }
        }
        self.pending_drag_hint = Some(crate::state::DragHintAction::Hide);
        if let Err(error) = self.cancel_layout_transition_for_exact_landing() {
            return IpcResponse::error(format!(
                "Cannot switch workspace until the current transition exits are safe: {error}"
            ));
        }
        self.abort_active_ghost_transition();

        let slide_height = self
            .monitors
            .get(&monitor)
            .map(|m| m.work_area.height)
            .unwrap_or(crate::state::FALLBACK_WORK_AREA_HEIGHT);
        // Positive offset = new workspace enters from below (scrolling up).
        let y_offset = if idx > current_idx {
            slide_height
        } else {
            -slide_height
        };

        // Snapshot old workspace's current positions (start for exiting windows).
        let mut old_placements = self.workspace_placements(monitor, current_idx);

        // Overview snapshot mode: grab the outgoing windows NOW, while
        // they are still on screen, so their cards show a real frame
        // after they move offscreen below. Skipped otherwise (PrintWindow
        // per window is not free).
        if self.config.overview.render == crate::config::OverviewRender::Snapshot {
            for (wid, _) in &old_placements {
                let _ = leopardwm_platform_win32::snapshot::snapshot_capture(*wid);
            }
        }

        // Ensure target workspace exists (lazy creation)
        self.ensure_workspace_exists(monitor, idx);

        // Switch active workspace
        self.active_workspace.insert(monitor, idx);

        // Sticky windows follow the switch: move them onto the
        // now-active workspace.
        self.rehome_sticky_windows();

        // Compute new workspace's final placements.
        let mut new_placements = self.workspace_placements(monitor, idx);

        // Keep sticky windows out of the slide animation so they sit
        // still while the rest of the layout scrolls past.
        old_placements.retain(|(w, _)| !self.sticky_windows.contains(w));
        new_placements.retain(|(w, _)| !self.sticky_windows.contains(w));

        // Build animation rects:
        // - Entering windows: start offscreen, end at final position
        // - Exiting windows: start at current position, end offscreen
        let mut start_rects = std::collections::HashMap::new();
        let mut exit_rects = std::collections::HashMap::new();

        // New workspace windows enter from the opposite side.
        for (wid, rect) in &new_placements {
            start_rects.insert(
                *wid,
                leopardwm_core_layout::Rect::new(
                    rect.x,
                    rect.y + y_offset,
                    rect.width,
                    rect.height,
                ),
            );
        }

        // Old workspace windows slide out.
        for (wid, rect) in &old_placements {
            start_rects.insert(*wid, *rect);
            exit_rects.insert(
                *wid,
                leopardwm_core_layout::Rect::new(
                    rect.x,
                    rect.y - y_offset,
                    rect.width,
                    rect.height,
                ),
            );
        }

        // Animate only when there is something to move and motion isn't reduced.
        // Otherwise hide the leaving windows immediately: reduce_motion (e.g. on
        // battery) skips the transition, and the transition is the only path that
        // would otherwise move them off-screen, so without this they linger as
        // ghosts on top of the new workspace.
        let animating = !start_rects.is_empty() && !self.reduce_motion;
        if animating {
            let duration = self.config.animation.workspace_switch_duration_ms;
            self.start_workspace_switch_transition(start_rects, exit_rects, duration);
        } else {
            let failures: Vec<_> = old_placements
                .iter()
                .filter_map(|(wid, _)| move_window_offscreen(*wid).err().map(|error| (*wid, error)))
                .collect();
            if !failures.is_empty() {
                self.active_workspace.insert(monitor, current_idx);
                self.rehome_sticky_windows();
                let rollback = self.apply_layout().err();
                return IpcResponse::error(format!(
                    "Workspace switch rolled back because old windows could not be parked: {failures:?}; rollback={rollback:?}"
                ));
            }
        }

        if let Err(e) = self.apply_layout() {
            return IpcResponse::error(format!("Failed to apply layout: {}", e));
        }
        // Hide the now-inactive workspace's windows from the taskbar (cloak).
        // Mid-slide windows are skipped here and cloaked when the transition
        // settles (see tick_animations).
        self.sync_taskbar_buttons();
        // Restore the floating window that was focused on this
        // workspace (if it still floats here) so it regains focus on
        // return, before syncing the OS foreground.
        if let Some(&hwnd) = self.floating_focus.get(&(monitor, idx)) {
            let still_floating = self
                .workspaces
                .get(&monitor)
                .and_then(|v| v.get(idx))
                .is_some_and(|ws| ws.is_floating(hwnd));
            if still_floating {
                self.previous_focused_hwnd = Some(hwnd);
            }
        }
        self.sync_foreground_window();
        // If a summoned scratchpad lives on this workspace, restore
        // its focus (it would otherwise stay visible but lose focus
        // to a tiled window on the switch back).
        self.refocus_scratchpad_if_active();
        // The user was focused on a pinned window: it followed the switch
        // (re-homed above), so focus stays on it. Re-assert again at the
        // animation landing — a spurious foreground event from the
        // destination's windows mid-slide (e.g. a fullscreen window
        // activating) can clobber previous_focused_hwnd before the
        // landing re-sync.
        if let Some(hwnd) = sticky_focus {
            if self.refocus_sticky_window(hwnd) && self.layout_transition.is_some() {
                self.pending_sticky_refocus = Some(hwnd);
            }
        }
        self.broadcast_event(leopardwm_ipc::IpcEvent::WorkspaceChanged {
            monitor: monitor as i64,
            old_index: current_idx as u8,
            new_index: idx as u8,
            name: self.config.workspaces.name_for(idx),
        });
        info!("Switched to workspace {}", index);
        IpcResponse::Ok
    }

    /// Handle `IpcCommand::MoveToWorkspace`.
    fn handle_move_to_workspace(&mut self, index: u8) -> IpcResponse {
        if !(1..=9).contains(&index) {
            return IpcResponse::error("Workspace index must be 1-9");
        }
        let idx = (index - 1) as usize;
        let monitor = self.focused_monitor;
        let current_idx = self.active_workspace_idx(monitor);
        if idx == current_idx {
            return IpcResponse::Ok;
        }

        // Get focused window — prefer the OS-foreground window (previous_focused_hwnd)
        // so that floating windows can also be moved between workspaces.
        // Fall back to tiled focus if previous_focused_hwnd is not on this workspace.
        let focused_hwnd = {
            let tiled_focus = self.focused_workspace().and_then(|ws| ws.focused_window());
            let os_focus = self.previous_focused_hwnd.and_then(|hwnd| {
                // Verify the OS-focused window is actually on the current workspace
                self.workspaces
                    .get(&monitor)
                    .and_then(|v| v.get(current_idx))
                    .filter(|ws| ws.contains_window(hwnd))
                    .map(|_| hwnd)
            });
            match os_focus.or(tiled_focus) {
                Some(hwnd) => hwnd,
                None => return IpcResponse::Ok,
            }
        };

        let snapshot = self.snapshot_layout();

        // Ensure target workspace exists (lazy creation)
        self.ensure_workspace_exists(monitor, idx);

        // Check if the window is floating so we use the correct add/remove APIs.
        let is_floating = self
            .workspaces
            .get(&monitor)
            .and_then(|v| v.get(current_idx))
            .is_some_and(|ws| ws.is_floating(focused_hwnd));

        // Capture the tiled column width before removal so the window re-tiles
        // at its chosen width on the target workspace instead of the default.
        let tiled_width = if is_floating {
            None
        } else {
            self.tiled_column_width(monitor, current_idx, focused_hwnd)
        };

        // Capture the source column + sibling so a later move back can restore
        // the window to its original position.
        let source_origin = if is_floating {
            None
        } else {
            self.tiled_column_origin(monitor, current_idx, focused_hwnd)
        };

        // If this window is returning to the workspace it was moved out of,
        // resolve where it should land: rejoin a surviving sibling's column,
        // else a new column at the remembered (clamped) index. Otherwise the
        // normal insert (right of the focused column) applies.
        enum Landing {
            Default,
            Stack(usize),
            NewColumn(usize),
        }
        let landing = if is_floating {
            Landing::Default
        } else {
            match self
                .move_origins
                .get(&focused_hwnd)
                .copied()
                .filter(|o| o.monitor == monitor && o.ws_idx == idx)
            {
                Some(o) => {
                    let sibling_col = o.sibling.and_then(|s| {
                        self.workspaces
                            .get(&monitor)?
                            .get(idx)?
                            .find_window_location(s)
                            .map(|(c, _)| c)
                    });
                    match sibling_col {
                        Some(c) => Landing::Stack(c),
                        None => Landing::NewColumn(o.column),
                    }
                }
                None => Landing::Default,
            }
        };

        // Remove from source and insert into target.
        // For floating windows, get the rect from workspace state (canonical position).
        let floating_rect = if is_floating {
            self.workspaces
                .get(&monitor)
                .and_then(|v| v.get(current_idx))
                .and_then(|ws| {
                    ws.floating_windows()
                        .iter()
                        .find(|f| f.id == focused_hwnd)
                        .map(|f| f.rect)
                })
        } else {
            None
        };

        if let Some(workspace) = self
            .workspaces
            .get_mut(&monitor)
            .and_then(|v| v.get_mut(current_idx))
        {
            if is_floating {
                workspace.remove_floating(focused_hwnd);
            } else if let Err(e) = workspace.remove_window(focused_hwnd) {
                return IpcResponse::error(format!("Failed to remove window: {}", e));
            }
        }

        // Ensure target workspace exists (lazy creation)
        self.ensure_workspace_exists(monitor, idx);

        // Insert into target workspace
        if let Some(workspace) = self
            .workspaces
            .get_mut(&monitor)
            .and_then(|v| v.get_mut(idx))
        {
            if is_floating {
                let rect =
                    floating_rect.unwrap_or(leopardwm_core_layout::Rect::new(0, 0, 800, 600));
                if let Err(e) = workspace.add_floating(focused_hwnd, rect) {
                    // Rollback: re-add to source
                    if let Some(src_ws) = self
                        .workspaces
                        .get_mut(&monitor)
                        .and_then(|v| v.get_mut(current_idx))
                    {
                        let _ = src_ws.add_floating(focused_hwnd, rect);
                    }
                    return IpcResponse::error(format!("Failed to move floating window: {}", e));
                }
            } else {
                let result = match landing {
                    Landing::Stack(col) => workspace.insert_window_in_column(focused_hwnd, col),
                    Landing::NewColumn(col) => {
                        workspace.insert_window_at_column(focused_hwnd, tiled_width, col)
                    }
                    Landing::Default => workspace.insert_window(focused_hwnd, tiled_width),
                };
                if let Err(e) = result {
                    // Rollback: restore the window to its original source position
                    // so a failed move leaves the layout exactly as it was.
                    if let Some(src_ws) = self
                        .workspaces
                        .get_mut(&monitor)
                        .and_then(|v| v.get_mut(current_idx))
                    {
                        let rejoin = source_origin
                            .and_then(|(_, sib)| sib)
                            .and_then(|s| src_ws.find_window_location(s))
                            .map(|(c, _)| c);
                        let _ = match (rejoin, source_origin) {
                            (Some(c), _) => src_ws.insert_window_in_column(focused_hwnd, c),
                            (None, Some((col, _))) => {
                                src_ws.insert_window_at_column(focused_hwnd, tiled_width, col)
                            }
                            (None, None) => src_ws.insert_window(focused_hwnd, tiled_width),
                        };
                    }
                    return IpcResponse::error(format!(
                        "Failed to add window to target workspace: {}",
                        e
                    ));
                }
                // A rejoin (Stack) doesn't move focus; focus the window so it is
                // current when the user next switches to the target workspace,
                // matching the default insert.
                let _ = workspace.focus_window(focused_hwnd);
            }
        }

        // Record where the window came from so moving it back restores its
        // column; a floating move clears any stale tiled origin instead.
        if let Some((column, sibling)) = source_origin {
            self.move_origins.insert(
                focused_hwnd,
                crate::state::MoveOrigin {
                    monitor,
                    ws_idx: current_idx,
                    column,
                    sibling,
                },
            );
        } else if is_floating {
            self.move_origins.remove(&focused_hwnd);
        }

        // Target workspace is not active — hide the moved window
        // (capture-on-hide first for the overview's snapshot mode).
        if self.config.overview.render == crate::config::OverviewRender::Snapshot {
            let _ = leopardwm_platform_win32::snapshot::snapshot_capture(focused_hwnd);
        }
        let _ = move_window_offscreen(focused_hwnd);

        // Ensure the source workspace scrolls to show its new focused window
        let viewport_width = self.viewport_width_for(monitor);
        if let Some(workspace) = self
            .workspaces
            .get_mut(&monitor)
            .and_then(|v| v.get_mut(current_idx))
        {
            workspace.ensure_focused_visible_animated(viewport_width);
        }

        self.start_layout_transition(snapshot);
        if let Err(e) = self.apply_layout() {
            return IpcResponse::error(format!("Failed to apply layout: {}", e));
        }
        // The moved window now lives on an inactive workspace; cloak it so its
        // taskbar button goes too.
        self.sync_taskbar_buttons();
        self.sync_foreground_window();
        info!("Moved window {} to workspace {}", focused_hwnd, index);
        IpcResponse::Ok
    }

    /// Handle `IpcCommand::HealthCheck`.
    fn handle_health_check(&mut self) -> IpcResponse {
        let uptime = self.start_time.elapsed().as_secs();
        let total_windows: usize = self
            .workspaces
            .values()
            .flat_map(|ws_vec| ws_vec.iter())
            .map(|ws| ws.window_count() + ws.floating_count())
            .sum();
        IpcResponse::HealthInfo {
            healthy: true,
            uptime_seconds: uptime,
            total_windows,
            monitors: self.monitors.len(),
            paused: self.paused,
            thumbnail_register_balance:
                leopardwm_platform_win32::thumbnail::current_register_balance(),
            elevation_blocked_windows: {
                // (hwnd, title), sorted for stable doctor/IPC output (HashMap
                // order is random); sort by title then hwnd.
                let mut windows: Vec<(u64, String)> = self
                    .elevation_blocked
                    .iter()
                    .map(|(&hwnd, title)| (hwnd, title.clone()))
                    .collect();
                windows.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
                windows
            },
        }
    }

    /// Handle `IpcCommand::SetAutoStart`.
    fn handle_set_auto_start(&mut self, enabled: bool) -> IpcResponse {
        use leopardwm_platform_win32::autostart;
        let result = if enabled {
            match std::env::current_exe() {
                Ok(exe) => autostart::enable_autostart(&exe).map(|()| exe),
                Err(e) => Err(anyhow::anyhow!("resolve daemon executable: {}", e)),
            }
        } else {
            autostart::disable_autostart().map(|()| std::path::PathBuf::new())
        };
        match result {
            Ok(exe) => {
                if enabled {
                    info!("Auto-start enabled (path: {})", exe.display());
                } else {
                    info!("Auto-start disabled");
                }
                IpcResponse::Ok
            }
            Err(e) => IpcResponse::error(format!("Failed to update auto-start: {}", e)),
        }
    }

    /// Handle `IpcCommand::SetActiveTab`.
    fn handle_set_active_tab(&mut self, column: usize, tab: usize) -> IpcResponse {
        // Pre-arm the same-column-suppression bypass so the
        // synthesized SetForegroundWindow that follows doesn't get
        // squashed as redundant intra-column churn.
        let monitor = self.focused_monitor;
        let ws_idx = self.active_workspace_idx(monitor);
        self.pending_tab_focus = Some(crate::state::PendingTabFocus {
            monitor,
            workspace_idx: ws_idx,
            column_idx: column,
            tab_idx: tab,
            set_at: std::time::Instant::now(),
        });
        let Some(workspace) = self.focused_workspace_mut() else {
            return IpcResponse::error("No focused workspace");
        };
        if let Err(e) = workspace.set_active_tab(column, tab) {
            self.pending_tab_focus = None;
            return IpcResponse::error(format!("set_active_tab failed: {}", e));
        }
        if let Err(e) = self.apply_layout() {
            return IpcResponse::error(format!("apply_layout failed: {}", e));
        }
        self.sync_foreground_window();
        info!("Set active tab: column={}, tab={}", column, tab);
        IpcResponse::Ok
    }
}

#[cfg(test)]
mod focus_nav_tests {
    use super::is_focus_navigation;
    use leopardwm_ipc::IpcCommand;

    #[test]
    fn focus_navigation_covers_focus_and_monitor_commands() {
        for cmd in [
            IpcCommand::FocusLeft,
            IpcCommand::FocusRight,
            IpcCommand::FocusUp,
            IpcCommand::FocusDown,
            IpcCommand::FocusNext,
            IpcCommand::FocusPrev,
            IpcCommand::FocusStart,
            IpcCommand::FocusEnd,
            IpcCommand::FocusMonitorLeft,
            IpcCommand::FocusMonitorRight,
            IpcCommand::FocusMonitorUp,
            IpcCommand::FocusMonitorDown,
            IpcCommand::MoveWindowToMonitorLeft,
            IpcCommand::MoveWindowToMonitorRight,
            IpcCommand::MoveWindowToMonitorUp,
            IpcCommand::MoveWindowToMonitorDown,
        ] {
            assert!(is_focus_navigation(&cmd), "{cmd:?} should warp the cursor");
        }
    }

    #[test]
    fn focus_navigation_excludes_workspace_and_structural_commands() {
        for cmd in [
            IpcCommand::WorkspaceNext,
            IpcCommand::WorkspacePrev,
            IpcCommand::MoveToWorkspaceNext,
            IpcCommand::MoveToWorkspacePrev,
            IpcCommand::MoveToWorkspace { index: 3 },
            IpcCommand::MoveWindowLeft,
            IpcCommand::MoveWindowUp,
            IpcCommand::CloseWindow,
        ] {
            assert!(
                !is_focus_navigation(&cmd),
                "{cmd:?} should not warp the cursor"
            );
        }
    }
}
