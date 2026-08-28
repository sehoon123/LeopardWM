//! Scratchpad: one designated window that hides to a holding area and
//! re-summons floating + centered on a hotkey.
//!
//! While hidden, the window is removed from every workspace (so the
//! layout engine never touches it) and DWM-cloaked in place. While shown,
//! it lives as a floating window on whichever workspace was active when it
//! was summoned. Session-scoped: the designation is keyed by HWND and is
//! not persisted across a daemon restart.

use crate::state::{AppState, ScratchpadState, SCRATCHPAD_TOTAL_MARGIN};
use anyhow::{anyhow, Result};
use leopardwm_core_layout::Rect;
use tracing::info;
#[cfg(not(test))]
use tracing::warn;

impl AppState {
    /// Remove `wid` from whichever workspace currently holds it (tiled or
    /// floating). Returns true if it was found and removed.
    fn detach_window_from_workspace(&mut self, wid: u64) -> bool {
        let Some((mon, ws_idx)) = self.find_window_workspace(wid) else {
            return false;
        };
        if let Some(ws) = self
            .workspaces
            .get_mut(&mon)
            .and_then(|v| v.get_mut(ws_idx))
        {
            if ws.is_floating(wid) {
                ws.remove_floating(wid);
            } else {
                let _ = ws.remove_window(wid);
            }
            return true;
        }
        false
    }

    fn park_scratchpad_window(&self, wid: u64) -> Result<()> {
        #[cfg(test)]
        {
            if self.injected_scratchpad_park_failure {
                return Err(anyhow!("injected scratchpad park failure"));
            }
            let _ = wid;
            Ok(())
        }
        #[cfg(not(test))]
        {
            leopardwm_platform_win32::move_window_offscreen(wid)
                .map_err(|error| anyhow!(error.to_string()))
        }
    }

    fn cloak_scratchpad_window(&self, wid: u64) {
        #[cfg(not(test))]
        leopardwm_platform_win32::dwm_cloak_window(wid);
        #[cfg(test)]
        let _ = wid;
    }

    fn uncloak_scratchpad_window(&self, wid: u64) {
        #[cfg(not(test))]
        leopardwm_platform_win32::dwm_uncloak_window(wid);
        #[cfg(test)]
        let _ = wid;
    }

    fn show_scratchpad_window(&self, wid: u64) -> Result<()> {
        #[cfg(test)]
        {
            let _ = wid;
            Ok(())
        }
        #[cfg(not(test))]
        {
            leopardwm_platform_win32::show_window_no_activate(wid)
                .map_err(|error| anyhow!(error.to_string()))
        }
    }

    fn restore_scratchpad_window(&self, wid: u64, rect: Rect) -> Result<()> {
        #[cfg(test)]
        {
            let _ = (wid, rect);
            Ok(())
        }
        #[cfg(not(test))]
        {
            leopardwm_platform_win32::position_window(wid, rect)
                .map_err(|error| anyhow!(error.to_string()))
        }
    }

    fn raise_scratchpad_window(&self, wid: u64) -> Result<()> {
        #[cfg(test)]
        {
            if self.injected_scratchpad_raise_failure {
                return Err(anyhow!("injected scratchpad raise failure"));
            }
            let _ = wid;
            Ok(())
        }
        #[cfg(not(test))]
        {
            leopardwm_platform_win32::raise_window(wid).map_err(|error| anyhow!(error.to_string()))
        }
    }

    fn prepare_scratchpad_ownership_change(&mut self) -> Result<()> {
        if self.paused {
            return Err(anyhow!(
                "scratchpad ownership cannot change while tiling is paused"
            ));
        }
        if self.drag_state.is_some() || self.resize_hwnd.is_some() {
            return Err(anyhow!(
                "scratchpad ownership cannot change during an active move/resize"
            ));
        }
        self.prepare_workspace_ownership_change()
            .map_err(|error| anyhow!("could not fence queued layout work: {error}"))?;
        self.prune_stale_windows();
        #[cfg(not(test))]
        if let Some(dead) = self
            .scratchpad
            .map(|scratchpad| scratchpad.window_id)
            .filter(|window_id| !leopardwm_platform_win32::is_window_valid(*window_id))
        {
            self.clear_recycled_hwnd_metadata(dead);
        }
        Ok(())
    }

    /// Designate the focused window as the scratchpad and hide it. If a
    /// different scratchpad is already stashed, summon it back first so it
    /// is not stranded hidden.
    pub(crate) fn scratchpad_stash(&mut self) -> Result<()> {
        self.prepare_scratchpad_ownership_change()?;
        let tiled_focus = self
            .focused_workspace()
            .and_then(|workspace| workspace.focused_window());
        #[cfg(not(test))]
        let live_foreground = leopardwm_platform_win32::get_foreground_window().filter(|wid| {
            self.focused_workspace()
                .is_some_and(|workspace| workspace.contains_window(*wid))
        });
        #[cfg(test)]
        let live_foreground = None;
        let floating_focus = live_foreground
            .or(self.previous_focused_hwnd)
            .filter(|wid| {
                self.focused_workspace()
                    .is_some_and(|workspace| workspace.is_floating(*wid))
            });
        let wid = floating_focus
            .or(live_foreground)
            .or(tiled_focus)
            .ok_or_else(|| {
                anyhow!("scratchpad stash has no focused window on the active workspace")
            })?;

        if let Some(state) = self.scratchpad.filter(|state| state.window_id == wid) {
            let _ = self.snapshot_managed_floating_geometry(wid);
            self.release_to_tiling(state)?;
            self.scratchpad = None;
            self.sync_taskbar_buttons();
            self.sync_foreground_window();
            info!("Scratchpad: released window {} back to tiling", wid);
            return Ok(());
        }

        #[cfg(not(test))]
        if !leopardwm_platform_win32::is_window_valid(wid) {
            return Err(anyhow!("scratchpad source {wid} is no longer valid"));
        }

        // Never strand an older designation. Its release is itself verified
        // before ownership of the newly selected source is recorded.
        if let Some(previous) = self.scratchpad {
            let _ = self.snapshot_managed_floating_geometry(previous.window_id);
            self.release_to_tiling(previous)?;
            self.scratchpad = None;
        }

        let last_size = if self.config.layout.remember_scratchpad_size {
            self.snapshot_managed_floating_geometry(wid)
                .map(|rect| self.logical_floating_size_for_rect(rect))
        } else {
            None
        };
        let (origin_column, origin_sibling, origin_width) = self
            .focused_workspace()
            .and_then(|workspace| {
                workspace.find_window_location(wid).and_then(|(column, _)| {
                    let source = workspace.column(column)?;
                    let sibling = source
                        .windows()
                        .iter()
                        .copied()
                        .find(|window| *window != wid);
                    Some((column, sibling, Some(source.width())))
                })
            })
            .unwrap_or_else(|| {
                (
                    self.focused_workspace()
                        .map(|workspace| workspace.focused_column_index())
                        .unwrap_or(0),
                    None,
                    None,
                )
            });
        let previous_designation = self.scratchpad;
        self.scratchpad = Some(ScratchpadState {
            window_id: wid,
            shown: false,
            origin_column,
            origin_sibling,
            origin_width,
            last_size,
        });
        self.move_origins.remove(&wid);
        if let Err(error) = self.hide_window_to_holding(wid) {
            self.scratchpad = previous_designation;
            return Err(error);
        }
        self.sync_taskbar_buttons();
        self.sync_foreground_window();
        info!("Scratchpad: stashed window {}", wid);
        Ok(())
    }

    /// Return `wid` to the active workspace as a tiled window: detach any
    /// floating entry, ensure it is uncloaked, then rejoin its original column
    /// if a window that shared it survives (found by `origin_sibling`, so it
    /// works even if column indices shifted). If that column is gone, fall back
    /// to a new column at `origin_column`. The subsequent `apply_layout`
    /// repositions it on-screen, overriding any off-screen parking.
    fn release_to_tiling(&mut self, state: ScratchpadState) -> Result<()> {
        let wid = state.window_id;
        let workspaces_backup = self.workspaces.clone();
        let previous_focus = self.previous_focused_hwnd;
        self.detach_window_from_workspace(wid);
        self.uncloak_scratchpad_window(wid);
        let viewport_width = self.viewport_width_for(self.focused_monitor);
        let reinserted = self
            .focused_workspace_mut()
            .map(|workspace| {
                let rejoin_column = state
                    .origin_sibling
                    .and_then(|sibling| workspace.find_window_location(sibling))
                    .map(|(column, _)| column);
                let inserted = match rejoin_column {
                    Some(column) => workspace.insert_window_in_column(wid, column).is_ok(),
                    None => workspace
                        .insert_window_at_column(wid, state.origin_width, state.origin_column)
                        .is_ok(),
                };
                if inserted {
                    let _ = workspace.focus_window(wid);
                    workspace.ensure_focused_visible(viewport_width);
                }
                inserted
            })
            .unwrap_or(false);
        if !reinserted {
            self.workspaces = workspaces_backup;
            self.previous_focused_hwnd = previous_focus;
            return Err(anyhow!(
                "scratchpad source {wid} could not return to tiling"
            ));
        }

        self.last_placed_layout_rects.clear();
        if let Err(error) = self.apply_layout() {
            self.workspaces = workspaces_backup;
            self.previous_focused_hwnd = previous_focus;
            self.last_placed_layout_rects.clear();
            let physical_rollback = if state.shown {
                self.uncloak_scratchpad_window(wid);
                self.apply_layout()
            } else {
                self.cloak_scratchpad_window(wid);
                self.park_scratchpad_window(wid)
                    .and_then(|_| self.apply_layout())
            };
            if physical_rollback.is_err() {
                self.paused = true;
            }
            return Err(anyhow!(
                "scratchpad release rolled back after placement failure: {error}; rollback={physical_rollback:?}"
            ));
        }
        self.disable_snap_for_window(wid);
        self.move_origins.remove(&wid);
        Ok(())
    }

    /// Remove `wid` from its workspace and hide it: cloak (hides from
    /// Alt-Tab/taskbar) AND park off-screen. The off-screen move is what
    /// actually removes it from view — cloaking the *foreground* window
    /// alone does not reliably hide it. Both are recovery-safe: shutdown /
    /// panic / `emergency-uncloak` drains the direct-cloak set and
    /// re-homes any off-screen window.
    fn hide_window_to_holding(&mut self, wid: u64) -> Result<()> {
        let workspaces_backup = self.workspaces.clone();
        let previous_focus = self.previous_focused_hwnd;
        let original_rect = leopardwm_platform_win32::get_window_chrome_rect(wid);
        let _ = self.snapshot_managed_floating_geometry(wid);
        self.detach_window_from_workspace(wid);
        self.cloak_scratchpad_window(wid);
        if let Err(error) = self.park_scratchpad_window(wid) {
            self.workspaces = workspaces_backup;
            self.previous_focused_hwnd = previous_focus;
            self.uncloak_scratchpad_window(wid);
            self.last_placed_layout_rects.clear();
            let rollback = if self.paused {
                Err(anyhow!("tiling paused before rollback"))
            } else {
                self.apply_layout()
            };
            if rollback.is_err() {
                self.paused = true;
            }
            return Err(anyhow!(
                "scratchpad source {wid} could not be parked: {error}; rollback={rollback:?}"
            ));
        }

        let viewport_width = self.viewport_width_for(self.focused_monitor);
        if let Some(workspace) = self.focused_workspace_mut() {
            workspace.ensure_focused_visible(viewport_width);
        }
        self.last_placed_layout_rects.clear();
        if let Err(error) = self.apply_layout() {
            self.workspaces = workspaces_backup;
            self.previous_focused_hwnd = previous_focus;
            self.uncloak_scratchpad_window(wid);
            let reposition = original_rect
                .ok_or_else(|| anyhow!("original scratchpad geometry unavailable"))
                .and_then(|rect| self.restore_scratchpad_window(wid, rect));
            self.last_placed_layout_rects.clear();
            let relayout = if self.paused {
                Err(anyhow!("tiling paused before rollback"))
            } else {
                self.apply_layout()
            };
            if reposition.is_err() || relayout.is_err() {
                self.paused = true;
            }
            return Err(anyhow!(
                "scratchpad hide rolled back after sibling relayout failure: {error}; reposition={reposition:?}; relayout={relayout:?}"
            ));
        }
        #[cfg(not(test))]
        leopardwm_platform_win32::taskbar::taskbar_hide(wid);
        Ok(())
    }

    /// Add `wid` as a floating, centered window on the active workspace,
    /// uncloak it, position it, and let the OS foreground event drive
    /// focus + the border. Returns `false` if the window is gone or could
    /// not be floated, so the caller can drop the designation.
    fn scratchpad_show(&mut self, wid: u64) -> Result<()> {
        #[cfg(not(test))]
        if !leopardwm_platform_win32::is_window_valid(wid) {
            return Err(anyhow!("scratchpad source {wid} is gone"));
        }
        let logical_size = if self.config.layout.remember_scratchpad_size {
            self.scratchpad
                .filter(|state| state.window_id == wid)
                .and_then(|state| state.last_size)
                .unwrap_or_else(|| self.default_scratchpad_size())
        } else {
            self.default_scratchpad_size()
        };
        let rect = self.centered_rect_for_logical_floating_size(
            self.focused_monitor,
            logical_size,
            SCRATCHPAD_TOTAL_MARGIN,
        );
        let workspaces_backup = self.workspaces.clone();
        let previous_focus = self.previous_focused_hwnd;
        self.detach_window_from_workspace(wid);
        self.uncloak_scratchpad_window(wid);
        if let Err(error) = self.show_scratchpad_window(wid) {
            self.workspaces = workspaces_backup;
            self.previous_focused_hwnd = previous_focus;
            self.cloak_scratchpad_window(wid);
            let _ = self.park_scratchpad_window(wid);
            return Err(anyhow!(
                "scratchpad source {wid} could not be shown: {error}"
            ));
        }
        let floated = self
            .focused_workspace_mut()
            .map(|workspace| {
                let added = workspace.add_floating(wid, rect).is_ok();
                if added {
                    let _ = workspace.focus_window(wid);
                }
                added
            })
            .unwrap_or(false);
        if !floated {
            self.workspaces = workspaces_backup;
            self.previous_focused_hwnd = previous_focus;
            self.cloak_scratchpad_window(wid);
            let _ = self.park_scratchpad_window(wid);
            return Err(anyhow!(
                "scratchpad source {wid} could not attach as a float"
            ));
        }

        self.last_placed_layout_rects.clear();
        let placement = self
            .apply_layout()
            .and_then(|_| self.raise_scratchpad_window(wid));
        if let Err(error) = placement {
            self.workspaces = workspaces_backup;
            self.previous_focused_hwnd = previous_focus;
            self.cloak_scratchpad_window(wid);
            let park = self.park_scratchpad_window(wid);
            self.last_placed_layout_rects.clear();
            let relayout = if self.paused {
                Err(anyhow!("tiling paused before rollback"))
            } else {
                self.apply_layout()
            };
            if park.is_err() || relayout.is_err() {
                self.paused = true;
            }
            return Err(anyhow!(
                "scratchpad summon rolled back after physical placement failure: {error}; park={park:?}; relayout={relayout:?}"
            ));
        }

        #[cfg(not(test))]
        {
            leopardwm_platform_win32::taskbar::taskbar_show(wid);
            if !leopardwm_platform_win32::set_foreground_window(wid).unwrap_or(false) {
                warn!("Scratchpad {wid} is visible but Windows refused foreground transfer");
            }
        }
        self.move_origins.remove(&wid);
        Ok(())
    }

    /// Hide the currently-shown scratchpad window.
    fn scratchpad_hide(&mut self, wid: u64) -> Result<()> {
        self.hide_window_to_holding(wid)?;
        self.sync_foreground_window();
        Ok(())
    }

    /// Toggle scratchpad visibility (summon if hidden, hide if shown).
    pub(crate) fn scratchpad_toggle(&mut self) -> Result<()> {
        self.prepare_scratchpad_ownership_change()?;
        let state = self
            .scratchpad
            .ok_or_else(|| anyhow!("no scratchpad is designated"))?;
        if state.shown {
            self.scratchpad_hide(state.window_id)?;
            if let Some(current) = self.scratchpad.as_mut() {
                if current.window_id == state.window_id {
                    current.shown = false;
                }
            }
            self.sync_taskbar_buttons();
            info!("Scratchpad: hid window {}", state.window_id);
        } else {
            self.scratchpad_show(state.window_id)?;
            if let Some(current) = self.scratchpad.as_mut() {
                if current.window_id == state.window_id {
                    current.shown = true;
                }
            }
            self.sync_taskbar_buttons();
            info!("Scratchpad: summoned window {}", state.window_id);
        }
        Ok(())
    }

    /// Clear the scratchpad designation if `wid` was the scratchpad
    /// (called when a window is destroyed).
    pub(crate) fn scratchpad_on_window_destroyed(&mut self, wid: u64) {
        if self.scratchpad.map(|s| s.window_id) == Some(wid) {
            self.scratchpad = None;
            info!("Scratchpad: designated window {} closed; cleared", wid);
        }
    }

    /// Re-focus the scratchpad after a workspace switch if it is shown and
    /// lives on the now-active workspace. A summoned scratchpad is a
    /// floating window on its workspace; switching away and back leaves it
    /// visible but focus lands on a tiled window, so it needs an explicit
    /// re-focus. No-op if there's no shown scratchpad on the active
    /// workspace.
    pub(crate) fn refocus_scratchpad_if_active(&mut self) {
        let Some(sp) = self.scratchpad else { return };
        if !sp.shown {
            return;
        }
        let wid = sp.window_id;
        let active = self.active_workspace_idx(self.focused_monitor);
        let on_active_workspace = self
            .workspaces
            .get(&self.focused_monitor)
            .and_then(|v| v.get(active))
            .is_some_and(|ws| ws.contains_window(wid));
        if !on_active_workspace {
            return;
        }
        if let Some(ws) = self.focused_workspace_mut() {
            let _ = ws.focus_window(wid);
        }
        self.previous_focused_hwnd = Some(wid);
        #[cfg(not(test))]
        {
            let _ = leopardwm_platform_win32::set_foreground_window(wid);
        }
    }
}
