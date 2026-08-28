//! Drag-and-drop handling for AppState.

use crate::state::{
    AppState, DragHintAction, DragMode, DragState, DropTarget, DRAG_PLACEHOLDER_HWND,
    FALLBACK_VIEWPORT_WIDTH,
};
use leopardwm_core_layout::{Rect, Visibility};
use leopardwm_platform_win32::MonitorId;
use tracing::{debug, info, warn};

impl AppState {
    /// Remove the drag placeholder window from all workspaces on all monitors.
    pub(crate) fn clear_drag_placeholder(&mut self) {
        for ws_vec in self.workspaces.values_mut() {
            for ws in ws_vec.iter_mut() {
                let _ = ws.remove_window(DRAG_PLACEHOLDER_HWND);
            }
        }
    }

    /// Remove the live-preview placeholder from the one workspace named by
    /// the previous drop target. Full-workspace cleanup remains the fallback
    /// for the first tick and drag-end recovery, but steady 60 Hz hint updates
    /// avoid scanning every monitor × workspace.
    fn remove_previous_drag_placeholder(&mut self) {
        let previous_target = self
            .drag_state
            .as_ref()
            .and_then(|drag| drag.last_drop_target);
        if let Some(target) = previous_target {
            let workspace_idx = self.active_workspace_idx(target.monitor);
            if let Some(workspace) = self
                .workspaces
                .get_mut(&target.monitor)
                .and_then(|workspaces| workspaces.get_mut(workspace_idx))
            {
                let _ = workspace.remove_window(DRAG_PLACEHOLDER_HWND);
                return;
            }
        }
        self.clear_drag_placeholder();
    }

    /// Compute and show a drag hint overlay.
    /// Default drag = move one window with merge enabled.
    /// Ctrl+Alt/config override = move one window as a standalone column.
    /// Shift+drag = move the complete column.
    pub(crate) fn update_drag_hint(&mut self, hwnd: u64) {
        // No drag ghost while tiling is paused.
        if self.paused {
            return;
        }

        // Re-pin the tab strip to the top of the z-order on every drag
        // mouse-move. The dragged window is being raised to `HWND_TOP`
        // by the OS continuously during a drag, which pushes our strip
        // behind it between repaints. `raise()` is a cheap z-order-only
        // SetWindowPos — no re-render — so calling it per mouse-move
        // doesn't add visible overhead.
        for strip in self.tab_strip_overlays.values() {
            strip.raise();
        }
        // Use the cursor for both monitor and drop hit-testing. The prior path
        // enumerated/cloned every monitor and fetched cross-process window
        // metadata on each 60 Hz hint tick, then waited until the WINDOW CENTER
        // crossed the seam. Cursor-first selection is cheaper and transfers as
        // soon as the user's pointer enters the other monitor. Only query the
        // window as a rare fallback when GetCursorPos fails.
        let (cursor_x, cursor_y) = match leopardwm_platform_win32::get_cursor_pos() {
            Some(point) => point,
            None => {
                let Some(win_info) = self.lookup_window_info(hwnd) else {
                    return;
                };
                (
                    win_info.rect.x + win_info.rect.width / 2,
                    win_info.rect.y + win_info.rect.height / 2,
                )
            }
        };

        // Read the mode latched at MoveSizeStart together with source fields.
        // Never re-sample Shift after a window-merge preview may have detached
        // the HWND from its source column.
        let (mode, current_col, source_monitor, source_ws_idx) = match self.drag_state {
            Some(ref drag) => (
                drag.mode,
                drag.current_column_index,
                drag.source_monitor,
                drag.source_workspace_idx,
            ),
            None => return,
        };
        let target_monitor_id = self.monitor_for_drag_point(cursor_x, cursor_y, source_monitor);

        if mode == DragMode::Column {
            // --- Shift+drag: column reorder mode ---
            // Only live-reorder on the source monitor; cross-monitor happens on drop.
            if target_monitor_id != source_monitor {
                self.shift_drag_cross_monitor_hint(cursor_x, target_monitor_id);
            } else {
                self.shift_drag_reorder_hint(cursor_x, source_monitor, source_ws_idx, current_col);
            }
        } else if mode == DragMode::WindowAsColumn {
            // --- Ctrl+Alt/config override: standalone-window mode ---
            // Never insert a vertical/tab placeholder. Show only the horizontal
            // column insertion edge and perform the structural split/reorder on
            // drop, so this mode cannot accidentally consume into a column.
            self.standalone_window_drag_hint(cursor_x, target_monitor_id);
        } else {
            // --- Default drag: window merge mode with live preview ---
            // Source column keeps the dragged window (preserving its space).
            // Target column gets a placeholder so its windows shift to make room.

            if !self.monitors.contains_key(&target_monitor_id) {
                return;
            }
            let viewport = self.layout_viewport(target_monitor_id);

            // Remove the prior live-preview placeholder before recomputing
            // bounds, touching only its recorded workspace on steady ticks.
            self.remove_previous_drag_placeholder();

            let ws_idx = self.active_workspace_idx(target_monitor_id);
            let Some(workspace) = self
                .workspaces
                .get(&target_monitor_id)
                .and_then(|v| v.get(ws_idx))
            else {
                return;
            };
            let column_bounds = column_bounds_from_placements(workspace, viewport);

            // If the cursor is over a visible tab strip, route the drop
            // to that strip's owning column. The strip overhangs the
            // column rect (sits in the reserved gap above the active
            // tab), so a cursor over the strip wouldn't otherwise hit
            // any column via `compute_target_column_index`. Tab-index
            // within the strip is intentionally ignored — see the slot
            // computation below for the "always append" rationale.
            // Iterate every live strip — multiple strips can be visible
            // simultaneously (one per tabbed column), and any of them
            // may be under the cursor.
            let strip_hit = self
                .tab_strip_overlays
                .values()
                .find_map(|s| s.hit_test_screen(cursor_x, cursor_y));
            let target_col = strip_hit
                .map(|hit| hit.column_idx)
                .or_else(|| compute_target_column_index(&column_bounds, cursor_x));

            // A plain cross-monitor drag into an empty workspace or unused
            // horizontal area creates a standalone column. Previously this
            // returned without a target, so dropping onto an empty second
            // monitor always snapped back. Keep the live preview lightweight:
            // a vertical insertion line, with no structural mutation until
            // drop.
            let Some(target_col) = target_col else {
                if target_monitor_id == source_monitor {
                    return;
                }
                let insert_index = compute_insertion_index(&column_bounds, cursor_x);
                let gap = workspace.gap();
                let outer_left = workspace.outer_gaps().0.max(0);
                let hint_x = if column_bounds.is_empty() {
                    viewport.x.saturating_add(outer_left)
                } else {
                    compute_insertion_hint_x(&column_bounds, insert_index, gap)
                };
                let drop_target = DropTarget {
                    monitor: target_monitor_id,
                    insert_index,
                    window_slot: None,
                };
                let unchanged = self
                    .drag_state
                    .as_ref()
                    .is_some_and(|drag| drag.last_drop_target == Some(drop_target));
                if let Some(ref mut drag) = self.drag_state {
                    drag.last_drop_target = Some(drop_target);
                }
                if !unchanged {
                    self.pending_drag_hint = Some(DragHintAction::ShowGhost {
                        rect: Rect::new(hint_x - 2, viewport.y, 4, viewport.height),
                    });
                }
                return;
            };

            // Determine if the dragged window is in the target column.
            let is_same_column = workspace
                .column(target_col)
                .is_some_and(|c| c.contains(hwnd));

            let target_is_tabbed = workspace.column(target_col).is_some_and(|c| c.is_tabbed());

            let n_existing = workspace.column(target_col).map(|c| c.len()).unwrap_or(0);
            // Same column: N slots (reorder). Different column: N+1 (placeholder added).
            let n_total = if is_same_column {
                n_existing
            } else {
                n_existing + 1
            };
            if n_total == 0 {
                return;
            }

            let col_rect = match compute_column_rect(workspace, viewport, target_col) {
                Some(r) => r,
                None => return,
            };

            // Slot selection: Tabbed targets always append to the end
            // (Chrome new-tab semantics — "the tab I just added goes to
            // the right"). Strip-hit position is intentionally ignored
            // for Tabbed targets even when the user happens to drop on
            // a specific tab; the cursor's column-internal position has
            // no useful meaning in a Tabbed column where every tab
            // occupies the same screen rect, and predictability beats
            // cleverness here. Vertical targets still pick the slot
            // from the cursor's Y.
            let window_slot = if target_is_tabbed {
                n_total.saturating_sub(1)
            } else {
                compute_window_slot(&col_rect, n_total, cursor_y)
            };

            let drop_target = DropTarget {
                monitor: target_monitor_id,
                insert_index: target_col,
                window_slot: Some(window_slot),
            };
            let drop_target_unchanged = self
                .drag_state
                .as_ref()
                .is_some_and(|d| d.last_drop_target == Some(drop_target));
            if let Some(ref mut drag) = self.drag_state {
                drag.last_drop_target = Some(drop_target);
            }
            if drop_target_unchanged {
                // Layout is already correct from the previous tick — skip
                // apply_layout. The `clear_drag_placeholder()` call at the
                // top of this function did, however, remove the placeholder
                // we previously inserted; `finalize_drag_merge` relies on
                // finding the placeholder on drop, so re-insert it at the
                // same position for cross-column drags. Same-column drags
                // don't use a placeholder, so nothing to do there.
                if !is_same_column {
                    if let Some(ws) = self
                        .workspaces
                        .get_mut(&target_monitor_id)
                        .and_then(|v| v.get_mut(ws_idx))
                    {
                        let _ = ws.insert_window_in_column_at(
                            DRAG_PLACEHOLDER_HWND,
                            target_col,
                            window_slot,
                        );
                    }
                }
                return;
            }

            if is_same_column {
                self.merge_reorder_same_column(hwnd, target_monitor_id, target_col, window_slot);
            } else {
                // Different column: remove window from multi-window source so
                // remaining windows expand, then insert placeholder at target.
                let snapshot = self.snapshot_layout();

                self.remove_drag_window_from_source(hwnd, source_monitor, source_ws_idx);

                let Some(target_is_tabbed_final) = self.insert_drag_placeholder_at_target(
                    viewport,
                    cursor_x,
                    target_col,
                    window_slot,
                    target_monitor_id,
                ) else {
                    return;
                };
                // Skip the live-preview transition for Tabbed targets.
                // Tabbed columns hide non-active tabs off-screen, so the
                // placeholder doesn't introduce a visible "gap" to
                // animate into — running the transition just shifts
                // every column laterally for ~150ms with no informational
                // payoff, which reads as the strip-and-column "sliding
                // weirdly" the user reported. Vertical targets still
                // animate so the user sees where the dragged window
                // will land.
                if !target_is_tabbed_final {
                    self.start_layout_transition(snapshot);
                } else {
                    let _ = snapshot;
                }
                if let Err(e) = self.apply_layout() {
                    warn!("Failed to apply layout during live drag preview: {}", e);
                }
            }

            // Show ghost at the target slot position (recompute from updated layout).
            self.show_merge_drop_ghost(hwnd, is_same_column, target_monitor_id, viewport);
        }
    }

    /// Show a standalone-window drop as a vertical insertion edge. This path
    /// never mutates workspace structure during preview, so cancellation is a
    /// no-op and drops cannot merge even while directly over another column.
    fn standalone_window_drag_hint(&mut self, cursor_x: i32, target_monitor_id: MonitorId) {
        if !self.monitors.contains_key(&target_monitor_id) {
            return;
        }
        self.remove_previous_drag_placeholder();
        let viewport = self.layout_viewport(target_monitor_id);
        let ws_idx = self.active_workspace_idx(target_monitor_id);
        let Some(workspace) = self
            .workspaces
            .get(&target_monitor_id)
            .and_then(|workspaces| workspaces.get(ws_idx))
        else {
            return;
        };
        let column_bounds = column_bounds_from_placements(workspace, viewport);
        let insert_index = compute_insertion_index(&column_bounds, cursor_x);
        let drop_target = DropTarget {
            monitor: target_monitor_id,
            insert_index,
            window_slot: None,
        };
        if let Some(ref mut drag) = self.drag_state {
            if drag.last_drop_target == Some(drop_target) {
                return;
            }
            drag.last_drop_target = Some(drop_target);
        }
        let gap = workspace.gap();
        let outer_left = workspace.outer_gaps().0.max(0);
        let hint_x = if column_bounds.is_empty() {
            viewport.x.saturating_add(outer_left)
        } else {
            compute_insertion_hint_x(&column_bounds, insert_index, gap)
        };
        self.pending_drag_hint = Some(DragHintAction::ShowGhost {
            rect: Rect::new(hint_x - 2, viewport.y, 4, viewport.height),
        });
    }

    /// Show the column-reorder ghost at the insertion edge on a non-source monitor.
    fn shift_drag_cross_monitor_hint(&mut self, cursor_x: i32, target_monitor_id: MonitorId) {
        // Show ghost at the edge of the target monitor.
        if !self.monitors.contains_key(&target_monitor_id) {
            return;
        }
        let viewport = self.layout_viewport(target_monitor_id);
        let ws_idx = self.active_workspace_idx(target_monitor_id);
        let Some(workspace) = self
            .workspaces
            .get(&target_monitor_id)
            .and_then(|v| v.get(ws_idx))
        else {
            return;
        };
        let column_bounds = column_bounds_from_placements(workspace, viewport);
        let insert_index = compute_insertion_index(&column_bounds, cursor_x);
        let drop_target = DropTarget {
            monitor: target_monitor_id,
            insert_index,
            window_slot: None,
        };
        if let Some(ref mut drag) = self.drag_state {
            if drag.last_drop_target == Some(drop_target) {
                return;
            }
            drag.last_drop_target = Some(drop_target);
        }
        let gap = workspace.gap();
        let outer_left = workspace.outer_gaps().0.max(0);
        let hint_x = if column_bounds.is_empty() {
            viewport.x.saturating_add(outer_left)
        } else {
            compute_insertion_hint_x(&column_bounds, insert_index, gap)
        };
        self.pending_drag_hint = Some(DragHintAction::ShowGhost {
            rect: Rect::new(hint_x - 2, viewport.y, 4, viewport.height),
        });
    }

    /// Live-reorder the dragged column on the source monitor and show its ghost.
    fn shift_drag_reorder_hint(
        &mut self,
        cursor_x: i32,
        source_monitor: MonitorId,
        source_ws_idx: usize,
        current_col: usize,
    ) {
        if !self.monitors.contains_key(&source_monitor) {
            return;
        }
        let viewport = self.layout_viewport(source_monitor);
        let Some(workspace) = self
            .workspaces
            .get(&source_monitor)
            .and_then(|v| v.get(source_ws_idx))
        else {
            return;
        };

        let column_bounds = column_bounds_from_placements(workspace, viewport);
        // Trigger reorder when cursor enters another column's area.
        let target_idx = match compute_target_column_index(&column_bounds, cursor_x) {
            Some(idx) => idx,
            None => {
                // Cursor is in the gap between columns — keep current position.
                // Still show the ghost at the current column.
                if let Some(rect) = compute_column_rect(workspace, viewport, current_col) {
                    self.pending_drag_hint = Some(DragHintAction::ShowGhost { rect });
                }
                return;
            }
        };

        if target_idx != current_col {
            debug!(
                "Live drag reorder: column {} → {} on monitor {}",
                current_col, target_idx, source_monitor
            );
            let snapshot = self.snapshot_layout();
            if let Some(workspace) = self
                .workspaces
                .get_mut(&source_monitor)
                .and_then(|v| v.get_mut(source_ws_idx))
            {
                workspace.reorder_column(current_col, target_idx);
            }
            if let Some(ref mut drag) = self.drag_state {
                drag.current_column_index = target_idx;
            }
            self.start_layout_transition(snapshot);
            if let Err(e) = self.apply_layout() {
                warn!("Failed to apply layout during live drag reorder: {}", e);
            }
        }

        // Show ghost at the dragged column's new position.
        let workspace = match self
            .workspaces
            .get(&source_monitor)
            .and_then(|v| v.get(source_ws_idx))
        {
            Some(ws) => ws,
            None => return,
        };
        let new_col_idx = match self.drag_state {
            Some(ref d) => d.current_column_index,
            None => return,
        };
        if let Some(rect) = compute_column_rect(workspace, viewport, new_col_idx) {
            self.pending_drag_hint = Some(DragHintAction::ShowGhost { rect });
        }
    }

    /// Reorder the dragged window to the cursor's slot within its own column.
    fn merge_reorder_same_column(
        &mut self,
        hwnd: u64,
        target_monitor_id: MonitorId,
        target_col: usize,
        window_slot: usize,
    ) {
        let ws_idx = self.active_workspace_idx(target_monitor_id);
        let current_location = self
            .workspaces
            .get(&target_monitor_id)
            .and_then(|v| v.get(ws_idx))
            .and_then(|ws| ws.find_window_location(hwnd));
        let needs_move = match current_location {
            Some((_, cur_win_idx)) => cur_win_idx != window_slot,
            None => false,
        };
        if needs_move {
            let snapshot = self.snapshot_layout();
            let idx = self.active_workspace_idx(target_monitor_id);
            if let Some(ws) = self
                .workspaces
                .get_mut(&target_monitor_id)
                .and_then(|v| v.get_mut(idx))
            {
                let _ = ws.remove_window(hwnd);
                let _ = ws.insert_window_in_column_at(hwnd, target_col, window_slot);
            }
            self.start_layout_transition(snapshot);
            if let Err(e) = self.apply_layout() {
                warn!("Failed to apply layout during live drag reorder: {}", e);
            }
        }
    }

    /// Remove the dragged window from a multi-window source column (once per drag).
    fn remove_drag_window_from_source(
        &mut self,
        hwnd: u64,
        source_monitor: MonitorId,
        source_ws_idx: usize,
    ) {
        let already_removed = self
            .drag_state
            .as_ref()
            .is_some_and(|d| d.removed_from_source);
        if !already_removed {
            // Remove window from multi-window source columns so remaining
            // windows expand. Single-window columns keep the window to
            // preserve column space until drop.
            let should_remove = self
                .workspaces
                .get(&source_monitor)
                .and_then(|v| v.get(source_ws_idx))
                .and_then(|ws| {
                    let (col, _) = ws.find_window_location(hwnd)?;
                    Some(ws.column(col)?.len() > 1)
                })
                .unwrap_or(false);
            if should_remove {
                if let Some(ws) = self
                    .workspaces
                    .get_mut(&source_monitor)
                    .and_then(|v| v.get_mut(source_ws_idx))
                {
                    let _ = ws.remove_window(hwnd);
                }
                if let Some(ref mut drag) = self.drag_state {
                    drag.removed_from_source = true;
                }
            }
        }
    }

    /// Insert the drag placeholder at the target column; returns whether the target is tabbed, or `None` to abort the hint.
    fn insert_drag_placeholder_at_target(
        &mut self,
        viewport: Rect,
        cursor_x: i32,
        target_col: usize,
        window_slot: usize,
        target_monitor_id: MonitorId,
    ) -> Option<bool> {
        // Insert placeholder at target to shift target windows.
        // Recompute target_col since removing from source may have shifted indices.
        let adj_target_col = if self
            .drag_state
            .as_ref()
            .is_some_and(|d| d.removed_from_source)
        {
            // Re-derive from updated layout.
            let tgt_idx = self.active_workspace_idx(target_monitor_id);
            let ws = self
                .workspaces
                .get(&target_monitor_id)
                .and_then(|v| v.get(tgt_idx))?;
            let bounds = column_bounds_from_placements(ws, viewport);
            compute_target_column_index(&bounds, cursor_x)?
        } else {
            target_col
        };

        let tgt_idx = self.active_workspace_idx(target_monitor_id);
        let target_is_tabbed_final = if let Some(ws) = self
            .workspaces
            .get_mut(&target_monitor_id)
            .and_then(|v| v.get_mut(tgt_idx))
        {
            let it = adj_target_col < ws.column_count()
                && ws.column(adj_target_col).is_some_and(|c| c.is_tabbed());
            if adj_target_col < ws.column_count() {
                let _ = ws.insert_window_in_column_at(
                    DRAG_PLACEHOLDER_HWND,
                    adj_target_col,
                    window_slot,
                );
            } else {
                let _ = ws.insert_window(DRAG_PLACEHOLDER_HWND, None);
            }
            it
        } else {
            false
        };
        Some(target_is_tabbed_final)
    }

    /// Show the drop ghost at the dragged window's (or placeholder's) slot in the target column.
    fn show_merge_drop_ghost(
        &mut self,
        hwnd: u64,
        is_same_column: bool,
        target_monitor_id: MonitorId,
        viewport: Rect,
    ) {
        let ws_idx = self.active_workspace_idx(target_monitor_id);
        let workspace = match self
            .workspaces
            .get(&target_monitor_id)
            .and_then(|v| v.get(ws_idx))
        {
            Some(ws) => ws,
            None => return,
        };

        // For cross-column, ghost at the placeholder position; for same-column, at the window.
        let ghost_id = if is_same_column {
            hwnd
        } else {
            DRAG_PLACEHOLDER_HWND
        };
        if let Some((ghost_col, ghost_slot)) = workspace.find_window_location(ghost_id) {
            if let Some(ghost_col_rect) = compute_column_rect(workspace, viewport, ghost_col) {
                let ghost_col_is_tabbed =
                    workspace.column(ghost_col).is_some_and(|c| c.is_tabbed());
                let ghost = if ghost_col_is_tabbed {
                    // Tabbed targets drop-as-tab (Chrome semantics —
                    // always append rightmost), so the ghost should
                    // communicate "this whole column is the target,"
                    // not "this specific Y-slot." A slot-sized ghost
                    // reads as vertical-stack semantics and misleads
                    // the user about where the window will actually
                    // land.
                    ghost_col_rect
                } else {
                    // Count only visible (non-minimized) windows and
                    // convert the raw slot index to a visible-window
                    // index so the ghost position is correct when
                    // minimized windows precede it.
                    let (ghost_n, visible_slot) = workspace
                        .column(ghost_col)
                        .map(|c| {
                            let mut vis_count = 0usize;
                            let mut vis_slot = 0usize;
                            for (i, w) in c.windows().iter().enumerate() {
                                if !workspace.is_minimized(*w) {
                                    if i < ghost_slot {
                                        vis_slot += 1;
                                    }
                                    vis_count += 1;
                                }
                            }
                            (vis_count, vis_slot)
                        })
                        .unwrap_or((1, 0));
                    let gap = workspace.gap();
                    let total_gaps = (ghost_n as i32 - 1) * gap;
                    let usable_height = ghost_col_rect.height - total_gaps;
                    let slot_height = usable_height / ghost_n as i32;
                    let slot_y = ghost_col_rect.y + visible_slot as i32 * (slot_height + gap);
                    Rect::new(ghost_col_rect.x, slot_y, ghost_col_rect.width, slot_height)
                };
                self.pending_drag_hint = Some(DragHintAction::ShowGhost { rect: ghost });
            }
        }
    }

    /// Execute window merge: extract the dragged window from its column and
    /// insert it at the target slot in the target column.
    pub(crate) fn execute_window_merge(
        &mut self,
        hwnd: u64,
        drag: &DragState,
        target_monitor: MonitorId,
        win_rect: &Rect,
    ) {
        let source_monitor = drag.source_monitor;

        // Find target column/slot from the cursor. On another monitor, empty
        // space is a valid standalone-column destination rather than a cancel.
        if !self.monitors.contains_key(&target_monitor) {
            self.cancel_tiled_drag(hwnd, drag);
            return;
        }
        let target_viewport = self.layout_viewport(target_monitor);

        let destination = {
            let ws_idx = self.active_workspace_idx(target_monitor);
            let Some(workspace) = self
                .workspaces
                .get(&target_monitor)
                .and_then(|v| v.get(ws_idx))
            else {
                self.cancel_tiled_drag(hwnd, drag);
                return;
            };
            let column_bounds = column_bounds_from_placements(workspace, target_viewport);
            let (cx, cy) = leopardwm_platform_win32::get_cursor_pos().unwrap_or_else(|| {
                (
                    win_rect.x + win_rect.width / 2,
                    win_rect.y + win_rect.height / 2,
                )
            });

            // If the drop lands on a visible tab strip, route to that strip's
            // owning column. Otherwise merge only when directly over a column.
            let strip_hit = self
                .tab_strip_overlays
                .values()
                .find_map(|s| s.hit_test_screen(cx, cy));
            if let Some(hit) = strip_hit {
                WindowDropDestination::ExistingColumn {
                    column: hit.column_idx,
                    slot: hit.tab_idx,
                }
            } else if let Some(col_idx) = compute_target_column_index(&column_bounds, cx) {
                let is_same_col = workspace.column(col_idx).is_some_and(|c| c.contains(hwnd));
                let n_existing = workspace.column(col_idx).map(|c| c.len()).unwrap_or(0);
                let n_total = if is_same_col {
                    n_existing
                } else {
                    n_existing + 1
                };
                let col_rect = compute_column_rect(workspace, target_viewport, col_idx);
                let slot = match col_rect {
                    Some(ref rect) if n_total > 0 => compute_window_slot(rect, n_total, cy),
                    _ => 0,
                };
                WindowDropDestination::ExistingColumn {
                    column: col_idx,
                    slot,
                }
            } else if target_monitor != source_monitor {
                WindowDropDestination::NewColumn {
                    index: compute_insertion_index(&column_bounds, cx),
                }
            } else {
                self.cancel_tiled_drag(hwnd, drag);
                return;
            }
        };

        let (target_col_idx, window_slot) = match destination {
            WindowDropDestination::ExistingColumn { column, slot } => (column, slot),
            WindowDropDestination::NewColumn { index } => {
                self.execute_cross_monitor_window_drop(hwnd, drag, target_monitor, index);
                return;
            }
        };

        // Check if window was already removed from source during live drag preview.
        let already_removed = drag.removed_from_source;

        // Find source column info before removal.
        let src_ws_idx = drag.source_workspace_idx;
        let src_col_info = if !already_removed && target_monitor == source_monitor {
            self.workspaces
                .get(&source_monitor)
                .and_then(|v| v.get(src_ws_idx))
                .and_then(|ws| ws.find_window_location(hwnd))
                .map(|(col_idx, _)| {
                    let col_len = self
                        .workspaces
                        .get(&source_monitor)
                        .and_then(|v| v.get(src_ws_idx))
                        .and_then(|ws| ws.column(col_idx))
                        .map(|c| c.len())
                        .unwrap_or(0);
                    (col_idx, col_len)
                })
        } else {
            None
        };

        // Single-window column dropped onto itself → snap back, nothing to merge.
        if let Some((src_col, src_len)) = src_col_info {
            if target_col_idx == src_col && src_len == 1 {
                self.cancel_tiled_drag(hwnd, drag);
                return;
            }
        }

        // Verify target workspace exists before mutating source.
        let tgt_check_idx = self.active_workspace_idx(target_monitor);
        if self
            .workspaces
            .get(&target_monitor)
            .and_then(|v| v.get(tgt_check_idx))
            .is_none()
        {
            self.cancel_tiled_drag(hwnd, drag);
            return;
        }

        // Snapshot AFTER all early returns, right before structural changes.
        let snapshot = self.snapshot_layout();

        // Remove the window from its source column (skip if already removed during drag).
        if !already_removed {
            if let Some(workspace) = self
                .workspaces
                .get_mut(&source_monitor)
                .and_then(|v| v.get_mut(src_ws_idx))
            {
                if let Err(e) = workspace.remove_window(hwnd) {
                    warn!("Failed to remove window {} for merge: {}", hwnd, e);
                    self.cancel_tiled_drag(hwnd, drag);
                    return;
                }
            }
        }

        // Adjust target column index if removing from the same monitor shifted indices.
        // If the source column was before the target and was fully removed (single window),
        // all subsequent columns shifted left by 1.
        let effective_target_col = if let Some((src_col, src_len)) = src_col_info {
            if src_len == 1 && src_col < target_col_idx {
                target_col_idx - 1
            } else {
                target_col_idx
            }
        } else {
            target_col_idx
        };

        // Add window to the target column at the computed slot.
        let tgt_mut_idx = self.active_workspace_idx(target_monitor);
        if let Some(workspace) = self
            .workspaces
            .get_mut(&target_monitor)
            .and_then(|v| v.get_mut(tgt_mut_idx))
        {
            if workspace.column_count() == 0 {
                let _ = workspace.insert_window(hwnd, None);
            } else {
                // Chrome semantics: drops into a Tabbed column always
                // append at the rightmost position, even when the drop
                // lands on a specific tab in the strip. Override the
                // cursor-derived slot here so the strip-hit branch above
                // (which returns `hit.tab_idx`) doesn't place new tabs
                // at the leftmost slot.
                let target_is_tabbed = workspace
                    .column(effective_target_col)
                    .is_some_and(|c| c.is_tabbed());
                let effective_slot = if target_is_tabbed {
                    workspace
                        .column(effective_target_col)
                        .map(|c| c.len())
                        .unwrap_or(window_slot)
                } else {
                    window_slot
                };
                if let Err(e) =
                    workspace.insert_window_in_column_at(hwnd, effective_target_col, effective_slot)
                {
                    warn!(
                        "Failed to merge window {} into column {} at slot {}: {}",
                        hwnd, effective_target_col, effective_slot, e
                    );
                    let _ = workspace.insert_window(hwnd, None);
                }
            }
            // Drop activates the inserted window in both Vertical and
            // Tabbed targets — matches the Chrome-tab mental model
            // ("I just dropped this here, of course I want to see it").
            // `focus_window` follows the hwnd and
            // `sync_active_tab_to_focus` promotes it to the active tab
            // in Tabbed columns.
            if let Err(e) = workspace.focus_window(hwnd) {
                debug!("Failed to focus merged window {}: {}", hwnd, e);
            }
            workspace.ensure_focused_visible_animated(target_viewport.width);
        }

        if target_monitor != source_monitor {
            let source_viewport_width = self.viewport_width_for(source_monitor);
            if let Some(source_ws) = self
                .workspaces
                .get_mut(&source_monitor)
                .and_then(|v| v.get_mut(src_ws_idx))
            {
                source_ws.ensure_focused_visible_animated(source_viewport_width);
            }
            // The window may acquire different invisible-frame metrics after
            // WM_DPICHANGED on the destination monitor. Invalidate both the
            // global and animation-worker caches before the transition.
            leopardwm_platform_win32::clear_inset_cache();
        }

        self.focused_monitor = target_monitor;
        info!(
            "Window merge: {} into column {} slot {} on monitor {}",
            hwnd, effective_target_col, window_slot, target_monitor
        );

        self.start_layout_transition(snapshot);
        if let Err(e) = self.apply_layout() {
            warn!("Failed to apply layout after window merge: {}", e);
        }
        self.sync_foreground_window();
    }

    /// Finalize a default drag-drop by swapping the placeholder with the real window.
    /// Avoids a redundant transition since windows are already at their final positions
    /// from the live preview during drag.
    ///
    /// Known visual limitation: when a single-window source column gets
    /// emptied by the drop, every column to its right shifts left to fill
    /// the gap — including a Tabbed destination column, which reads as the
    /// tabbed column "sliding" into its new x. There's no transition to
    /// suppress; the column simply arrives at a different `x` because there's
    /// one fewer column before it. Consistent with Vertical→Vertical resize,
    /// just more noticeable when the destination strip moves with the column.
    pub(crate) fn finalize_drag_merge(
        &mut self,
        hwnd: u64,
        drag: &DragState,
        target_monitor: MonitorId,
        win_rect: &Rect,
    ) {
        use crate::state::DRAG_PLACEHOLDER_HWND;
        let source_monitor = drag.source_monitor;

        // Find where the placeholder is (this is where the real window should go).
        let tgt_idx = self.active_workspace_idx(target_monitor);
        let placeholder_location = self
            .workspaces
            .get(&target_monitor)
            .and_then(|v| v.get(tgt_idx))
            .and_then(|ws| ws.find_window_location(DRAG_PLACEHOLDER_HWND));

        if let Some((ph_col, ph_slot)) = placeholder_location {
            // Capture source column info BEFORE any removals.
            let src_ws_idx = drag.source_workspace_idx;
            let src_info = if !drag.removed_from_source && target_monitor == source_monitor {
                self.workspaces
                    .get(&source_monitor)
                    .and_then(|v| v.get(src_ws_idx))
                    .and_then(|ws| ws.find_window_location(hwnd))
                    .map(|(col, _)| {
                        let len = self
                            .workspaces
                            .get(&source_monitor)
                            .and_then(|v| v.get(src_ws_idx))
                            .and_then(|ws| ws.column(col))
                            .map(|c| c.len())
                            .unwrap_or(0);
                        (col, len)
                    })
            } else {
                None
            };

            // Remove placeholder.
            if let Some(ws) = self
                .workspaces
                .get_mut(&target_monitor)
                .and_then(|v| v.get_mut(tgt_idx))
            {
                let _ = ws.remove_window(DRAG_PLACEHOLDER_HWND);
            }

            // Remove real window from source (if not already removed during drag).
            if !drag.removed_from_source {
                if let Some(ws) = self
                    .workspaces
                    .get_mut(&source_monitor)
                    .and_then(|v| v.get_mut(src_ws_idx))
                {
                    let _ = ws.remove_window(hwnd);
                }
            }

            // Adjust column index if source column was removed and was before placeholder.
            let adj_col = if let Some((src_col, src_len)) = src_info {
                if src_len == 1 && src_col < ph_col {
                    ph_col - 1
                } else {
                    ph_col
                }
            } else {
                ph_col
            };

            // Insert real window at the placeholder's former position.
            if let Some(ws) = self
                .workspaces
                .get_mut(&target_monitor)
                .and_then(|v| v.get_mut(tgt_idx))
            {
                if ws.column_count() == 0 {
                    let _ = ws.insert_window(hwnd, None);
                } else {
                    // Chrome semantics: Tabbed drops always append to
                    // the rightmost position. The placeholder slot
                    // reflects where live-preview anchored it, which can
                    // be wrong if the cursor wiggled across columns
                    // during the drag — force the end here so the user
                    // sees the new tab where they expect it.
                    let target_is_tabbed = ws.column(adj_col).is_some_and(|c| c.is_tabbed());
                    let effective_slot = if target_is_tabbed {
                        ws.column(adj_col).map(|c| c.len()).unwrap_or(ph_slot)
                    } else {
                        ph_slot
                    };
                    if let Err(e) = ws.insert_window_in_column_at(hwnd, adj_col, effective_slot) {
                        warn!(
                            "Failed to place window {} at col {} slot {}: {}",
                            hwnd, adj_col, effective_slot, e
                        );
                        let _ = ws.insert_window(hwnd, None);
                    }
                }
                // Drop activates the inserted window for both Vertical
                // and Tabbed targets — Chrome-tab semantics. Mirrors
                // `execute_window_merge`.
                if let Err(e) = ws.focus_window(hwnd) {
                    debug!("Failed to focus merged window {}: {}", hwnd, e);
                }
                let vw = self
                    .monitors
                    .get(&target_monitor)
                    .map(|m| m.work_area.width)
                    .unwrap_or(FALLBACK_VIEWPORT_WIDTH);
                ws.ensure_focused_visible_animated(vw);
            }

            if target_monitor != source_monitor {
                let source_viewport_width = self.viewport_width_for(source_monitor);
                if let Some(source_ws) = self
                    .workspaces
                    .get_mut(&source_monitor)
                    .and_then(|v| v.get_mut(src_ws_idx))
                {
                    source_ws.ensure_focused_visible_animated(source_viewport_width);
                }
                leopardwm_platform_win32::clear_inset_cache();
            }
            self.focused_monitor = target_monitor;

            // Never discard transition-owned exit HWNDs at an intermediate
            // position. A dead exit is already safe; a live parking failure keeps
            // ownership and defers this physical landing.
            if let Err(error) = self.cancel_layout_transition_for_exact_landing() {
                warn!("Drag merge transition cancellation failed: {error}");
                return;
            }
            self.abort_active_ghost_transition();
            // Evict the dragged hwnd from last_placed_layout_rects: when the
            // user drops it back in its original column, the layout is
            // unchanged and apply_layout's fast-path would skip repositioning,
            // leaving the window where the user dropped it instead of
            // snapping it back to its layout slot.
            self.last_placed_layout_rects.remove(&hwnd);
            if let Err(e) = self.apply_layout() {
                warn!("Failed to apply layout after drag merge: {}", e);
            }
            self.sync_foreground_window();
        } else {
            // No placeholder found — fall back to full merge (cross-monitor or edge case).
            self.clear_drag_placeholder();
            self.execute_window_merge(hwnd, drag, target_monitor, win_rect);
        }
    }

    /// Finish a no-merge window drag at the pointer's horizontal insertion
    /// edge. Same-monitor drops split/reorder one standalone column;
    /// cross-monitor drops use the existing transactional transfer.
    pub(crate) fn finish_standalone_window_drag(
        &mut self,
        hwnd: u64,
        drag: &DragState,
        target_monitor: MonitorId,
        cursor_x: i32,
        win_rect: &Rect,
    ) {
        self.clear_drag_placeholder();
        if !self.monitors.contains_key(&target_monitor) {
            self.cancel_tiled_drag(hwnd, drag);
            return;
        }
        let target_idx = self.active_workspace_idx(target_monitor);
        let target_viewport = self.layout_viewport(target_monitor);
        let Some(target_workspace) = self
            .workspaces
            .get(&target_monitor)
            .and_then(|workspaces| workspaces.get(target_idx))
        else {
            self.cancel_tiled_drag(hwnd, drag);
            return;
        };
        let bounds = column_bounds_from_placements(target_workspace, target_viewport);
        let pointer_x = if bounds.is_empty() {
            win_rect.x.saturating_add(win_rect.width / 2)
        } else {
            cursor_x
        };
        let insert_index = compute_insertion_index(&bounds, pointer_x);

        if target_monitor == drag.source_monitor {
            self.execute_same_monitor_standalone_window_drop(hwnd, drag, insert_index);
        } else {
            self.execute_cross_monitor_window_drop(hwnd, drag, target_monitor, insert_index);
        }
    }

    /// Move one window into its own column on the source monitor. The mutation
    /// is prepared on a clone and committed only after remove+insert+focus all
    /// succeed, so a bad insertion cannot orphan the HWND.
    pub(crate) fn execute_same_monitor_standalone_window_drop(
        &mut self,
        hwnd: u64,
        drag: &DragState,
        insert_index: usize,
    ) {
        self.clear_drag_placeholder();
        let monitor = drag.source_monitor;
        let workspace_idx = drag.source_workspace_idx;
        if self.active_workspace_idx(monitor) != workspace_idx || drag.removed_from_source {
            self.cancel_tiled_drag(hwnd, drag);
            return;
        }
        let viewport_width = self.viewport_width_for(monitor);
        let Some(mut workspace) = self
            .workspaces
            .get(&monitor)
            .and_then(|workspaces| workspaces.get(workspace_idx))
            .cloned()
        else {
            self.cancel_tiled_drag(hwnd, drag);
            return;
        };
        let Some((source_column, _)) = workspace.find_window_location(hwnd) else {
            self.cancel_tiled_drag(hwnd, drag);
            return;
        };
        let source_column_len = workspace
            .column(source_column)
            .map(|column| column.len())
            .unwrap_or(0);
        if source_column_len == 0 || workspace.remove_window(hwnd).is_err() {
            self.cancel_tiled_drag(hwnd, drag);
            return;
        }

        // Removing a single-window source column shifts every later insertion
        // edge left. Multi-window sources remain in place and need no adjustment.
        let adjusted_index = if source_column_len == 1 && source_column < insert_index {
            insert_index - 1
        } else {
            insert_index
        };
        let width = (drag.source_column_width > 0).then_some(drag.source_column_width);
        if let Err(error) = workspace.insert_window_at_column(hwnd, width, adjusted_index) {
            warn!(
                "Failed to insert standalone drag window {} at column {}: {}",
                hwnd, adjusted_index, error
            );
            self.cancel_tiled_drag(hwnd, drag);
            return;
        }
        workspace.ensure_focused_visible_animated(viewport_width);

        let snapshot = self.snapshot_layout();
        self.workspaces.get_mut(&monitor).unwrap()[workspace_idx] = workspace;
        self.focused_monitor = monitor;
        self.last_placed_layout_rects.remove(&hwnd);
        info!(
            "Standalone window drag: {} moved to column {} on monitor {}",
            hwnd, adjusted_index, monitor
        );
        if self.focused_window_uses_sticky_compositor() {
            if let Some(workspace) = self
                .workspaces
                .get_mut(&monitor)
                .and_then(|workspaces| workspaces.get_mut(workspace_idx))
            {
                workspace.stop_animation();
            }
        } else {
            self.start_layout_transition(snapshot);
        }
        if let Err(error) = self.apply_layout() {
            warn!("Failed to apply standalone window drag: {}", error);
        }
        self.sync_foreground_window();
    }

    /// Complete a one-window drag into empty/unused space on another monitor
    /// by creating a standalone column. Shift-drag retains its existing
    /// whole-column behavior.
    pub(crate) fn execute_cross_monitor_window_drop(
        &mut self,
        hwnd: u64,
        drag: &DragState,
        target_monitor: MonitorId,
        insert_index: usize,
    ) {
        if target_monitor == drag.source_monitor {
            self.cancel_tiled_drag(hwnd, drag);
            return;
        }

        self.clear_drag_placeholder();
        let source_monitor = drag.source_monitor;
        let source_idx = drag.source_workspace_idx;
        let target_idx = self.active_workspace_idx(target_monitor);
        let target_viewport = self.layout_viewport(target_monitor);
        let source_viewport_width = self.viewport_width_for(source_monitor);

        let Some(mut source_workspace) = self
            .workspaces
            .get(&source_monitor)
            .and_then(|v| v.get(source_idx))
            .cloned()
        else {
            self.cancel_tiled_drag(hwnd, drag);
            return;
        };
        let Some(mut target_workspace) = self
            .workspaces
            .get(&target_monitor)
            .and_then(|v| v.get(target_idx))
            .cloned()
        else {
            self.cancel_tiled_drag(hwnd, drag);
            return;
        };

        // The live preview may already have detached the window from a
        // multi-window source column. Otherwise remove it on the snapshot;
        // failures leave the real state untouched.
        if source_workspace.contains_window(hwnd) {
            if let Err(error) = source_workspace.remove_window(hwnd) {
                warn!(
                    "Failed to detach window {} for cross-monitor drop: {}",
                    hwnd, error
                );
                self.cancel_tiled_drag(hwnd, drag);
                return;
            }
        } else if !drag.removed_from_source {
            self.cancel_tiled_drag(hwnd, drag);
            return;
        }

        let (outer_left, outer_right, _, _) = target_workspace.outer_gaps();
        let fitting_width = target_viewport
            .width
            .saturating_sub(outer_left.max(0))
            .saturating_sub(outer_right.max(0))
            .max(100);
        let carried_width = if drag.source_column_width > 0 {
            self.scale_px_between_monitors(drag.source_column_width, source_monitor, target_monitor)
        } else {
            target_workspace.default_column_width()
        };
        let target_width = carried_width.min(fitting_width);
        if let Err(error) =
            target_workspace.insert_window_at_column(hwnd, Some(target_width), insert_index)
        {
            warn!(
                "Failed to insert window {} on target monitor {}: {}",
                hwnd, target_monitor, error
            );
            self.cancel_tiled_drag(hwnd, drag);
            return;
        }
        source_workspace.ensure_focused_visible_animated(source_viewport_width);
        target_workspace.ensure_focused_visible_animated(target_viewport.width);

        let snapshot = self.snapshot_layout();
        self.workspaces.get_mut(&source_monitor).unwrap()[source_idx] = source_workspace;
        self.workspaces.get_mut(&target_monitor).unwrap()[target_idx] = target_workspace;
        self.focused_monitor = target_monitor;
        self.last_placed_layout_rects.remove(&hwnd);
        leopardwm_platform_win32::clear_inset_cache();

        info!(
            "Cross-monitor window drop: {} from monitor {} to monitor {} as column {}",
            hwnd, source_monitor, target_monitor, insert_index
        );
        if self.focused_window_uses_sticky_compositor() {
            if let Some(target_workspace) = self
                .workspaces
                .get_mut(&target_monitor)
                .and_then(|workspaces| workspaces.get_mut(target_idx))
            {
                target_workspace.stop_animation();
            }
        } else {
            self.start_layout_transition(snapshot);
        }
        if let Err(error) = self.apply_layout() {
            warn!("Failed to apply cross-monitor window drop: {}", error);
        }
        self.sync_foreground_window();
    }

    /// Restore a window detached by live merge preview to its exact original
    /// column/slot. Returns true when no restore was needed or it succeeded.
    pub(crate) fn restore_detached_drag_window(&mut self, hwnd: u64, drag: &DragState) -> bool {
        if !drag.removed_from_source {
            return true;
        }
        self.workspaces
            .get_mut(&drag.source_monitor)
            .and_then(|v| v.get_mut(drag.source_workspace_idx))
            .is_some_and(|workspace| {
                if workspace.contains_window(hwnd) {
                    return true;
                }
                let sibling_column = drag.source_sibling.and_then(|sibling| {
                    workspace
                        .find_window_location(sibling)
                        .map(|(column, _)| column)
                });
                let result = if let Some(column) = sibling_column {
                    workspace.insert_window_in_column_at(hwnd, column, drag.source_window_index)
                } else {
                    workspace.insert_window_at_column(
                        hwnd,
                        Some(drag.source_column_width),
                        drag.source_column_index,
                    )
                };
                result.is_ok() && workspace.focus_window(hwnd).is_ok()
            })
    }

    /// Restore all source-side preview mutations. Plain mode may need the
    /// detached HWND reinserted; column mode may need a live reorder reversed.
    fn restore_drag_source_state(
        &mut self,
        hwnd: u64,
        drag: &DragState,
        restore_window: bool,
    ) -> bool {
        let window_restored = !restore_window || self.restore_detached_drag_window(hwnd, drag);
        let order_restored = if drag.mode == DragMode::Column {
            self.workspaces
                .get_mut(&drag.source_monitor)
                .and_then(|workspaces| workspaces.get_mut(drag.source_workspace_idx))
                .is_some_and(|workspace| {
                    let location = workspace
                        .find_window_location(hwnd)
                        .or_else(|| {
                            drag.source_sibling
                                .and_then(|sibling| workspace.find_window_location(sibling))
                        })
                        .map(|(column, _)| column);
                    if let Some(column) = location {
                        let original = drag
                            .source_column_index
                            .min(workspace.column_count().saturating_sub(1));
                        workspace.reorder_column(column, original);
                        true
                    } else {
                        false
                    }
                })
        } else {
            true
        };
        window_restored && order_restored
    }

    /// Abort the active drag without applying layout. Callers choose whether a
    /// still-live detached HWND should be restored (real destruction passes
    /// false), then decide when to reapply/re-enable DWM transitions.
    pub(crate) fn abort_active_drag(&mut self, restore_window: bool) -> Option<u64> {
        let drag = self.drag_state.take()?;
        self.clear_drag_placeholder();
        if drag.is_tiled && !self.restore_drag_source_state(drag.hwnd, &drag, restore_window) {
            warn!(
                "Failed to restore aborted drag {} on monitor {} workspace {}",
                drag.hwnd, drag.source_monitor, drag.source_workspace_idx
            );
        }
        self.pending_drag_hint = Some(DragHintAction::Hide);
        Some(drag.hwnd)
    }

    /// Restore a window detached by live merge preview, then snap the source
    /// workspace back. This closes a management-loss bug where moving out of a
    /// valid target and releasing over empty space could leave the HWND in no
    /// workspace at all.
    pub(crate) fn cancel_tiled_drag(&mut self, hwnd: u64, drag: &DragState) {
        self.clear_drag_placeholder();
        if !self.restore_drag_source_state(hwnd, drag, true) {
            warn!(
                "Failed to restore cancelled drag {} to monitor {} workspace {}",
                hwnd, drag.source_monitor, drag.source_workspace_idx
            );
        }
        self.snap_back_tiled(drag.source_monitor, drag.source_workspace_idx);
    }

    /// Finish a floating-window drag. Cross-monitor drops transfer workspace
    /// ownership transactionally; every drop clamps the visible frame to the
    /// destination work area so no left/right/top/bottom edge remains outside.
    pub(crate) fn finish_floating_drag(
        &mut self,
        hwnd: u64,
        drag: &DragState,
        target_monitor: MonitorId,
        visible_rect: Rect,
    ) -> bool {
        let Some(target_work_area) = self.monitors.get(&target_monitor).map(|m| m.work_area) else {
            return false;
        };
        let clamped_rect = clamp_rect_to_work_area(visible_rect, target_work_area);
        let mut transferred = false;

        if target_monitor != drag.source_monitor {
            let source_idx = drag.source_workspace_idx;
            let target_idx = self.active_workspace_idx(target_monitor);
            let Some(mut source_workspace) = self
                .workspaces
                .get(&drag.source_monitor)
                .and_then(|v| v.get(source_idx))
                .cloned()
            else {
                return false;
            };
            let Some(floating) = source_workspace
                .floating_windows()
                .iter()
                .find(|floating| floating.id == hwnd)
                .cloned()
            else {
                return false;
            };
            let Some(mut target_workspace) = self
                .workspaces
                .get(&target_monitor)
                .and_then(|v| v.get(target_idx))
                .cloned()
            else {
                return false;
            };

            if !source_workspace.remove_floating(hwnd)
                || target_workspace.add_floating(hwnd, clamped_rect).is_err()
            {
                return false;
            }
            target_workspace.set_floating_pinned(hwnd, floating.pinned);
            self.workspaces.get_mut(&drag.source_monitor).unwrap()[source_idx] = source_workspace;
            self.workspaces.get_mut(&target_monitor).unwrap()[target_idx] = target_workspace;
            self.focused_monitor = target_monitor;
            transferred = true;
            leopardwm_platform_win32::clear_inset_cache();
        }

        let updated = self.update_floating_geometry(hwnd, clamped_rect);
        if updated && (transferred || clamped_rect != visible_rect) {
            self.last_placed_layout_rects.remove(&hwnd);
            if let Err(error) = self.apply_layout() {
                warn!(
                    "Failed to clamp/re-home floating window {}: {}",
                    hwnd, error
                );
            }
        }
        updated
    }

    /// Move a column to a different monitor after cross-monitor drag-drop.
    pub(crate) fn execute_cross_monitor_drag(
        &mut self,
        hwnd: u64,
        drag: &DragState,
        target_monitor: MonitorId,
        win_rect: &Rect,
    ) {
        let source_monitor = drag.source_monitor;
        let snapshot = self.snapshot_layout();

        // Use the workspace index from drag start — it's stable even if
        // active workspace changed during drag.
        let src_idx = drag.source_workspace_idx;
        let col_idx = match self
            .workspaces
            .get(&source_monitor)
            .and_then(|v| v.get(src_idx))
            .and_then(|ws| ws.find_window_location(hwnd))
            .map(|(col, _)| col)
        {
            Some(idx) => idx,
            None => {
                self.cancel_tiled_drag(hwnd, drag);
                return;
            }
        };

        // Compute target insertion index.
        if !self.monitors.contains_key(&target_monitor) {
            self.cancel_tiled_drag(hwnd, drag);
            return;
        }
        let target_viewport = self.layout_viewport(target_monitor);

        let tgt_idx = self.active_workspace_idx(target_monitor);
        let target_bounds = self
            .workspaces
            .get(&target_monitor)
            .and_then(|v| v.get(tgt_idx))
            .map(|ws| column_bounds_from_placements(ws, target_viewport))
            .unwrap_or_default();
        let cursor_x = leopardwm_platform_win32::get_cursor_pos()
            .map(|(x, _)| x)
            .unwrap_or(win_rect.x + win_rect.width / 2);
        let insert_idx = compute_insertion_index(&target_bounds, cursor_x);

        // Build the transfer on cloned workspaces and commit only after the
        // destination accepted the complete column. A duplicate/corrupt target
        // must not lose the real source column.
        let Some(mut source_workspace) = self
            .workspaces
            .get(&source_monitor)
            .and_then(|workspaces| workspaces.get(src_idx))
            .cloned()
        else {
            self.cancel_tiled_drag(hwnd, drag);
            return;
        };
        let tgt_idx = self.active_workspace_idx(target_monitor);
        let Some(mut target_workspace) = self
            .workspaces
            .get(&target_monitor)
            .and_then(|workspaces| workspaces.get(tgt_idx))
            .cloned()
        else {
            self.cancel_tiled_drag(hwnd, drag);
            return;
        };
        let minimized_in_col: Vec<u64> = source_workspace
            .column(col_idx)
            .map(|column| {
                column
                    .windows()
                    .iter()
                    .copied()
                    .filter(|window| source_workspace.is_minimized(*window))
                    .collect()
            })
            .unwrap_or_default();
        let Some(mut column) = source_workspace.remove_column(col_idx) else {
            self.cancel_tiled_drag(hwnd, drag);
            return;
        };
        let moved_windows: Vec<u64> = column.windows().to_vec();
        let source_viewport_width = self.viewport_width_for(source_monitor);
        source_workspace.ensure_focused_visible_animated(source_viewport_width);

        // Preserve logical width across per-monitor DPI, then cap a column
        // that cannot fit in the destination's visible strip so the focused
        // frame does not arrive clipped on both horizontal edges.
        let dpi_scaled_width =
            self.scale_px_between_monitors(column.width(), source_monitor, target_monitor);
        let (outer_left, outer_right, _, _) = target_workspace.outer_gaps();
        let fitting_width = target_viewport
            .width
            .saturating_sub(outer_left.max(0))
            .saturating_sub(outer_right.max(0))
            .max(100);
        column.set_width(dpi_scaled_width.min(fitting_width));

        debug!(
            "Cross-monitor drag: column with {} window(s) from monitor {} → monitor {} at index {}",
            column.len(),
            source_monitor,
            target_monitor,
            insert_idx
        );

        let target_column_count = target_workspace.column_count();
        target_workspace.insert_column_at(column, insert_idx);
        if target_workspace.column_count() != target_column_count + 1 {
            warn!(
                "Cross-monitor column drop rejected by target monitor {}; source left unchanged",
                target_monitor
            );
            self.cancel_tiled_drag(hwnd, drag);
            return;
        }
        for window in &minimized_in_col {
            target_workspace.mark_minimized(*window);
        }
        if let Err(error) = target_workspace.focus_window(hwnd) {
            warn!(
                "Cross-monitor column drop could not focus {}: {}; source left unchanged",
                hwnd, error
            );
            self.cancel_tiled_drag(hwnd, drag);
            return;
        }
        debug_assert!(
            moved_windows
                .iter()
                .all(|window| target_workspace.contains_window(*window)),
            "accepted column must contain every transferred window"
        );
        target_workspace.ensure_focused_visible_animated(target_viewport.width);

        self.workspaces.get_mut(&source_monitor).unwrap()[src_idx] = source_workspace;
        self.workspaces.get_mut(&target_monitor).unwrap()[tgt_idx] = target_workspace;
        self.focused_monitor = target_monitor;
        self.last_placed_layout_rects.remove(&hwnd);
        leopardwm_platform_win32::clear_inset_cache();

        self.start_layout_transition(snapshot);
        if let Err(e) = self.apply_layout() {
            warn!("Failed to apply layout after cross-monitor drag: {}", e);
        }
        self.sync_foreground_window();
    }

    /// Snap a tiled window back to its layout position with animation.
    /// Uses the given workspace index instead of `active_workspace_idx` so that
    /// snap-back targets the workspace where the drag originated.
    pub(crate) fn snap_back_tiled(&mut self, monitor_id: MonitorId, ws_idx: usize) {
        let snapshot = self.snapshot_layout();
        let viewport_width = self.viewport_width_for(monitor_id);
        if let Some(workspace) = self
            .workspaces
            .get_mut(&monitor_id)
            .and_then(|v| v.get_mut(ws_idx))
        {
            workspace.ensure_focused_visible_animated(viewport_width);
        }
        self.start_layout_transition(snapshot);
        if let Err(e) = self.apply_layout() {
            warn!("Failed to snap back layout after drag: {}", e);
        }
    }
}

/// Clamp a visible window rectangle wholly inside a monitor work area. If the
/// window is larger than the work area, shrink that axis first; otherwise no
/// position can make both opposing edges visible.
pub(crate) fn clamp_rect_to_work_area(rect: Rect, work_area: Rect) -> Rect {
    rect.clamped_inside(work_area)
}

/// Resolved plain-drag destination. A cross-monitor drop over an existing
/// column merges into it; a drop into empty/unused target space creates a new
/// standalone column at the insertion edge.
enum WindowDropDestination {
    ExistingColumn { column: usize, slot: usize },
    NewColumn { index: usize },
}

/// Screen-space boundary of a single column.
struct ColumnBound {
    column_index: usize,
    screen_left: i32,
    screen_right: i32,
}

/// Derive column screen boundaries from animated placements.
///
/// Tabbed columns emit off-screen placements for inactive tabs (positioned
/// at `viewport.x - viewport.width`, size 0×0). Those must be excluded
/// from the bounds aggregation: including them would stretch the column's
/// reported `screen_left` to a huge negative number, and the "first
/// matching bound" pick in `compute_target_column_index` would then claim
/// any cursor to the left of the tabbed column as belonging to it.
fn column_bounds_from_placements(
    workspace: &leopardwm_core_layout::Workspace,
    viewport: Rect,
) -> Vec<ColumnBound> {
    let placements = workspace.compute_placements_animated(viewport);
    // Placements are emitted in column order, with all windows from a column
    // contiguous. Accumulate directly instead of building a HashMap and sorting
    // it on every ~60 Hz drag-hint tick.
    let mut bounds: Vec<ColumnBound> = Vec::with_capacity(workspace.column_count());
    for placement in &placements {
        if placement.column_index == usize::MAX
            || !matches!(
                placement.visibility,
                leopardwm_core_layout::Visibility::Visible
            )
        {
            continue;
        }
        let right = placement.rect.x.saturating_add(placement.rect.width);
        if let Some(bound) = bounds
            .last_mut()
            .filter(|bound| bound.column_index == placement.column_index)
        {
            bound.screen_left = bound.screen_left.min(placement.rect.x);
            bound.screen_right = bound.screen_right.max(right);
        } else {
            bounds.push(ColumnBound {
                column_index: placement.column_index,
                screen_left: placement.rect.x,
                screen_right: right,
            });
        }
    }
    bounds
}

/// Determine the insertion column index for a given screen X coordinate.
fn compute_insertion_index(bounds: &[ColumnBound], screen_x: i32) -> usize {
    if bounds.is_empty() {
        return 0;
    }
    for b in bounds {
        let midpoint = b.screen_left + (b.screen_right - b.screen_left) / 2;
        if screen_x < midpoint {
            return b.column_index;
        }
    }
    // Past the last column: insert at end.
    bounds.last().map(|b| b.column_index + 1).unwrap_or(0)
}

/// Compute the screen X coordinate for the vertical insertion indicator line.
fn compute_insertion_hint_x(bounds: &[ColumnBound], insert_index: usize, gap: i32) -> i32 {
    if bounds.is_empty() {
        return 0;
    }
    if insert_index == 0 {
        return bounds[0].screen_left - gap / 2;
    }
    // Find the bound right before the insertion point.
    let prev = bounds.iter().rev().find(|b| b.column_index < insert_index);
    let next = bounds.iter().find(|b| b.column_index >= insert_index);
    match (prev, next) {
        (Some(p), Some(n)) => (p.screen_right + n.screen_left) / 2,
        (Some(p), None) => p.screen_right + gap / 2,
        _ => bounds[0].screen_left - gap / 2,
    }
}

/// Find which column the cursor is directly over (not between columns).
fn compute_target_column_index(bounds: &[ColumnBound], screen_x: i32) -> Option<usize> {
    bounds
        .iter()
        .find(|b| screen_x >= b.screen_left && screen_x <= b.screen_right)
        .map(|b| b.column_index)
}

/// Compute the bounding rect of a column from its placements.
fn compute_column_rect(
    workspace: &leopardwm_core_layout::Workspace,
    viewport: Rect,
    column_index: usize,
) -> Option<Rect> {
    let placements = workspace.compute_placements_animated(viewport);
    let mut min_x = i32::MAX;
    let mut min_y = i32::MAX;
    let mut max_right = i32::MIN;
    let mut max_bottom = i32::MIN;
    let mut found = false;
    for p in &placements {
        if p.column_index == column_index && p.visibility == Visibility::Visible {
            found = true;
            min_x = min_x.min(p.rect.x);
            min_y = min_y.min(p.rect.y);
            max_right = max_right.max(p.rect.x + p.rect.width);
            max_bottom = max_bottom.max(p.rect.y + p.rect.height);
        }
    }
    if found {
        Some(Rect::new(
            min_x,
            min_y,
            max_right - min_x,
            max_bottom - min_y,
        ))
    } else {
        None
    }
}

/// Compute the vertical insertion slot by dividing the column rect into equal zones.
/// For N possible slots, the column height is split into N equal regions.
/// Returns 0..n_slots-1.
fn compute_window_slot(col_rect: &Rect, n_slots: usize, screen_y: i32) -> usize {
    if n_slots <= 1 {
        return 0;
    }
    let relative_y = (screen_y - col_rect.y).max(0);
    let zone_height = col_rect.height / n_slots as i32;
    if zone_height <= 0 {
        return 0;
    }
    let slot = (relative_y / zone_height) as usize;
    slot.min(n_slots - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use leopardwm_core_layout::Workspace;

    #[test]
    fn column_bounds_exclude_floating_sentinel_placements() {
        let mut workspace = Workspace::with_gaps(10, 0);
        workspace.insert_window(1, Some(600)).unwrap();
        workspace
            .add_floating(2, Rect::new(900, 100, 500, 400))
            .unwrap();

        let bounds = column_bounds_from_placements(&workspace, Rect::new(0, 0, 1920, 1040));

        assert_eq!(bounds.len(), 1);
        assert_eq!(bounds[0].column_index, 0);
    }
}
