//! Scratchpad: one designated window that hides to a holding area and
//! re-summons floating + centered on a hotkey.
//!
//! While hidden, the window is removed from every workspace (so the
//! layout engine never touches it) and DWM-cloaked in place. While shown,
//! it lives as a floating window on whichever workspace was active when it
//! was summoned. Session-scoped: the designation is keyed by HWND and is
//! not persisted across a daemon restart.

use crate::state::{AppState, ScratchpadState, SCRATCHPAD_TOTAL_MARGIN};
use leopardwm_core_layout::Rect;
use tracing::{info, warn};

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

    /// A centered default scratchpad rectangle on the focused monitor's work
    /// area. Scratchpads with a remembered size use that size at summon time.
    fn centered_default_scratchpad_rect(&self) -> Rect {
        self.centered_rect_for_logical_floating_size(
            self.focused_monitor,
            self.default_scratchpad_size(),
            SCRATCHPAD_TOTAL_MARGIN,
        )
    }

    /// Designate the focused window as the scratchpad and hide it. If a
    /// different scratchpad is already stashed, summon it back first so it
    /// is not stranded hidden.
    pub(crate) fn scratchpad_stash(&mut self) {
        // Prefer the OS-foreground window only when the active workspace
        // confirms it is floating. Floating focus is tracked outside the
        // column focus model, so always preferring `Workspace::focused_window`
        // would stash a tile instead of a focused regular float. Otherwise the
        // tiled focus remains authoritative and immune to a stale foreground
        // event from a tiled window the user just left.
        let tiled_focus = self.focused_workspace().and_then(|ws| ws.focused_window());
        let floating_focus = self.previous_focused_hwnd.filter(|wid| {
            self.focused_workspace()
                .is_some_and(|workspace| workspace.is_floating(*wid))
        });
        let wid = floating_focus
            .or(tiled_focus)
            .or(self.previous_focused_hwnd);
        let Some(wid) = wid else {
            info!("Scratchpad stash: no focused window");
            return;
        };

        // Stashing the window that is already the scratchpad releases it
        // back to the tiled layout. Capture its geometry while the
        // designation still owns it, then clear the designation before
        // reattaching the window.
        if let Some(sp) = self.scratchpad {
            if sp.window_id == wid {
                let _ = self.snapshot_managed_floating_geometry(wid);
                self.scratchpad = None;
                self.release_to_tiling(wid, sp.origin_column, sp.origin_sibling);
                // Keep focus on the returned window — it was focused while
                // summoned, so re-tiling it (especially back into a stack)
                // shouldn't hand focus to a sibling.
                if let Some(ws) = self.focused_workspace_mut() {
                    let _ = ws.focus_window(wid);
                }
                let _ = self.apply_layout();
                self.sync_foreground_window();
                info!("Scratchpad: released window {} back to tiling", wid);
                return;
            }
        }

        // Only stash a window that still exists; otherwise we would cloak /
        // move a dead HWND and record a dangling designation.
        #[cfg(not(test))]
        if !leopardwm_platform_win32::is_window_valid(wid) {
            info!(
                "Scratchpad stash: focused window {} is no longer valid",
                wid
            );
            return;
        }

        // Designating a new scratchpad: release any existing one back to
        // tiling first so it is not orphaned hidden. Capture a shown window
        // while its designation still owns its size, then clear the
        // designation before reattaching it.
        if let Some(prev) = self.scratchpad {
            let _ = self.snapshot_managed_floating_geometry(prev.window_id);
            self.scratchpad = None;
            self.release_to_tiling(prev.window_id, prev.origin_column, prev.origin_sibling);
        }

        // Preserve a regular floating window's current size when it becomes
        // a scratchpad. Tiled windows start with no size memory and use the
        // configured scratchpad default on their first summon.
        let last_size = if self.config.layout.remember_scratchpad_size {
            self.snapshot_managed_floating_geometry(wid)
                .map(|rect| self.logical_floating_size_for_rect(rect))
        } else {
            None
        };

        // Remember where it sat so releasing later restores it to the same
        // spot: the column index (fallback) and a window that shared the
        // column (so it can rejoin that exact column even if indices shift).
        let (origin_column, origin_sibling) = self
            .focused_workspace()
            .and_then(|ws| {
                ws.find_window_location(wid).map(|(col, _)| {
                    let sibling = ws
                        .columns()
                        .get(col)
                        .and_then(|c| c.windows().iter().copied().find(|&w| w != wid));
                    (col, sibling)
                })
            })
            .unwrap_or_else(|| {
                let col = self
                    .focused_workspace()
                    .map(|ws| ws.focused_column_index())
                    .unwrap_or(0);
                (col, None)
            });

        // Record the designation BEFORE hiding, so if anything aborts
        // mid-hide the daemon still knows it owns this window (the destroyed
        // handler, next toggle, and shutdown/emergency recovery can all act
        // on it) rather than leaving it cloaked/off-screen with no owner.
        self.scratchpad = Some(ScratchpadState {
            window_id: wid,
            shown: false,
            origin_column,
            origin_sibling,
            last_size,
        });
        self.hide_window_to_holding(wid);
        let _ = self.apply_layout();
        self.sync_foreground_window();
        info!("Scratchpad: stashed window {}", wid);
    }

    /// Return `wid` to the active workspace as a tiled window: detach any
    /// floating entry, ensure it is uncloaked, then rejoin its original column
    /// if a window that shared it survives (found by `origin_sibling`, so it
    /// works even if column indices shifted). If that column is gone, fall back
    /// to a new column at `origin_column`. The subsequent `apply_layout`
    /// repositions it on-screen, overriding any off-screen parking.
    fn release_to_tiling(&mut self, wid: u64, origin_column: usize, origin_sibling: Option<u64>) {
        self.detach_window_from_workspace(wid);
        leopardwm_platform_win32::dwm_uncloak_window(wid);
        let reinserted = self
            .focused_workspace_mut()
            .map(|ws| {
                let rejoin_column = origin_sibling
                    .and_then(|s| ws.find_window_location(s))
                    .map(|(col, _)| col);
                match rejoin_column {
                    Some(col) => ws.insert_window_in_column(wid, col).is_ok(),
                    None => ws.insert_window_at_column(wid, None, origin_column).is_ok(),
                }
            })
            .unwrap_or(false);
        if !reinserted {
            // Reattach failed (no workspace, or a duplicate that detach
            // somehow missed). The window is uncloaked but may still be
            // parked off-screen from the holding state, so pull it back
            // on-screen rather than leave it lost.
            warn!(
                "Scratchpad: could not re-tile window {}; restoring it on-screen",
                wid
            );
            let rect = self.centered_default_scratchpad_rect();
            let _ = leopardwm_platform_win32::position_window(wid, rect);
        }
    }

    /// Remove `wid` from its workspace and hide it: cloak (hides from
    /// Alt-Tab/taskbar) AND park off-screen. The off-screen move is what
    /// actually removes it from view — cloaking the *foreground* window
    /// alone does not reliably hide it. Both are recovery-safe: shutdown /
    /// panic / `emergency-uncloak` drains the direct-cloak set and
    /// re-homes any off-screen window.
    fn hide_window_to_holding(&mut self, wid: u64) {
        let _ = self.snapshot_managed_floating_geometry(wid);
        self.detach_window_from_workspace(wid);
        leopardwm_platform_win32::dwm_cloak_window(wid);
        let _ = leopardwm_platform_win32::move_window_offscreen(wid);
    }

    /// Add `wid` as a floating, centered window on the active workspace,
    /// uncloak it, position it, and let the OS foreground event drive
    /// focus + the border. Returns `false` if the window is gone or could
    /// not be floated, so the caller can drop the designation.
    fn scratchpad_show(&mut self, wid: u64) -> bool {
        #[cfg(not(test))]
        if !leopardwm_platform_win32::is_window_valid(wid) {
            warn!("Scratchpad: cannot summon window {}; it is gone", wid);
            return false;
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
        self.detach_window_from_workspace(wid);
        leopardwm_platform_win32::dwm_uncloak_window(wid);
        let floated = self
            .focused_workspace_mut()
            .map(|ws| {
                let ok = ws.add_floating(wid, rect).is_ok();
                if ok {
                    let _ = ws.focus_window(wid);
                }
                ok
            })
            .unwrap_or(false);
        if !floated {
            // Uncloaked but not attached to a workspace. Pull it on-screen so
            // the now-visible window is not stranded at its off-screen park.
            warn!("Scratchpad: could not float window {} on summon", wid);
            let _ = leopardwm_platform_win32::position_window(wid, rect);
            return false;
        }
        let _ = self.apply_layout();
        // Layout places floating windows asynchronously; force the final
        // position synchronously so the window is physically centered.
        let _ = leopardwm_platform_win32::position_window(wid, rect);
        // Deliberately do NOT pre-set previous_focused_hwnd here. Setting
        // the OS foreground fires EVENT_SYSTEM_FOREGROUND; the Focused
        // handler then shows the border once the window has composited at
        // its new spot (its DWM frame bounds, which the border reads, are
        // stale for a frame right after uncloak+move). Pre-setting the
        // focus would make that handler dedupe-skip and the border would
        // track the stale rect — the "no border on first summon" bug.
        #[cfg(not(test))]
        {
            let _ = leopardwm_platform_win32::set_foreground_window(wid);
        }
        true
    }

    /// Hide the currently-shown scratchpad window.
    fn scratchpad_hide(&mut self, wid: u64) {
        self.hide_window_to_holding(wid);
        let _ = self.apply_layout();
        self.sync_foreground_window();
    }

    /// Toggle scratchpad visibility (summon if hidden, hide if shown).
    pub(crate) fn scratchpad_toggle(&mut self) {
        let Some(state) = self.scratchpad else {
            info!("Scratchpad toggle: none designated");
            return;
        };
        if state.shown {
            self.scratchpad_hide(state.window_id);
            if let Some(current) = self.scratchpad.as_mut() {
                if current.window_id == state.window_id {
                    current.shown = false;
                }
            }
            info!("Scratchpad: hid window {}", state.window_id);
        } else if self.scratchpad_show(state.window_id) {
            if let Some(current) = self.scratchpad.as_mut() {
                if current.window_id == state.window_id {
                    current.shown = true;
                }
            }
            info!("Scratchpad: summoned window {}", state.window_id);
        } else {
            // Window vanished or could not be floated; drop the designation
            // rather than keep a dangling, un-summonable scratchpad.
            self.scratchpad = None;
            info!(
                "Scratchpad: summon of window {} failed; cleared designation",
                state.window_id
            );
        }
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
