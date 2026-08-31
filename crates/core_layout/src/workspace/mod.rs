pub mod focus;
pub mod layout;
pub mod operations;
pub mod sizing;
pub mod state;

use serde::{de::Error as DeError, Deserialize, Deserializer, Serialize};
use std::collections::{HashMap, HashSet};

use crate::animation::{Easing, ScrollAnimation, DEFAULT_ANIMATION_DURATION_MS};
use crate::column::Column;
use crate::types::*;

/// Focus centering mode.
/// Determines how the viewport adjusts when focus changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CenteringMode {
    /// Center the focused column in the viewport.
    #[default]
    Center,
    /// Fully expose a fitting column at a deterministic edge anchor. For an
    /// oversized column, keep the viewport inside it without an unnecessary
    /// edge snap.
    JustInView,
    /// Behave like `JustInView` for fitting columns and center only when the
    /// focused column is wider than the viewport.
    OnOverflow,
}

/// A floating window that is not part of the tiling layout.
///
/// Floating windows are positioned at absolute coordinates and always
/// remain visible (not scrolled with the workspace).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FloatingWindow {
    /// The window identifier.
    pub id: WindowId,
    /// The position and size of the floating window.
    pub rect: Rect,
    /// Whether the window stays visible above a fullscreen window.
    #[serde(default)]
    pub pinned: bool,
}

/// Tiling state needed to restore a manually floated window.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FloatOrigin {
    pub(crate) left_neighbor: Option<WindowId>,
    pub(crate) fallback_index: usize,
    pub(crate) column_width: i32,
}

/// The scrollable workspace.
/// This is the core data structure representing the infinite horizontal strip.
///
/// # Invariants
///
/// The following invariants are maintained by all methods:
///
/// 1. **No duplicate windows:** Each `WindowId` appears at most once.
/// 2. **Valid focus:** If `columns` is empty, `focused_window()` returns `None`.
///    Otherwise, `focused_column < columns.len()` and
///    `focused_window_in_column < columns[focused_column].len()`.
/// 3. **Valid column widths:** All column widths are >= `MIN_COLUMN_WIDTH` (100px).
/// 4. **Valid scroll range:** `0.0 <= scroll_offset <= max_scroll` where
///    `max_scroll = (total_width() - viewport_width).max(0)`.
///    Exception: when `center_past_edges` is true, `scroll_offset` may be
///    negative (centering first column) or exceed `max_scroll` (last column).
#[derive(Debug, Clone, Serialize)]
pub struct Workspace {
    /// Columns in the workspace, ordered left to right.
    pub(crate) columns: Vec<Column>,
    /// Index of the currently focused column.
    pub(crate) focused_column: usize,
    /// Index of the focused window within the focused column.
    pub(crate) focused_window_in_column: usize,
    /// Current scroll offset (x position of viewport's left edge on the strip).
    pub(crate) scroll_offset: f64,
    /// Gap between columns in pixels (always >= 0).
    pub(crate) gap: i32,
    /// Gap at the left edge of the viewport (always >= 0).
    #[serde(default = "default_outer_gap_value")]
    pub(crate) outer_gap_left: i32,
    /// Gap at the right edge of the viewport (always >= 0).
    #[serde(default = "default_outer_gap_value")]
    pub(crate) outer_gap_right: i32,
    /// Gap at the top edge of the viewport (always >= 0).
    #[serde(default = "default_outer_gap_value")]
    pub(crate) outer_gap_top: i32,
    /// Gap at the bottom edge of the viewport (always >= 0).
    #[serde(default = "default_outer_gap_value")]
    pub(crate) outer_gap_bottom: i32,
    /// Default width for new columns (always >= MIN_COLUMN_WIDTH).
    pub(crate) default_column_width: i32,
    /// Centering mode for focus changes.
    pub(crate) centering_mode: CenteringMode,
    /// Active scroll animation, if any.
    #[serde(skip)]
    pub(crate) active_animation: Option<ScrollAnimation>,
    /// Floating windows outside the tiling layout.
    #[serde(default)]
    pub(crate) floating_windows: Vec<FloatingWindow>,
    /// Window ID in fullscreen mode, if any.
    #[serde(default)]
    pub(crate) fullscreen_window: Option<WindowId>,
    /// Windows that are currently minimized (excluded from layout).
    #[serde(default)]
    pub(crate) minimized_windows: HashSet<WindowId>,
    /// Known minimum widths for windows that enforce a minimum size.
    /// Detected by the platform layer and fed back so the layout engine
    /// can allocate correct column widths from the start.
    #[serde(skip)]
    pub(crate) window_min_widths: HashMap<WindowId, i32>,
    /// Known minimum heights for windows that enforce a minimum size.
    /// Detected by the platform layer and fed back so the layout engine
    /// can allocate correct intra-column heights from the start.
    #[serde(skip)]
    pub(crate) window_min_heights: HashMap<WindowId, i32>,
    /// Windows whose min-size constraints are scheduled for clearing on the
    /// next apply_layout pass. Populated when column composition changes so
    /// stale per-sibling constraints learned under the old window count can't
    /// over-allocate; the actual removal is deferred so a timed-out / paused
    /// apply path cannot strand the column with cleared constraints.
    #[serde(skip)]
    pub(crate) pending_min_size_clears: HashSet<WindowId>,
    /// Origin info for windows floated via toggle_floating.
    /// The left-neighbor/index pair restores position after intervening column changes;
    /// `column_width` preserves the user's custom tiled width across the round trip.
    #[serde(skip)]
    pub(crate) float_origin_column: HashMap<WindowId, FloatOrigin>,
    /// Snap scroll instantly instead of animating (Windows "Show animations" off).
    #[serde(skip)]
    pub(crate) reduce_motion: bool,
    /// Duration (ms) for scroll animations. Set by the daemon from
    /// `[animation].scroll_duration_ms`; defaults to the engine default.
    #[serde(skip)]
    pub(crate) scroll_duration_ms: u64,
    /// Easing curve for scroll animations. Set by the daemon from
    /// `[animation].easing`; defaults to cubic ease-out.
    #[serde(skip)]
    pub(crate) scroll_easing: Easing,
    /// Whether center-column can scroll past content edges.
    #[serde(skip)]
    pub(crate) center_past_edges: bool,
    /// State for maximized column toggle (fills viewport width).
    #[serde(skip)]
    pub(crate) maximized_column: Option<MaximizedColumnState>,
    /// Pixels reserved at the top of each Tabbed column for the tab strip
    /// overlay. The daemon sets this from `appearance.tab_strip_height` scaled
    /// by the focused monitor's DPI so the strip has room to render above
    /// the active tab. `0` (default, used in tests/headless) means no
    /// reservation — strip would overlap the active tab's top edge or sit
    /// off-screen above the work area.
    #[serde(skip)]
    pub(crate) tab_strip_reserve_px: i32,
}

/// State saved when a column is maximized to fill the viewport width.
#[derive(Debug, Clone)]
pub struct MaximizedColumnState {
    /// The original column width before maximizing.
    pub original_width: i32,
    /// Sentinel window ID used to relocate the column after index shifts.
    pub sentinel_window: WindowId,
}

#[derive(Debug, Deserialize)]
struct PersistedWorkspace {
    #[serde(default)]
    columns: Vec<Column>,
    #[serde(default)]
    focused_column: usize,
    #[serde(default)]
    focused_window_in_column: usize,
    #[serde(default)]
    scroll_offset: f64,
    #[serde(default = "default_gap_value")]
    gap: i32,
    #[serde(default = "default_outer_gap_value")]
    outer_gap_left: i32,
    #[serde(default = "default_outer_gap_value")]
    outer_gap_right: i32,
    #[serde(default = "default_outer_gap_value")]
    outer_gap_top: i32,
    #[serde(default = "default_outer_gap_value")]
    outer_gap_bottom: i32,
    #[serde(default = "default_column_width_value")]
    default_column_width: i32,
    #[serde(default)]
    centering_mode: CenteringMode,
    #[serde(default)]
    floating_windows: Vec<FloatingWindow>,
    #[serde(default)]
    fullscreen_window: Option<WindowId>,
    #[serde(default)]
    minimized_windows: HashSet<WindowId>,
}

fn default_gap_value() -> i32 {
    DEFAULT_GAP
}

fn default_column_width_value() -> i32 {
    DEFAULT_COLUMN_WIDTH
}

impl<'de> Deserialize<'de> for Workspace {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let persisted = PersistedWorkspace::deserialize(deserializer)?;
        let mut workspace = Self {
            columns: persisted.columns,
            focused_column: persisted.focused_column,
            focused_window_in_column: persisted.focused_window_in_column,
            scroll_offset: sanitize_scroll_offset(persisted.scroll_offset),
            gap: persisted.gap.max(0),
            outer_gap_left: persisted.outer_gap_left.max(0),
            outer_gap_right: persisted.outer_gap_right.max(0),
            outer_gap_top: persisted.outer_gap_top.max(0),
            outer_gap_bottom: persisted.outer_gap_bottom.max(0),
            default_column_width: persisted.default_column_width.max(MIN_COLUMN_WIDTH),
            centering_mode: persisted.centering_mode,
            active_animation: None,
            floating_windows: persisted.floating_windows,
            fullscreen_window: persisted.fullscreen_window,
            minimized_windows: persisted.minimized_windows,
            window_min_widths: HashMap::new(),
            window_min_heights: HashMap::new(),
            pending_min_size_clears: HashSet::new(),
            float_origin_column: HashMap::new(),
            reduce_motion: false,
            scroll_duration_ms: DEFAULT_ANIMATION_DURATION_MS,
            scroll_easing: Easing::default(),
            center_past_edges: false,
            maximized_column: None,
            tab_strip_reserve_px: 0,
        };
        workspace
            .validate_persisted_state()
            .map_err(<D::Error as DeError>::custom)?;
        Ok(workspace)
    }
}

impl Default for Workspace {
    fn default() -> Self {
        Self {
            columns: Vec::new(),
            focused_column: 0,
            focused_window_in_column: 0,
            scroll_offset: 0.0,
            gap: DEFAULT_GAP,
            outer_gap_left: DEFAULT_OUTER_GAP,
            outer_gap_right: DEFAULT_OUTER_GAP,
            outer_gap_top: DEFAULT_OUTER_GAP,
            outer_gap_bottom: DEFAULT_OUTER_GAP,
            default_column_width: DEFAULT_COLUMN_WIDTH,
            centering_mode: CenteringMode::default(),
            active_animation: None,
            floating_windows: Vec::new(),
            fullscreen_window: None,
            minimized_windows: HashSet::new(),
            window_min_widths: HashMap::new(),
            window_min_heights: HashMap::new(),
            pending_min_size_clears: HashSet::new(),
            float_origin_column: HashMap::new(),
            reduce_motion: false,
            scroll_duration_ms: DEFAULT_ANIMATION_DURATION_MS,
            scroll_easing: Easing::default(),
            center_past_edges: false,
            maximized_column: None,
            tab_strip_reserve_px: 0,
        }
    }
}

impl Workspace {
    /// Reject persisted topology that would violate safe-method assumptions.
    /// Numeric fields that have harmless legacy representations are normalized
    /// during deserialization; ownership and focus corruption is rejected.
    fn validate_persisted_state(&mut self) -> Result<(), String> {
        let mut seen = HashSet::new();
        for column in &self.columns {
            if column.is_empty() {
                return Err("persisted workspace contains an empty column".into());
            }
            for &window_id in column.windows() {
                if !seen.insert(window_id) {
                    return Err(format!("persisted workspace duplicates window {window_id}"));
                }
            }
        }
        for floating in &self.floating_windows {
            if !seen.insert(floating.id) {
                return Err(format!(
                    "persisted workspace duplicates window {}",
                    floating.id
                ));
            }
        }

        if self.columns.is_empty() {
            self.focused_column = 0;
            self.focused_window_in_column = 0;
        } else {
            let Some(column) = self.columns.get(self.focused_column) else {
                return Err("persisted workspace has an out-of-range focused column".into());
            };
            if self.focused_window_in_column >= column.len() {
                return Err("persisted workspace has an out-of-range focused window".into());
            }
            // A focused tabbed column must render the focused tab.
            self.columns[self.focused_column].set_active_tab(self.focused_window_in_column);
        }

        if !self.minimized_windows.is_subset(&seen) {
            return Err("persisted workspace minimizes an unknown window".into());
        }
        if let Some(window_id) = self.fullscreen_window {
            if !seen.contains(&window_id) || self.minimized_windows.contains(&window_id) {
                return Err("persisted workspace has an invalid fullscreen window".into());
            }
        }
        Ok(())
    }

    /// Widths used by both focus scrolling and placement. Learned minimums
    /// may transiently widen constrained columns and reduce flexible columns;
    /// every strip calculation must use this same geometry.
    pub(crate) fn effective_column_widths(&self) -> Vec<i32> {
        let mut widths: Vec<i32> = self.columns.iter().map(|column| column.width()).collect();
        if self.window_min_widths.is_empty() {
            return widths;
        }

        let mut excess = 0i64;
        let mut flexible_total = 0i64;
        for (index, column) in self.columns.iter().enumerate() {
            if !self.is_column_active(column) {
                continue;
            }
            let minimum = self.column_effective_min_width(column);
            if minimum > column.width() {
                excess = excess.saturating_add(i64::from(minimum) - i64::from(column.width()));
                widths[index] = minimum;
            } else {
                flexible_total = flexible_total.saturating_add(i64::from(column.width()));
            }
        }

        if excess == 0 || flexible_total == 0 {
            return widths;
        }

        let mut remaining = excess;
        for (index, column) in self.columns.iter().enumerate() {
            if !self.is_column_active(column) || widths[index] != column.width() {
                continue;
            }
            let requested =
                (f64::from(column.width()) / flexible_total as f64 * excess as f64).round();
            let proportional_share = if requested.is_finite() && requested > 0.0 {
                requested.min(i64::MAX as f64) as i64
            } else {
                0
            };
            let capacity = i64::from(column.width().saturating_sub(MIN_COLUMN_WIDTH));
            let shrink = proportional_share.min(remaining).min(capacity);
            widths[index] = widths[index].saturating_sub(i64_to_i32_saturating(shrink));
            remaining -= shrink;
        }
        widths
    }

    pub(crate) fn total_width_for_effective_widths(&self, widths: &[i32]) -> i32 {
        let mut active_count = 0usize;
        let mut column_widths = 0i64;
        for (index, column) in self.columns.iter().enumerate() {
            if self.is_column_active(column) {
                active_count += 1;
                column_widths = column_widths.saturating_add(i64::from(
                    widths.get(index).copied().unwrap_or_else(|| column.width()),
                ));
            }
        }
        if active_count == 0 {
            return 0;
        }
        let gaps = i64::from(self.gap.max(0))
            .saturating_mul(i64::try_from(active_count.saturating_sub(1)).unwrap_or(i64::MAX));
        i64_to_i32_saturating(column_widths.saturating_add(gaps))
    }

    /// Create a new empty workspace with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a workspace with uniform gap settings.
    /// Gap values are clamped to >= 0.
    pub fn with_gaps(gap: i32, outer_gap: i32) -> Self {
        let og = outer_gap.max(0);
        Self {
            gap: gap.max(0),
            outer_gap_left: og,
            outer_gap_right: og,
            outer_gap_top: og,
            outer_gap_bottom: og,
            ..Default::default()
        }
    }

    /// Create a workspace with per-side outer gap settings.
    /// Gap values are clamped to >= 0.
    pub fn with_directional_gaps(
        gap: i32,
        outer_gap_left: i32,
        outer_gap_right: i32,
        outer_gap_top: i32,
        outer_gap_bottom: i32,
    ) -> Self {
        Self {
            gap: gap.max(0),
            outer_gap_left: outer_gap_left.max(0),
            outer_gap_right: outer_gap_right.max(0),
            outer_gap_top: outer_gap_top.max(0),
            outer_gap_bottom: outer_gap_bottom.max(0),
            ..Default::default()
        }
    }

    /// Check if the workspace is empty.
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// Get the number of columns.
    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    /// Check if a window ID already exists in the workspace (tiled or floating).
    pub fn contains_window(&self, window_id: WindowId) -> bool {
        self.columns.iter().any(|c| c.windows.contains(&window_id))
            || self.floating_windows.iter().any(|f| f.id == window_id)
    }

    /// Check if a window is floating.
    pub fn is_floating(&self, window_id: WindowId) -> bool {
        self.floating_windows.iter().any(|f| f.id == window_id)
    }

    /// Get the number of floating windows.
    pub fn floating_count(&self) -> usize {
        self.floating_windows.len()
    }

    /// Add a floating window to the workspace.
    ///
    /// # Errors
    ///
    /// Returns `LayoutError::DuplicateWindow` if the window ID already exists.
    pub fn add_floating(&mut self, window_id: WindowId, rect: Rect) -> Result<(), LayoutError> {
        if self.contains_window(window_id) {
            return Err(LayoutError::DuplicateWindow(window_id));
        }

        self.floating_windows.push(FloatingWindow {
            id: window_id,
            rect,
            pinned: false,
        });
        Ok(())
    }

    /// Set whether a floating window stays visible above a fullscreen window.
    ///
    /// Returns true if the window was found, false otherwise.
    pub fn set_floating_pinned(&mut self, window_id: WindowId, pinned: bool) -> bool {
        if let Some(floating) = self.floating_windows.iter_mut().find(|f| f.id == window_id) {
            floating.pinned = pinned;
            true
        } else {
            false
        }
    }

    /// Remove a floating window from the workspace.
    ///
    /// Returns true if the window was found and removed, false otherwise.
    pub fn remove_floating(&mut self, window_id: WindowId) -> bool {
        if let Some(pos) = self.floating_windows.iter().position(|f| f.id == window_id) {
            self.floating_windows.remove(pos);
            self.window_min_widths.remove(&window_id);
            self.window_min_heights.remove(&window_id);
            self.pending_min_size_clears.remove(&window_id);
            self.minimized_windows.remove(&window_id);
            self.float_origin_column.remove(&window_id);
            if self.fullscreen_window == Some(window_id) {
                self.fullscreen_window = None;
            }
            true
        } else {
            false
        }
    }

    /// Update the position/size of a floating window.
    pub fn update_floating(&mut self, window_id: WindowId, rect: Rect) -> bool {
        if let Some(floating) = self.floating_windows.iter_mut().find(|f| f.id == window_id) {
            floating.rect = rect;
            true
        } else {
            false
        }
    }

    /// Clamp every floating rectangle inside a monitor work area.
    /// Returns whether any stored geometry changed.
    pub fn clamp_floating_to(&mut self, bounds: Rect) -> bool {
        let mut changed = false;
        for floating in &mut self.floating_windows {
            let clamped = floating.rect.clamped_inside(bounds);
            if clamped != floating.rect {
                floating.rect = clamped;
                changed = true;
            }
        }
        changed
    }

    /// Get the current rectangle for a floating window.
    pub fn floating_rect(&self, window_id: WindowId) -> Option<Rect> {
        self.floating_windows
            .iter()
            .find(|floating| floating.id == window_id)
            .map(|floating| floating.rect)
    }

    /// Get all floating windows.
    pub fn floating_windows(&self) -> &[FloatingWindow] {
        &self.floating_windows
    }

    /// Get the total width of the strip (sum of all column widths + gaps).
    ///
    /// Note: Negative gaps are treated as zero for calculation purposes.
    pub fn total_width(&self) -> i32 {
        self.total_width_for_effective_widths(&self.effective_column_widths())
    }

    /// Get the current scroll offset.
    pub fn scroll_offset(&self) -> f64 {
        self.scroll_offset
    }

    /// Get a slice of all columns.
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// Get a column by index (safe access).
    pub fn column(&self, index: usize) -> Option<&Column> {
        self.columns.get(index)
    }

    /// Find a window's location in the workspace.
    /// Returns (column_index, window_index_in_column) if found.
    pub fn find_window_location(&self, window_id: WindowId) -> Option<(usize, usize)> {
        for (col_idx, column) in self.columns.iter().enumerate() {
            if let Some(win_idx) = column.windows.iter().position(|&w| w == window_id) {
                return Some((col_idx, win_idx));
            }
        }
        None
    }

    /// Get total window count across all columns.
    pub fn window_count(&self) -> usize {
        self.columns.iter().map(|c| c.len()).sum()
    }

    /// Get all window IDs in this workspace (both tiled and floating).
    ///
    /// Useful for migrating windows when monitors are disconnected.
    pub fn all_window_ids(&self) -> Vec<WindowId> {
        let mut ids: Vec<WindowId> = self
            .columns
            .iter()
            .flat_map(|c| c.windows().iter().copied())
            .collect();
        ids.extend(self.floating_windows.iter().map(|f| f.id));
        ids
    }

    /// Get the gap between columns in pixels.
    pub fn gap(&self) -> i32 {
        self.gap
    }

    /// Set the gap between columns in pixels.
    /// Value is clamped to >= 0.
    pub fn set_gap(&mut self, gap: i32) {
        self.gap = gap.max(0);
    }

    /// Get outer gaps as (left, right, top, bottom).
    pub fn outer_gaps(&self) -> (i32, i32, i32, i32) {
        (
            self.outer_gap_left,
            self.outer_gap_right,
            self.outer_gap_top,
            self.outer_gap_bottom,
        )
    }

    /// Set the gap at viewport edges in pixels.
    /// Values are clamped to >= 0.
    pub fn set_outer_gaps(&mut self, left: i32, right: i32, top: i32, bottom: i32) {
        self.outer_gap_left = left.max(0);
        self.outer_gap_right = right.max(0);
        self.outer_gap_top = top.max(0);
        self.outer_gap_bottom = bottom.max(0);
    }

    /// Get the default width for new columns.
    pub fn default_column_width(&self) -> i32 {
        self.default_column_width
    }

    /// Set the default width for new columns.
    /// Value is clamped to >= MIN_COLUMN_WIDTH (100px).
    pub fn set_default_column_width(&mut self, width: i32) {
        self.default_column_width = width.max(MIN_COLUMN_WIDTH);
    }

    /// Get the pixels reserved at the top of Tabbed columns for the tab strip overlay.
    pub fn tab_strip_reserve_px(&self) -> i32 {
        self.tab_strip_reserve_px
    }

    /// Set the pixels reserved at the top of Tabbed columns for the tab strip overlay.
    /// Value is clamped to >= 0. Vertical columns ignore this value.
    pub fn set_tab_strip_reserve_px(&mut self, px: i32) {
        self.tab_strip_reserve_px = px.max(0);
    }

    /// Get the centering mode for focus changes.
    pub fn centering_mode(&self) -> CenteringMode {
        self.centering_mode
    }

    /// Set the centering mode for focus changes.
    pub fn set_centering_mode(&mut self, mode: CenteringMode) {
        self.centering_mode = mode;
    }

    /// Get whether scroll animations are skipped.
    pub fn reduce_motion(&self) -> bool {
        self.reduce_motion
    }

    /// Set whether to skip scroll animations (snap instantly).
    pub fn set_reduce_motion(&mut self, reduce: bool) {
        self.reduce_motion = reduce;
    }

    /// Set scroll animation duration and easing (from `[animation]` config).
    pub fn set_scroll_animation(&mut self, duration_ms: u64, easing: Easing) {
        self.scroll_duration_ms = duration_ms;
        self.scroll_easing = easing;
    }

    /// Set whether center-column can scroll past content edges.
    pub fn set_center_past_edges(&mut self, allow: bool) {
        self.center_past_edges = allow;
    }

    /// Calculate the x-coordinate of a column's left edge on the strip.
    ///
    /// Note: Negative gaps are treated as zero for calculation purposes.
    /// Check if a column has at least one non-minimized window.
    pub(crate) fn is_column_active(&self, column: &Column) -> bool {
        column
            .windows()
            .iter()
            .any(|w| !self.minimized_windows.contains(w))
    }
}
