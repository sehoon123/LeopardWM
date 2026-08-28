use crate::*;

use crate::workspace::Workspace;

impl Workspace {
    /// Compute placements for all windows given a viewport.
    ///
    /// Returns a list of WindowPlacement structs indicating where each window
    /// should be positioned and whether it's visible or off-screen.
    ///
    /// Note: Negative gaps are treated as zero for calculation purposes.
    pub fn compute_placements(&self, viewport: Rect) -> Vec<WindowPlacement> {
        // Use rounding instead of truncation to prevent sub-pixel jitter
        let viewport_left = rounded_scroll_offset(self.scroll_offset);

        // Fullscreen mode: one window covers the entire viewport, others are off-screen
        if let Some(fs_wid) = self.fullscreen_window {
            return self.compute_fullscreen_placements(fs_wid, viewport, viewport_left);
        }

        self.compute_non_fullscreen_placements(viewport, viewport_left)
    }

    /// Compute placements for the ENTIRE strip, ignoring scroll: viewport
    /// left is pinned to 0 and the caller supplies a viewport wide enough
    /// for every column, so nothing is marked off-screen. Used by the
    /// workspace overview to miniaturize the whole strip.
    pub fn placements_for_full_strip(&self, viewport: Rect) -> Vec<WindowPlacement> {
        self.compute_non_fullscreen_placements(viewport, 0)
    }

    /// Compute non-fullscreen placements for a specific viewport-left offset.
    /// Used by both static and animated placement paths.
    fn compute_non_fullscreen_placements(
        &self,
        viewport: Rect,
        viewport_left: i32,
    ) -> Vec<WindowPlacement> {
        let mut placements = Vec::with_capacity(self.window_count());
        // Reuse per-column scratch buffers instead of allocating three Vecs
        // for every column on every animation frame.
        let mut visible_windows: smallvec::SmallVec<[(usize, WindowId); 8]> =
            smallvec::SmallVec::new();
        let mut visible_weights: smallvec::SmallVec<[f64; 8]> = smallvec::SmallVec::new();
        let mut min_heights: smallvec::SmallVec<[i32; 8]> = smallvec::SmallVec::new();

        // Defensively clamp gaps to >= 0 in case fields were set directly
        let gap = self.gap.max(0);
        let outer_left = self.outer_gap_left.max(0);
        let outer_top = self.outer_gap_top.max(0);
        let outer_bottom = self.outer_gap_bottom.max(0);

        // Visible strip area inside viewport padding
        let vis_w = self.visible_width(viewport.width);
        let visible_right = viewport_left.saturating_add(vis_w);

        // Focus scrolling, strip bounds, and placement all share this exact
        // effective geometry. Keeping the learned-minimum adjustment in one
        // helper prevents a focused column from being "visible" in stored
        // widths while placement has moved it elsewhere.
        let effective_widths = self.effective_column_widths();

        // Strip starts at 0 — outer gaps are viewport padding
        let mut current_x: i32 = 0;

        for (col_idx, column) in self.columns.iter().enumerate() {
            // Fully minimized columns consume no strip geometry. Bail out
            // before building any per-column scratch data.
            if !self.is_column_active(column) {
                continue;
            }
            visible_windows.clear();
            visible_weights.clear();
            min_heights.clear();

            let eff_width = effective_widths
                .get(col_idx)
                .copied()
                .unwrap_or_else(|| column.width());

            // Calculate column position in strip coordinates
            let col_strip_x = current_x;
            let col_strip_right = col_strip_x.saturating_add(eff_width);

            // Transform to screen coordinates in a widened domain. This still
            // permits negative positions for scrolled columns without making
            // extreme public viewport/scroll values build-profile dependent.
            let natural_screen_x = i64_to_i32_saturating(
                i64::from(col_strip_x)
                    .saturating_sub(i64::from(viewport_left))
                    .saturating_add(i64::from(viewport.x))
                    .saturating_add(i64::from(outer_left)),
            );

            // Determine visibility against the visible strip area
            let visibility = if col_strip_right <= viewport_left {
                Visibility::OffScreenLeft
            } else if col_strip_x >= visible_right {
                Visibility::OffScreenRight
            } else {
                Visibility::Visible
            };

            // Push off-screen columns past the viewport edge so they cannot
            // peek through the outer-gap area when DWM cloaking is slow or
            // ineffective. The natural strip-flow position for an
            // `OffScreenRight` column at the boundary is
            // `viewport.x + viewport.width - outer_right` — i.e. INSIDE
            // the viewport by `outer_right` pixels — so the leftmost
            // off-screen-right column's first `outer_right` pixels would
            // otherwise be visible if anything fails to cloak it.
            // Symmetric clamp for `OffScreenLeft`.
            //
            // The clamp/no-clamp boundary aligns with the cloak/uncloak
            // boundary in `apply_placements`: a column transitions from
            // OffScreen to Visible in the same `apply` call where the
            // window is uncloaked, so the position jump is invisible to
            // the user. Animation frames continue to use this placement
            // logic so the position transitions atomically with the
            // visibility flip.
            let mut col_screen_x = match visibility {
                Visibility::OffScreenRight => {
                    natural_screen_x.max(viewport.x.saturating_add(viewport.width))
                }
                Visibility::OffScreenLeft => {
                    natural_screen_x.min(viewport.x.saturating_sub(eff_width))
                }
                Visibility::Visible => natural_screen_x,
            };
            // A focused column whose enforceable minimum consumes outer-gap
            // space can still fit in the full work area. Sacrifice only the
            // necessary horizontal padding rather than clipping either edge.
            if visibility == Visibility::Visible
                && col_idx == self.focused_column
                && eff_width > vis_w
                && eff_width <= viewport.width
            {
                let max_x = viewport
                    .x
                    .saturating_add(viewport.width)
                    .saturating_sub(eff_width);
                col_screen_x = col_screen_x.clamp(viewport.x, max_x);
            }

            // Build the set of windows that occupy column geometry on this pass.
            // Vertical: all non-minimized windows split by height_weights.
            // Tabbed: only the active tab takes the full column rect; if it's
            // minimized, fall back to the first visible tab so the column
            // doesn't render empty.
            match column.mode() {
                crate::ColumnMode::Vertical => visible_windows.extend(
                    column
                        .windows()
                        .iter()
                        .enumerate()
                        .filter(|(_, w)| !self.minimized_windows.contains(w))
                        .map(|(i, &w)| (i, w)),
                ),
                crate::ColumnMode::Tabbed { .. } => {
                    // Shared picker: prefer active tab, fall back to first
                    // visible if active is minimized.
                    if let Some((i, &w)) = column
                        .effective_visible_tab(|w| self.minimized_windows.contains(&w))
                        .and_then(|i| column.windows().get(i).map(|w| (i, w)))
                    {
                        visible_windows.push((i, w));
                    }
                }
            }

            // In Tabbed mode, every non-active non-minimized tab gets an
            // off-screen placement so the daemon's cloak machinery hides it.
            // (Minimized tabs are already excluded by the apply path.)
            //
            // The rect is positioned `viewport.width` pixels to the LEFT of
            // the viewport so the window is genuinely off-screen even with
            // `SWP_NOSIZE` keeping its previous size. Cloak is still applied
            // by the platform layer; this position is defense-in-depth in
            // case cloak races, fails, or hasn't taken effect on the first
            // frame after the toggle.
            if column.is_tabbed() {
                let on_screen_idx = visible_windows.first().map(|(i, _)| *i);
                // `SWP_NOSIZE` can retain a tab wider than the viewport.
                // Move by at least that retained effective width as well as
                // the viewport width so the fallback position truly clears it.
                let offscreen_distance = viewport.width.max(eff_width).max(1);
                let offscreen_x = viewport.x.saturating_sub(offscreen_distance);
                for (i, &wid) in column.windows().iter().enumerate() {
                    if Some(i) == on_screen_idx {
                        continue;
                    }
                    if self.minimized_windows.contains(&wid) {
                        continue;
                    }
                    placements.push(WindowPlacement {
                        window_id: wid,
                        rect: Rect::new(offscreen_x, viewport.y, 0, 0),
                        visibility: Visibility::OffScreenLeft,
                        column_index: col_idx,
                    });
                }
            }

            // Reserve space at the top of Tabbed columns for the tab strip
            // overlay. Without this the strip is positioned at
            // `column.y - strip_h`, which lands above the work-area top
            // edge when `outer_top` is small — i.e. invisible. Reserving
            // shifts the active tab down by `strip_h` so the overlay has
            // room to render *inside* the column's allocated area.
            let column_top_reserve = if column.is_tabbed() {
                self.tab_strip_reserve_px.max(0)
            } else {
                0
            };

            // Build visible-window weights
            let usable_height = viewport
                .height
                .saturating_sub(outer_top)
                .saturating_sub(outer_bottom)
                .saturating_sub(column_top_reserve)
                .max(0);
            let window_count = visible_windows.len() as i32;
            let window_gaps = if window_count > 1 {
                gap.saturating_mul(window_count - 1)
            } else {
                0
            };
            let available_height = (usable_height - window_gaps).max(0);

            // Compute per-window heights respecting known min-heights. Each
            // window with a recorded minimum is pinned to at least that
            // minimum; the remaining space is distributed among the flexible
            // (no-min) windows using their height weights. The last window
            // absorbs rounding remainder so the column stays flush with the
            // viewport — this also means if there are no flexible windows,
            // any leftover space simply flows into the last pinned window.
            if column.height_weights.len() == column.windows().len() {
                visible_weights.extend(
                    visible_windows
                        .iter()
                        .map(|(i, _)| column.height_weights[*i]),
                );
            } else {
                visible_weights.resize(visible_windows.len(), 1.0);
            }

            min_heights.extend(visible_windows.iter().map(|(_, wid)| {
                self.window_min_heights
                    .get(wid)
                    .copied()
                    .unwrap_or(0)
                    .max(0)
            }));
            // Sum widened minimums so two valid i32::MAX public inputs cannot
            // overflow in debug builds or wrap in release builds.
            let total_min = min_heights
                .iter()
                .fold(0i64, |sum, &height| sum.saturating_add(i64::from(height)));
            let flex_height = (i64::from(available_height) - total_min).max(0);

            // Sum only finite non-negative weights. Persisted columns repair
            // these values, but placement remains defensive for internal and
            // future construction paths.
            let flex_weight_sum: f64 = visible_weights
                .iter()
                .zip(min_heights.iter())
                .filter(|(_, minimum)| **minimum == 0)
                .map(|(weight, _)| {
                    if weight.is_finite() && *weight > 0.0 {
                        *weight
                    } else {
                        0.0
                    }
                })
                .sum();
            // If any flexible window exists, pinned windows get exactly their
            // minimum and flexible windows share flex_height. If every window
            // is pinned, pinned windows still get exactly their minimum and
            // the last-window remainder rule absorbs any leftover space.
            let has_flex = flex_weight_sum.is_finite() && flex_weight_sum > 0.0;

            let mut current_y = i64::from(viewport.y)
                .saturating_add(i64::from(outer_top))
                .saturating_add(i64::from(column_top_reserve));
            let content_bottom = i64::from(viewport.y)
                .saturating_add(i64::from(viewport.height))
                .saturating_sub(i64::from(outer_bottom));
            let visible_placement_start = placements.len();

            for (win_idx, &(_, window_id)) in visible_windows.iter().enumerate() {
                let is_last = win_idx == visible_windows.len() - 1;
                let height = if is_last {
                    // Last window absorbs the rounding remainder so the column
                    // stays flush with the viewport, but we honor its minimum
                    // even if doing so causes the column to overflow downward.
                    (content_bottom - current_y)
                        .max(0)
                        .max(i64::from(min_heights[win_idx]))
                } else if min_heights[win_idx] > 0 {
                    // Pinned non-last window: exactly its minimum.
                    i64::from(min_heights[win_idx])
                } else if has_flex {
                    // Flexible window: share of flex_height by weight.
                    let weight = visible_weights[win_idx];
                    let share = if weight.is_finite() && weight > 0.0 {
                        weight / flex_weight_sum
                    } else {
                        0.0
                    };
                    let requested = flex_height as f64 * share;
                    if requested.is_finite() {
                        requested.round().clamp(0.0, i32::MAX as f64) as i64
                    } else {
                        0
                    }
                } else {
                    // No flex windows, and this one isn't pinned — give it an
                    // even split of available_height as a last resort.
                    i64::from(available_height)
                        / i64::try_from(visible_windows.len().max(1)).unwrap_or(i64::MAX)
                };
                let height = i64_to_i32_saturating(height.max(0));

                placements.push(WindowPlacement {
                    window_id,
                    rect: Rect::new(
                        col_screen_x,
                        i64_to_i32_saturating(current_y),
                        eff_width,
                        height,
                    ),
                    visibility,
                    column_index: col_idx,
                });

                current_y = current_y
                    .saturating_add(i64::from(height))
                    .saturating_add(i64::from(gap));
            }

            // Minimum heights may consume the top/bottom outer padding while
            // still fitting physically in the work area. Shift this column's
            // complete visible stack as a unit when that exposes both edges;
            // if the stack itself exceeds the viewport, no translation can do
            // so and the application's combined minimums are impossible here.
            let visible_slice = &mut placements[visible_placement_start..];
            if let (Some(first), Some(last)) = (visible_slice.first(), visible_slice.last()) {
                let stack_top = first.rect.y;
                let stack_bottom = last.rect.bottom();
                let stack_height = stack_bottom.saturating_sub(stack_top);
                if stack_height <= viewport.height {
                    let max_top = viewport
                        .y
                        .saturating_add(viewport.height)
                        .saturating_sub(stack_height);
                    let clamped_top = stack_top.clamp(viewport.y, max_top);
                    let shift = clamped_top.saturating_sub(stack_top);
                    if shift != 0 {
                        for placement in visible_slice {
                            placement.rect.y = placement.rect.y.saturating_add(shift);
                        }
                    }
                }
            }

            current_x = current_x.saturating_add(eff_width).saturating_add(gap);
        }

        // Add floating windows (visible unless minimized, at their absolute positions)
        for floating in &self.floating_windows {
            if self.minimized_windows.contains(&floating.id) {
                continue;
            }
            placements.push(WindowPlacement {
                window_id: floating.id,
                rect: floating.rect,
                visibility: Visibility::Visible,
                column_index: usize::MAX, // Sentinel for floating windows
            });
        }

        placements
    }

    /// Compute placements for all windows, using animated scroll offset if active.
    ///
    /// This is similar to `compute_placements` but uses `effective_scroll_offset()`
    /// to support smooth scrolling animations.
    pub fn compute_placements_animated(&self, viewport: Rect) -> Vec<WindowPlacement> {
        // Use animated scroll offset
        let viewport_left = rounded_scroll_offset(self.effective_scroll_offset());

        // Fullscreen mode: one window covers the entire viewport, others are off-screen
        if let Some(fs_wid) = self.fullscreen_window {
            return self.compute_fullscreen_placements(fs_wid, viewport, viewport_left);
        }

        self.compute_non_fullscreen_placements(viewport, viewport_left)
    }

    /// Compute placements when a window is fullscreen.
    ///
    /// The fullscreen window gets the full viewport; all others are marked
    /// off-screen but KEEP their real layout rects — cloaking hides them, and
    /// the platform layer moves off-screen windows with `SWP_NOSIZE`, so a
    /// zeroed rect would visibly snap them to the top-left corner. Pinned
    /// floating windows stay Visible at their floating rect, above fullscreen.
    fn compute_fullscreen_placements(
        &self,
        fs_wid: WindowId,
        viewport: Rect,
        viewport_left: i32,
    ) -> Vec<WindowPlacement> {
        // Stale or minimized fullscreen target: fall back to normal placements.
        if !self.contains_window(fs_wid) || self.minimized_windows.contains(&fs_wid) {
            return self.compute_non_fullscreen_placements(viewport, viewport_left);
        }

        let mut placements = self.compute_non_fullscreen_placements(viewport, viewport_left);

        for placement in &mut placements {
            if placement.window_id == fs_wid {
                placement.rect = viewport;
                placement.visibility = Visibility::Visible;
            } else if placement.column_index == usize::MAX
                && self
                    .floating_windows
                    .iter()
                    .any(|f| f.id == placement.window_id && f.pinned)
            {
                // Pinned floating window: stays exactly as computed.
            } else {
                placement.visibility = Visibility::OffScreenLeft;
            }
        }

        placements
    }
}
