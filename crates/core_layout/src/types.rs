use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Minimum width for columns in pixels.
pub(crate) const MIN_COLUMN_WIDTH: i32 = 100;

/// Default gap between columns in pixels.
pub const DEFAULT_GAP: i32 = 10;
/// Default outer gaps at viewport edges in pixels.
pub const DEFAULT_OUTER_GAP: i32 = 10;
/// Default width for new columns in pixels.
pub const DEFAULT_COLUMN_WIDTH: i32 = 800;

pub(crate) fn default_outer_gap_value() -> i32 {
    DEFAULT_OUTER_GAP
}

/// Unique identifier for a window.
/// On Windows, this will typically be the HWND cast to u64.
pub type WindowId = u64;

/// Width and height for a floating window, without a screen position.
///
/// Floating sizes are kept separately from [`Rect`] so callers can restore a
/// window on a different monitor without retaining stale absolute coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FloatingSize {
    /// Width in pixels (logical or physical according to the caller).
    pub width: i32,
    /// Height in pixels (logical or physical according to the caller).
    pub height: i32,
}

impl FloatingSize {
    /// Create a floating size with dimensions clamped to at least one pixel.
    pub fn new(width: i32, height: i32) -> Self {
        Self {
            width: width.max(1),
            height: height.max(1),
        }
    }
}

/// Errors that can occur during layout operations.
#[derive(Debug, Error)]
pub enum LayoutError {
    #[error("Column index {0} is out of bounds (max: {1})")]
    ColumnOutOfBounds(usize, usize),

    #[error("Window {0} not found in workspace")]
    WindowNotFound(WindowId),

    #[error("Window {0} already exists in workspace")]
    DuplicateWindow(WindowId),

    #[error("Window index {0} is out of bounds in column {1} (max: {2})")]
    WindowIndexOutOfBounds(usize, usize, usize),
}

/// A rectangle in screen coordinates (pixels).
///
/// Note: Fields are intentionally public for convenient read access.
/// Use `Rect::new()` to construct with dimension validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    /// Create a new rectangle.
    /// Width and height are clamped to >= 0 to prevent invalid dimensions.
    pub fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width: width.max(0),
            height: height.max(0),
        }
    }

    /// Check if this rectangle intersects with another.
    pub fn intersects(&self, other: &Rect) -> bool {
        let self_left = i64::from(self.x);
        let self_top = i64::from(self.y);
        let self_right = self_left + i64::from(self.width);
        let self_bottom = self_top + i64::from(self.height);
        let other_left = i64::from(other.x);
        let other_top = i64::from(other.y);
        let other_right = other_left + i64::from(other.width);
        let other_bottom = other_top + i64::from(other.height);

        self_left < other_right
            && self_right > other_left
            && self_top < other_bottom
            && self_bottom > other_top
    }

    /// Get the right edge x-coordinate.
    pub fn right(&self) -> i32 {
        self.x.saturating_add(self.width)
    }

    /// Get the bottom edge y-coordinate.
    pub fn bottom(&self) -> i32 {
        self.y.saturating_add(self.height)
    }

    /// Return this rectangle wholly contained by `bounds`.
    ///
    /// An oversized axis is shrunk before its origin is clamped; otherwise no
    /// position could expose both opposing edges at once. Bounds with a zero
    /// extent still produce a one-pixel rectangle anchored at their origin.
    pub fn clamped_inside(self, bounds: Rect) -> Rect {
        let bounds_width = bounds.width.max(1);
        let bounds_height = bounds.height.max(1);
        let width = self.width.max(1).min(bounds_width);
        let height = self.height.max(1).min(bounds_height);
        // Subtract the contained size before adding the remaining travel.
        // `(origin + bounds) - size` can saturate at i32::MAX first and then
        // fall below `origin`, which would invert the subsequent clamp range.
        let max_x = bounds.x.saturating_add(bounds_width.saturating_sub(width));
        let max_y = bounds
            .y
            .saturating_add(bounds_height.saturating_sub(height));
        Rect::new(
            self.x.clamp(bounds.x, max_x),
            self.y.clamp(bounds.y, max_y),
            width,
            height,
        )
    }
}

/// Center a requested floating size in a viewport, clamping it to the space
/// left after reserving `margin` pixels in total on each axis.
///
/// The margin is deliberately a total rather than a per-edge value: callers
/// preserve the existing 40px floating and 80px scratchpad compatibility
/// margins while keeping the resulting rectangle inside the work area.
pub fn centered_rect_for_size(viewport: Rect, requested_size: FloatingSize, margin: i32) -> Rect {
    let margin = margin.max(0);
    let max_width = viewport.width.saturating_sub(margin).max(1);
    let max_height = viewport.height.saturating_sub(margin).max(1);
    let width = requested_size.width.clamp(1, max_width);
    let height = requested_size.height.clamp(1, max_height);
    let x = viewport
        .x
        .saturating_add(viewport.width.saturating_sub(width) / 2);
    let y = viewport
        .y
        .saturating_add(viewport.height.saturating_sub(height) / 2);

    Rect::new(x, y, width, height)
}

/// Visibility state for layout computation.
/// Determines whether a window should be rendered or cloaked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Visibility {
    /// Window is within the viewport and should be rendered.
    Visible,
    /// Window is off-screen to the left of the viewport.
    OffScreenLeft,
    /// Window is off-screen to the right of the viewport.
    OffScreenRight,
}

/// Computed placement for a window.
/// Contains the target rectangle and visibility state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowPlacement {
    /// The window identifier.
    pub window_id: WindowId,
    /// The target rectangle in screen coordinates.
    pub rect: Rect,
    /// Whether the window is visible or off-screen.
    pub visibility: Visibility,
    /// The column index this window belongs to.
    pub column_index: usize,
}
