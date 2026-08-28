use crate::*;

use crate::workspace::Workspace;

#[derive(Debug, Clone, Copy)]
struct FocusScrollTarget {
    offset: f64,
    allow_edge_overscroll: bool,
}

impl Workspace {
    /// Compute a strip X coordinate from the same effective widths used by
    /// placement. Fully minimized columns consume no strip width or gap.
    fn column_x(&self, column_index: usize, widths: &[i32]) -> i32 {
        let gap = self.gap.max(0);
        let mut x = 0i64;
        for (index, column) in self.columns.iter().enumerate() {
            if index == column_index {
                return i64_to_i32_saturating(x);
            }
            if !self.is_column_active(column) {
                continue;
            }
            let width = widths.get(index).copied().unwrap_or_else(|| column.width());
            x = x
                .saturating_add(i64::from(width))
                .saturating_add(i64::from(gap));
        }
        i64_to_i32_saturating(x)
    }

    fn focused_column_bounds(&self) -> Option<(i32, i32)> {
        let widths = self.effective_column_widths();
        self.columns.get(self.focused_column).map(|column| {
            let width = widths
                .get(self.focused_column)
                .copied()
                .unwrap_or_else(|| column.width());
            (self.column_x(self.focused_column, &widths), width)
        })
    }

    /// Width available to the scrolling strip after horizontal outer gaps.
    pub(crate) fn visible_width(&self, viewport_width: i32) -> i32 {
        viewport_width
            .saturating_sub(self.outer_gap_left.max(0))
            .saturating_sub(self.outer_gap_right.max(0))
            .max(0)
    }

    fn should_center(&self, column_width: i32, visible_width: i32) -> bool {
        match self.centering_mode {
            CenteringMode::Center => true,
            CenteringMode::JustInView => false,
            CenteringMode::OnOverflow => column_width > visible_width,
        }
    }

    fn centered_scroll_target(column_x: i32, column_width: i32, visible_width: i32) -> f64 {
        f64::from(column_x) + f64::from(column_width.max(0)) / 2.0
            - f64::from(visible_width.max(0)) / 2.0
    }

    /// Return a deterministic visibility target. Fitting columns anchor their
    /// right edge to the viewport's right edge (subject to ordinary strip
    /// bounds), so a 50%/75% layout reaches the same preview arrangement after
    /// focus changes or a 100% → 75% resize. Oversized columns retain the
    /// intentional nearest-valid-viewport behavior because either contained
    /// viewport edge is equally useful there.
    fn nearest_visible_scroll_target(
        current: f64,
        column_x: i32,
        column_width: i32,
        visible_width: i32,
    ) -> f64 {
        if visible_width <= 0 {
            return 0.0;
        }
        let current = sanitize_scroll_offset(current);
        let left = f64::from(column_x);
        let width = f64::from(column_width.max(0));
        let right = left + width;
        let viewport = f64::from(visible_width);

        if width <= viewport {
            // The lower interval bound is canonical and independent of the
            // previous focus direction or stale pre-resize scroll position.
            return right - viewport;
        }

        // The column cannot fit. Every offset in this interval keeps the
        // complete viewport inside it, maximizing visible focused content.
        current.clamp(left, right - viewport)
    }

    fn focus_scroll_target(
        &self,
        current: f64,
        column_x: i32,
        column_width: i32,
        visible_width: i32,
    ) -> FocusScrollTarget {
        if self.should_center(column_width, visible_width) {
            FocusScrollTarget {
                offset: Self::centered_scroll_target(column_x, column_width, visible_width),
                allow_edge_overscroll: self.center_past_edges,
            }
        } else {
            FocusScrollTarget {
                offset: Self::nearest_visible_scroll_target(
                    current,
                    column_x,
                    column_width,
                    visible_width,
                ),
                allow_edge_overscroll: false,
            }
        }
    }

    fn normal_scroll_bounds(&self, visible_width: i32) -> (f64, f64) {
        let maximum = self.total_width().saturating_sub(visible_width).max(0);
        (0.0, f64::from(maximum))
    }

    fn scroll_bounds_for_focus(
        &self,
        visible_width: i32,
        allow_edge_overscroll: bool,
    ) -> (f64, f64) {
        let normal = self.normal_scroll_bounds(visible_width);
        if !allow_edge_overscroll || !self.center_past_edges {
            return normal;
        }

        let Some((column_x, column_width)) = self.focused_column_bounds() else {
            return normal;
        };
        let centered = Self::centered_scroll_target(column_x, column_width, visible_width);
        // For a strip shorter than the viewport, a middle column can also
        // require edge overscroll to reach the center. Extend only as far as
        // this focus's exact centered target; arbitrary blank-space scrolling
        // remains impossible.
        (normal.0.min(centered), normal.1.max(centered))
    }

    /// Bounds used by render-time repair. Explicit center-column commands are
    /// preserved while their exact target still matches the current geometry.
    fn focused_scroll_bounds(&self, visible_width: i32) -> (f64, f64) {
        let Some((column_x, column_width)) = self.focused_column_bounds() else {
            return self.normal_scroll_bounds(visible_width);
        };
        let centered = Self::centered_scroll_target(column_x, column_width, visible_width);
        let explicitly_centered = (self.scroll_offset - centered).abs() < 0.5;
        self.scroll_bounds_for_focus(
            visible_width,
            self.should_center(column_width, visible_width) || explicitly_centered,
        )
    }

    /// Adjust the viewport according to the configured focus policy.
    pub fn ensure_focused_visible(&mut self, viewport_width: i32) {
        let Some((column_x, column_width)) = self.focused_column_bounds() else {
            return;
        };
        let visible_width = self.visible_width(viewport_width);
        let target =
            self.focus_scroll_target(self.scroll_offset, column_x, column_width, visible_width);
        let bounds = self.scroll_bounds_for_focus(visible_width, target.allow_edge_overscroll);
        self.scroll_offset = target.offset.clamp(bounds.0, bounds.1);
    }

    /// Enforce the scroll bounds for the current content and focus.
    ///
    /// A structural change that shrinks the strip (closing a window, moving a
    /// column to another workspace, unfloating, etc.) reduces `total_width`
    /// without necessarily re-running `ensure_focused_visible`. That can leave
    /// `scroll_offset` past the content, which renders as the strip pushed too
    /// far left: the leftmost content is clipped off-screen and blank desktop
    /// shows at the right edge. This is a render-time safety net that only
    /// corrects an out-of-range offset; a valid offset is left untouched.
    ///
    /// No-op while an animation owns the offset. `center_past_edges` extends
    /// the appropriate bound only when the focused first/last active column is
    /// intentionally centered; it does not legalize arbitrary stale blank
    /// space after a shrink.
    pub fn clamp_scroll_to_bounds(&mut self, viewport_width: i32) {
        if self.active_animation.is_some() {
            return;
        }
        let vis_w = self.visible_width(viewport_width);
        let (min_scroll, max_scroll) = self.focused_scroll_bounds(vis_w);
        let clamped = sanitize_scroll_offset(self.scroll_offset).clamp(min_scroll, max_scroll);
        self.scroll_offset = clamped;
    }

    /// Resize the focused column by a delta amount.
    pub fn resize_focused_column(&mut self, delta: i32) {
        self.maximized_column = None;
        if let Some(column) = self.columns.get_mut(self.focused_column) {
            let new_width = column.width.saturating_add(delta).max(MIN_COLUMN_WIDTH);
            column.width = new_width;
            // Clear cached min-widths and min-heights so constraints will be
            // re-detected from the actual window size on the next apply cycle.
            for wid in column.windows() {
                self.window_min_widths.remove(wid);
                self.window_min_heights.remove(wid);
            }
        }
    }

    /// Move the focused column left (swap with the column to its left).
    pub fn move_column_left(&mut self) {
        if self.focused_column < self.columns.len() && self.focused_column > 0 {
            self.columns
                .swap(self.focused_column, self.focused_column - 1);
            self.focused_column -= 1;
        }
    }

    /// Move the focused column right (swap with the column to its right).
    pub fn move_column_right(&mut self) {
        if self.focused_column < self.columns.len().saturating_sub(1) {
            self.columns
                .swap(self.focused_column, self.focused_column + 1);
            self.focused_column += 1;
        }
    }

    /// Move the focused column to the start (leftmost) of the strip.
    pub fn move_column_to_start(&mut self) {
        self.reorder_column(self.focused_column, 0);
    }

    /// Move the focused column to the end (rightmost) of the strip.
    pub fn move_column_to_end(&mut self) {
        if !self.columns.is_empty() {
            self.reorder_column(self.focused_column, self.columns.len() - 1);
        }
    }

    /// Move a column from one index to another, shifting intermediate columns.
    /// No-op if indices are equal or out of bounds.
    pub fn reorder_column(&mut self, from: usize, to: usize) {
        if from == to || from >= self.columns.len() || to >= self.columns.len() {
            return;
        }
        let column = self.columns.remove(from);
        self.columns.insert(to, column);

        // Update focused_column to track correctly after the shift.
        if self.focused_column == from {
            self.focused_column = to;
        } else if from < to {
            // Column moved forward: indices in (from, to] shifted left by 1
            if self.focused_column > from && self.focused_column <= to {
                self.focused_column -= 1;
            }
        } else {
            // Column moved backward: indices in [to, from) shifted right by 1
            if self.focused_column >= to && self.focused_column < from {
                self.focused_column += 1;
            }
        }
        self.clamp_focus_indices();
    }

    /// Remove an entire column and return it. Used for cross-monitor drag.
    /// Returns `None` if index is out of bounds.
    pub fn remove_column(&mut self, index: usize) -> Option<Column> {
        if index >= self.columns.len() {
            return None;
        }
        // The animation target was computed from the old strip geometry and
        // can restore an invalid offset after this column disappears.
        self.cancel_animation();
        let col = self.columns.remove(index);
        for wid in col.windows() {
            // Mirror remove_window's per-window cleanup. Cross-monitor column
            // moves must not leave stale constraints or a maximize sentinel
            // behind in the source workspace.
            self.minimized_windows.remove(wid);
            self.window_min_widths.remove(wid);
            self.window_min_heights.remove(wid);
            self.pending_min_size_clears.remove(wid);
            if self.fullscreen_window == Some(*wid) {
                self.fullscreen_window = None;
            }
            if self
                .maximized_column
                .as_ref()
                .is_some_and(|m| m.sentinel_window == *wid)
            {
                self.maximized_column = None;
            }
        }
        if self.columns.is_empty() {
            self.focused_column = 0;
            self.focused_window_in_column = 0;
            self.scroll_offset = 0.0;
        } else {
            if self.focused_column > index {
                self.focused_column -= 1;
            }
            // Reclamp scroll offset — the strip may have shrunk
            let max_scroll = f64::from(self.total_width().max(0));
            self.scroll_offset = sanitize_scroll_offset(self.scroll_offset).clamp(0.0, max_scroll);
        }
        self.clamp_focus_indices();
        Some(col)
    }

    /// Insert a column at the given index. Used for cross-monitor drag.
    /// Index is clamped to `columns.len()`. Empty columns are rejected to
    /// preserve the invariant that all columns contain at least one window.
    pub fn insert_column_at(&mut self, column: Column, index: usize) {
        if column.is_empty() {
            return;
        }
        // Reject if any window already exists in this workspace
        if column
            .windows()
            .iter()
            .any(|wid| self.contains_window(*wid))
        {
            return;
        }
        let clamped = index.min(self.columns.len());
        let was_empty = self.columns.is_empty();
        self.columns.insert(clamped, column);
        if !was_empty && self.focused_column >= clamped {
            self.focused_column += 1;
        }
        self.clamp_focus_indices();
    }

    /// Move the focused window to the column on the left (joining it).
    /// Focus follows the moved window. If the source column becomes empty it is removed.
    /// In a Tabbed receiver, the moved window becomes the new active tab
    /// (consistent with the "user-initiated keyboard move" intent).
    pub fn move_window_left(&mut self) {
        if !self.has_valid_focus() {
            return;
        }
        if self.focused_column == 0 {
            // At the left edge there's no column to move into; instead unstack
            // the window into a new column at the left edge (no-op if not stacked).
            self.expel_to_left();
            return;
        }
        let Some(wid) =
            self.columns[self.focused_column].remove_at_index(self.focused_window_in_column)
        else {
            return;
        };
        let source_empty = self.columns[self.focused_column].is_empty();
        if source_empty {
            self.columns.remove(self.focused_column);
        }
        // Target is now one index to the left (or same index if source was removed)
        let target_idx = self.focused_column - 1;
        self.columns[target_idx].add_window(wid);
        self.focused_column = target_idx;
        self.focused_window_in_column = self.columns[target_idx].len() - 1;
        self.sync_active_tab_to_focus();
    }

    /// Move the focused window to the column on the right (joining it).
    /// Focus follows the moved window. If the source column becomes empty it is removed.
    /// In a Tabbed receiver, the moved window becomes the new active tab.
    pub fn move_window_right(&mut self) {
        if !self.has_valid_focus() {
            return;
        }
        if self.focused_column >= self.columns.len().saturating_sub(1) {
            // At the right edge: unstack into a new column off the end
            // instead of a dead-end (no-op if the column isn't stacked).
            self.expel_to_right();
            return;
        }
        let Some(wid) =
            self.columns[self.focused_column].remove_at_index(self.focused_window_in_column)
        else {
            return;
        };
        let source_empty = self.columns[self.focused_column].is_empty();
        if source_empty {
            self.columns.remove(self.focused_column);
            // Right column shifted left into focused_column's slot
            self.columns[self.focused_column].add_window(wid);
            self.focused_window_in_column = self.columns[self.focused_column].len() - 1;
        } else {
            let right_idx = self.focused_column + 1;
            self.columns[right_idx].add_window(wid);
            self.focused_column = right_idx;
            self.focused_window_in_column = self.columns[right_idx].len() - 1;
        }
        self.sync_active_tab_to_focus();
    }

    /// Push the focused window out to a new column on the left.
    /// The new column is always Vertical (single-window).
    pub fn expel_to_left(&mut self) {
        if !self.has_valid_focus() || self.columns[self.focused_column].len() <= 1 {
            return;
        }
        let Some(wid) =
            self.columns[self.focused_column].remove_at_index(self.focused_window_in_column)
        else {
            return;
        };
        // Clamp focus in old column (column.remove_at_index already adjusted
        // active_idx for Tabbed; here we clamp focused_window_in_column).
        let old_len = self.columns[self.focused_column].len();
        if self.focused_window_in_column >= old_len {
            self.focused_window_in_column = old_len.saturating_sub(1);
        }
        let width = self.columns[self.focused_column].width();
        let new_col = Column::new(wid, width);
        self.columns.insert(self.focused_column, new_col);
        // Focus the new column (it took the current index)
        self.focused_window_in_column = 0;
        // sync not needed: new column is Vertical.
    }

    /// Push the focused window out to a new column on the right.
    /// The new column is always Vertical (single-window).
    pub fn expel_to_right(&mut self) {
        if !self.has_valid_focus() || self.columns[self.focused_column].len() <= 1 {
            return;
        }
        let Some(wid) =
            self.columns[self.focused_column].remove_at_index(self.focused_window_in_column)
        else {
            return;
        };
        let old_len = self.columns[self.focused_column].len();
        if self.focused_window_in_column >= old_len {
            self.focused_window_in_column = old_len.saturating_sub(1);
        }
        let width = self.columns[self.focused_column].width();
        let new_col = Column::new(wid, width);
        self.columns.insert(self.focused_column + 1, new_col);
        self.focused_column += 1;
        self.focused_window_in_column = 0;
    }

    /// Pull the top window of the column to the right into the focused
    /// column (the inverse of expel). The window is appended to the focused
    /// column's stack and becomes focused; if the right column empties, it
    /// is removed. No-op if there is no column to the right.
    pub fn consume_from_right(&mut self) {
        if self.focused_column >= self.columns.len() {
            return;
        }
        let right = self.focused_column.saturating_add(1);
        if right >= self.columns.len() {
            return;
        }
        let Some(wid) = self.columns[right].remove_at_index(0) else {
            return;
        };
        if self.columns[right].is_empty() {
            self.columns.remove(right);
        }
        self.columns[self.focused_column].add_window(wid);
        self.focused_window_in_column = self.columns[self.focused_column].len().saturating_sub(1);
        // If the focused column is tabbed, make the consumed window the
        // active (visible) tab rather than leaving the old tab showing.
        self.sync_active_tab_to_focus();
    }

    /// Pull the top window of the column to the left into the focused
    /// column (the inverse of expel). The window is appended to the focused
    /// column's stack and becomes focused; if the left column empties, it
    /// is removed and the focus index follows the focused column. No-op if
    /// there is no column to the left.
    pub fn consume_from_left(&mut self) {
        if self.focused_column == 0 || self.focused_column >= self.columns.len() {
            return;
        }
        let left = self.focused_column - 1;
        let Some(wid) = self.columns[left].remove_at_index(0) else {
            return;
        };
        if self.columns[left].is_empty() {
            self.columns.remove(left);
            self.focused_column -= 1;
        }
        self.columns[self.focused_column].add_window(wid);
        self.focused_window_in_column = self.columns[self.focused_column].len().saturating_sub(1);
        // If the focused column is tabbed, make the consumed window the
        // active (visible) tab rather than leaving the old tab showing.
        self.sync_active_tab_to_focus();
    }

    /// Swap the focused window with the one above in the same column.
    /// In a Tabbed column, `swap_windows` keeps `active_idx` tracking the
    /// same window (handled inside `Column::swap_windows`).
    pub fn move_window_up_in_column(&mut self) {
        if !self.has_valid_focus() || self.focused_window_in_column == 0 {
            return;
        }
        self.columns[self.focused_column].swap_windows(
            self.focused_window_in_column,
            self.focused_window_in_column - 1,
        );
        self.focused_window_in_column -= 1;
        self.sync_active_tab_to_focus();
    }

    /// Swap the focused window with the one below in the same column.
    pub fn move_window_down_in_column(&mut self) {
        if !self.has_valid_focus()
            || self.focused_window_in_column
                >= self.columns[self.focused_column].len().saturating_sub(1)
        {
            return;
        }
        self.columns[self.focused_column].swap_windows(
            self.focused_window_in_column,
            self.focused_window_in_column + 1,
        );
        self.focused_window_in_column += 1;
        self.sync_active_tab_to_focus();
    }

    /// Scroll the viewport by a pixel delta.
    ///
    /// Cancels any active scroll animation so the manual scroll takes effect
    /// immediately. Special float values (NaN, Infinity) are treated as zero.
    pub fn scroll_by(&mut self, delta: f64, viewport_width: i32) {
        // Cancel any in-flight animation so manual scroll is not overridden
        self.cancel_animation();
        // Treat NaN and Infinity as zero and repair a previously-corrupt
        // stored offset before arithmetic.
        let safe_delta = if delta.is_finite() { delta } else { 0.0 };
        let next = sanitize_scroll_offset(self.scroll_offset) + safe_delta;
        let next = if next.is_finite() {
            sanitize_scroll_offset(next)
        } else if next.is_sign_positive() {
            f64::from(i32::MAX)
        } else if next.is_sign_negative() {
            f64::from(i32::MIN)
        } else {
            0.0
        };
        let vis_w = self.visible_width(viewport_width);
        let max_scroll = f64::from((self.total_width() - vis_w).max(0));
        self.scroll_offset = next.clamp(0.0, max_scroll);
    }

    // ========================================================================
    // Animation Methods
    // ========================================================================

    /// Check if a scroll animation is currently active.
    pub fn is_animating(&self) -> bool {
        self.active_animation.is_some()
    }

    /// Get the current effective scroll offset.
    /// Returns the animated offset if an animation is active, otherwise the base offset.
    pub fn effective_scroll_offset(&self) -> f64 {
        match &self.active_animation {
            Some(anim) => sanitize_scroll_offset(anim.current_offset()),
            None => sanitize_scroll_offset(self.scroll_offset),
        }
    }

    fn start_scroll_animation_to(
        &mut self,
        target: f64,
        duration_ms: Option<u64>,
        easing: Option<Easing>,
    ) {
        let target = sanitize_scroll_offset(target);
        let start = self.effective_scroll_offset();
        if (start - target).abs() < 0.5 {
            self.scroll_offset = target;
            self.active_animation = None;
            return;
        }

        self.active_animation = Some(ScrollAnimation::new(
            start,
            target,
            duration_ms.unwrap_or(self.scroll_duration_ms),
            easing.unwrap_or(self.scroll_easing),
        ));
    }

    /// Start an animated scroll to a target offset. Public callers retain the
    /// current focus-aware bounds; focus navigation uses the more precise plan
    /// produced by `focus_scroll_target`.
    pub fn start_scroll_animation(
        &mut self,
        target: f64,
        viewport_width: i32,
        duration_ms: Option<u64>,
        easing: Option<Easing>,
    ) {
        let visible_width = self.visible_width(viewport_width);
        let bounds = self.focused_scroll_bounds(visible_width);
        let target = sanitize_scroll_offset(target);
        self.start_scroll_animation_to(target.clamp(bounds.0, bounds.1), duration_ms, easing);
    }

    /// Advance the active animation by the given delta time in milliseconds.
    /// Returns true if an animation is still active, false if complete or no animation.
    pub fn tick_animation(&mut self, delta_ms: u64) -> bool {
        let Some(anim) = &mut self.active_animation else {
            return false;
        };

        let still_running = anim.tick(delta_ms);

        if !still_running {
            // Animation complete - finalize scroll offset and clear animation
            self.scroll_offset = sanitize_scroll_offset(anim.target());
            self.active_animation = None;
            false
        } else {
            true
        }
    }

    /// Stop the current animation and snap to the target position.
    pub fn stop_animation(&mut self) {
        if let Some(anim) = self.active_animation.take() {
            self.scroll_offset = sanitize_scroll_offset(anim.target());
        }
    }

    /// Cancel the current animation and stay at the current position.
    pub fn cancel_animation(&mut self) {
        if let Some(anim) = self.active_animation.take() {
            self.scroll_offset = sanitize_scroll_offset(anim.current_offset());
        }
    }

    /// Ensure the focused column is visible with animation. The same pure
    /// target calculation is used by both animated and reduced-motion paths.
    pub fn ensure_focused_visible_animated(&mut self, viewport_width: i32) {
        if self.reduce_motion {
            self.stop_animation();
            self.ensure_focused_visible(viewport_width);
            return;
        }

        let Some((column_x, column_width)) = self.focused_column_bounds() else {
            return;
        };
        let visible_width = self.visible_width(viewport_width);
        let target = self.focus_scroll_target(
            self.effective_scroll_offset(),
            column_x,
            column_width,
            visible_width,
        );
        let bounds = self.scroll_bounds_for_focus(visible_width, target.allow_edge_overscroll);
        self.start_scroll_animation_to(target.offset.clamp(bounds.0, bounds.1), None, None);
    }

    /// Center the focused column in the viewport, regardless of centering mode.
    pub fn center_focused_column_animated(&mut self, viewport_width: i32) {
        let Some((column_x, column_width)) = self.focused_column_bounds() else {
            return;
        };
        let visible_width = self.visible_width(viewport_width);
        let target = Self::centered_scroll_target(column_x, column_width, visible_width);
        let bounds = self.scroll_bounds_for_focus(visible_width, true);
        let target = target.clamp(bounds.0, bounds.1);

        if self.reduce_motion {
            self.stop_animation();
            self.scroll_offset = target;
        } else {
            self.start_scroll_animation_to(target, None, None);
        }
    }
}

#[cfg(test)]
mod edge_centering_tests {
    use super::*;

    fn five_columns() -> Workspace {
        let mut workspace = Workspace::with_gaps(10, 10);
        for window_id in 1..=5 {
            workspace.insert_window(window_id, Some(600)).unwrap();
        }
        workspace.set_centering_mode(CenteringMode::Center);
        workspace.set_center_past_edges(true);
        workspace
    }

    fn finish_scroll(workspace: &mut Workspace) {
        assert!(!workspace.tick_animation(10_000));
    }

    #[test]
    fn animated_centering_places_first_column_at_viewport_center() {
        let mut workspace = five_columns();
        workspace.set_focus(0, 0).unwrap();

        workspace.ensure_focused_visible_animated(1920);
        finish_scroll(&mut workspace);

        // visible width = 1920 - 10 - 10 = 1900
        // target = 0 + 600/2 - 1900/2 = -650
        assert_eq!(workspace.scroll_offset(), -650.0);
    }

    #[test]
    fn animated_centering_places_last_column_at_viewport_center() {
        let mut workspace = five_columns();
        workspace.set_focus(4, 0).unwrap();

        workspace.ensure_focused_visible_animated(1920);
        finish_scroll(&mut workspace);

        // last x = 4 * (600 + 10) = 2440
        // target = 2440 + 600/2 - 1900/2 = 1790
        assert_eq!(workspace.scroll_offset(), 1790.0);
    }

    #[test]
    fn disabling_edge_centering_keeps_normal_scroll_bounds() {
        let mut workspace = five_columns();
        workspace.set_center_past_edges(false);
        workspace.set_focus(0, 0).unwrap();

        workspace.ensure_focused_visible_animated(1920);
        finish_scroll(&mut workspace);

        assert_eq!(workspace.scroll_offset(), 0.0);
    }

    #[test]
    fn manual_scroll_never_creates_edge_blank_space() {
        let mut workspace = five_columns();
        workspace.set_focus(0, 0).unwrap();

        workspace.scroll_by(-10_000.0, 1920);
        assert_eq!(workspace.scroll_offset(), 0.0);
    }
}
