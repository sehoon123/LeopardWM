from pathlib import Path

ROOT = Path('.')


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding='utf-8')


def write(path: str, text: str) -> None:
    (ROOT / path).write_text(text, encoding='utf-8', newline='\n')


def replace_section(path: str, start: str, end: str, replacement: str) -> None:
    text = read(path)
    start_at = text.find(start)
    end_at = text.find(end, start_at + len(start))
    if start_at < 0 or end_at < 0:
        raise RuntimeError(f'{path}: section markers not found: {start!r} .. {end!r}')
    write(path, text[:start_at] + replacement + text[end_at:])


operations = 'crates/core_layout/src/workspace/operations.rs'
text = read(operations)
import_marker = 'use crate::workspace::Workspace;\n'
if text.count(import_marker) != 1:
    raise RuntimeError('operations.rs: import marker mismatch')
text = text.replace(
    import_marker,
    import_marker
    + '''\n#[derive(Debug, Clone, Copy)]\nstruct FocusScrollTarget {\n    offset: f64,\n    allow_edge_overscroll: bool,\n}\n''',
    1,
)
write(operations, text)

replace_section(
    operations,
    '    fn column_x(&self, column_index: usize) -> i32 {',
    '    /// Enforce the scroll bounds for the current content and focus.',
    '''    fn column_x(&self, column_index: usize) -> i32 {\n        self.column_x_with_minimized_handling(column_index, true)\n    }\n\n    /// Compute the X position of a column, optionally skipping minimized columns.\n    fn column_x_with_minimized_handling(&self, column_index: usize, skip_minimized: bool) -> i32 {\n        let gap = self.gap.max(0);\n        let mut x = 0;\n        for (index, column) in self.columns.iter().enumerate() {\n            if index == column_index {\n                return x;\n            }\n            if skip_minimized && !self.is_column_active(column) {\n                continue;\n            }\n            x = x.saturating_add(column.width).saturating_add(gap);\n        }\n        x\n    }\n\n    fn focused_column_bounds(&self) -> Option<(i32, i32)> {\n        self.columns.get(self.focused_column).map(|column| {\n            (self.column_x(self.focused_column), column.width)\n        })\n    }\n\n    /// Width available to the scrolling strip after horizontal outer gaps.\n    pub(crate) fn visible_width(&self, viewport_width: i32) -> i32 {\n        viewport_width\n            .saturating_sub(self.outer_gap_left.max(0))\n            .saturating_sub(self.outer_gap_right.max(0))\n            .max(0)\n    }\n\n    fn should_center(&self, column_width: i32, visible_width: i32) -> bool {\n        match self.centering_mode {\n            CenteringMode::Center => true,\n            CenteringMode::JustInView => false,\n            CenteringMode::OnOverflow => column_width > visible_width,\n        }\n    }\n\n    fn centered_scroll_target(column_x: i32, column_width: i32, visible_width: i32) -> f64 {\n        f64::from(column_x) + f64::from(column_width.max(0)) / 2.0\n            - f64::from(visible_width.max(0)) / 2.0\n    }\n\n    /// Return the closest offset that fully exposes a fitting column. For an\n    /// oversized column, keep the viewport inside the column with the smallest\n    /// possible movement instead of snapping arbitrarily to either edge.\n    fn nearest_visible_scroll_target(\n        current: f64,\n        column_x: i32,\n        column_width: i32,\n        visible_width: i32,\n    ) -> f64 {\n        if visible_width <= 0 {\n            return 0.0;\n        }\n        let current = if current.is_finite() { current } else { 0.0 };\n        let left = f64::from(column_x);\n        let width = f64::from(column_width.max(0));\n        let right = left + width;\n        let viewport = f64::from(visible_width);\n\n        let (minimum, maximum) = if width <= viewport {\n            // Every offset in this interval keeps the complete column visible.\n            (right - viewport, left)\n        } else {\n            // The column cannot fit. Every offset in this interval keeps the\n            // complete viewport inside it, maximizing visible focused content.\n            (left, right - viewport)\n        };\n        current.clamp(minimum.min(maximum), minimum.max(maximum))\n    }\n\n    fn focus_scroll_target(\n        &self,\n        current: f64,\n        column_x: i32,\n        column_width: i32,\n        visible_width: i32,\n    ) -> FocusScrollTarget {\n        if self.should_center(column_width, visible_width) {\n            FocusScrollTarget {\n                offset: Self::centered_scroll_target(\n                    column_x,\n                    column_width,\n                    visible_width,\n                ),\n                allow_edge_overscroll: self.center_past_edges,\n            }\n        } else {\n            FocusScrollTarget {\n                offset: Self::nearest_visible_scroll_target(\n                    current,\n                    column_x,\n                    column_width,\n                    visible_width,\n                ),\n                allow_edge_overscroll: false,\n            }\n        }\n    }\n\n    fn normal_scroll_bounds(&self, visible_width: i32) -> (f64, f64) {\n        let maximum = self.total_width().saturating_sub(visible_width).max(0);\n        (0.0, f64::from(maximum))\n    }\n\n    fn scroll_bounds_for_focus(\n        &self,\n        visible_width: i32,\n        allow_edge_overscroll: bool,\n    ) -> (f64, f64) {\n        let normal = self.normal_scroll_bounds(visible_width);\n        if !allow_edge_overscroll || !self.center_past_edges {\n            return normal;\n        }\n\n        let Some((column_x, column_width)) = self.focused_column_bounds() else {\n            return normal;\n        };\n        let centered =\n            Self::centered_scroll_target(column_x, column_width, visible_width);\n        // For a strip shorter than the viewport, a middle column can also\n        // require edge overscroll to reach the center. Extend only as far as\n        // this focus's exact centered target; arbitrary blank-space scrolling\n        // remains impossible.\n        (normal.0.min(centered), normal.1.max(centered))\n    }\n\n    /// Bounds used by render-time repair. Explicit center-column commands are\n    /// preserved while their exact target still matches the current geometry.\n    fn focused_scroll_bounds(&self, visible_width: i32) -> (f64, f64) {\n        let Some((column_x, column_width)) = self.focused_column_bounds() else {\n            return self.normal_scroll_bounds(visible_width);\n        };\n        let centered =\n            Self::centered_scroll_target(column_x, column_width, visible_width);\n        let explicitly_centered = (self.scroll_offset - centered).abs() < 0.5;\n        self.scroll_bounds_for_focus(\n            visible_width,\n            self.should_center(column_width, visible_width) || explicitly_centered,\n        )\n    }\n\n    /// Adjust the viewport according to the configured focus policy.\n    pub fn ensure_focused_visible(&mut self, viewport_width: i32) {\n        let Some((column_x, column_width)) = self.focused_column_bounds() else {\n            return;\n        };\n        let visible_width = self.visible_width(viewport_width);\n        let target = self.focus_scroll_target(\n            self.scroll_offset,\n            column_x,\n            column_width,\n            visible_width,\n        );\n        let bounds =\n            self.scroll_bounds_for_focus(visible_width, target.allow_edge_overscroll);\n        self.scroll_offset = target.offset.clamp(bounds.0, bounds.1);\n    }\n\n''',
)

replace_section(
    operations,
    '    /// Start an animated scroll to a target offset.',
    '    /// Advance the active animation by the given delta time in milliseconds.',
    '''    fn start_scroll_animation_to(\n        &mut self,\n        target: f64,\n        duration_ms: Option<u64>,\n        easing: Option<Easing>,\n    ) {\n        let target = if target.is_finite() { target } else { 0.0 };\n        let current = self.effective_scroll_offset();\n        let start = if current.is_finite() { current } else { 0.0 };\n        if (start - target).abs() < 0.5 {\n            self.scroll_offset = target;\n            self.active_animation = None;\n            return;\n        }\n\n        self.active_animation = Some(ScrollAnimation::new(\n            start,\n            target,\n            duration_ms.unwrap_or(self.scroll_duration_ms),\n            easing.unwrap_or(self.scroll_easing),\n        ));\n    }\n\n    /// Start an animated scroll to a target offset. Public callers retain the\n    /// current focus-aware bounds; focus navigation uses the more precise plan\n    /// produced by `focus_scroll_target`.\n    pub fn start_scroll_animation(\n        &mut self,\n        target: f64,\n        viewport_width: i32,\n        duration_ms: Option<u64>,\n        easing: Option<Easing>,\n    ) {\n        let visible_width = self.visible_width(viewport_width);\n        let bounds = self.focused_scroll_bounds(visible_width);\n        let target = if target.is_finite() { target } else { 0.0 };\n        self.start_scroll_animation_to(target.clamp(bounds.0, bounds.1), duration_ms, easing);\n    }\n\n''',
)

replace_section(
    operations,
    '    /// Ensure the focused column is visible with animation.',
    '    /// Center the focused column in the viewport, regardless of centering mode.',
    '''    /// Ensure the focused column is visible with animation. The same pure\n    /// target calculation is used by both animated and reduced-motion paths.\n    pub fn ensure_focused_visible_animated(&mut self, viewport_width: i32) {\n        if self.reduce_motion {\n            self.stop_animation();\n            self.ensure_focused_visible(viewport_width);\n            return;\n        }\n\n        let Some((column_x, column_width)) = self.focused_column_bounds() else {\n            return;\n        };\n        let visible_width = self.visible_width(viewport_width);\n        let target = self.focus_scroll_target(\n            self.effective_scroll_offset(),\n            column_x,\n            column_width,\n            visible_width,\n        );\n        let bounds =\n            self.scroll_bounds_for_focus(visible_width, target.allow_edge_overscroll);\n        self.start_scroll_animation_to(target.offset.clamp(bounds.0, bounds.1), None, None);\n    }\n\n''',
)

replace_section(
    operations,
    '    /// Center the focused column in the viewport, regardless of centering mode.',
    '#[cfg(test)]\nmod edge_centering_tests',
    '''    /// Center the focused column in the viewport, regardless of centering mode.\n    pub fn center_focused_column_animated(&mut self, viewport_width: i32) {\n        let Some((column_x, column_width)) = self.focused_column_bounds() else {\n            return;\n        };\n        let visible_width = self.visible_width(viewport_width);\n        let target = Self::centered_scroll_target(column_x, column_width, visible_width);\n        let bounds = self.scroll_bounds_for_focus(visible_width, true);\n        let target = target.clamp(bounds.0, bounds.1);\n\n        if self.reduce_motion {\n            self.stop_animation();\n            self.scroll_offset = target;\n        } else {\n            self.start_scroll_animation_to(target, None, None);\n        }\n    }\n}\n\n''',
)

# Clarify public mode semantics without changing serialization or defaults.
mod_path = 'crates/core_layout/src/workspace/mod.rs'
mod_text = read(mod_path)
old_docs = '''    /// Only scroll if the focused column would be outside the viewport.\n    JustInView,\n    /// Center only when the focused column is wider than the viewport (so it\n    /// cannot fit otherwise); behave like `JustInView` for columns that fit.\n    OnOverflow,\n'''
new_docs = '''    /// Move by the minimum distance needed to fully expose a fitting column.\n    /// For an oversized column, keep the viewport inside it without an\n    /// unnecessary edge snap.\n    JustInView,\n    /// Behave like `JustInView` for fitting columns and center only when the\n    /// focused column is wider than the viewport.\n    OnOverflow,\n'''
if mod_text.count(old_docs) != 1:
    raise RuntimeError('workspace/mod.rs: centering-mode docs mismatch')
write(mod_path, mod_text.replace(old_docs, new_docs, 1))

print('focus-scroll mode refactor applied')
