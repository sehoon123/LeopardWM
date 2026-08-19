from __future__ import annotations

import re
import shutil
from pathlib import Path

ROOT = Path.cwd()
CONTROL = ROOT.parent / "control" / ".github" / "v12"


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def write(path: str, text: str) -> None:
    (ROOT / path).write_text(text, encoding="utf-8", newline="\n")


def replace_once(path: str, old: str, new: str) -> None:
    text = read(path)
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: expected one occurrence, found {count}: {old[:120]!r}")
    write(path, text.replace(old, new))


def replace_section(path: str, start: str, end: str, replacement: str) -> None:
    text = read(path)
    start_at = text.find(start)
    end_at = text.find(end, start_at + len(start))
    if start_at < 0 or end_at < 0:
        raise RuntimeError(f"{path}: section markers missing")
    write(path, text[:start_at] + replacement + text[end_at:])


def insert_after_balanced_div(html: str, marker: str, block: str) -> str:
    marker_at = html.find(marker)
    if marker_at < 0:
        raise RuntimeError(f"settings marker missing: {marker}")
    start = html.rfind('<div class="setting-row">', 0, marker_at)
    if start < 0:
        raise RuntimeError("settings row start missing")
    token = re.compile(r"<div\b|</div>", re.IGNORECASE)
    depth = 0
    for match in token.finditer(html, start):
        if match.group(0).lower().startswith("<div"):
            depth += 1
        else:
            depth -= 1
            if depth == 0:
                return html[: match.end()] + "\n" + block + html[match.end() :]
    raise RuntimeError("settings row is not balanced")


def insert_state_field() -> None:
    path = "crates/daemon/src/state.rs"
    lines = read(path).splitlines()
    field_hits = [
        i
        for i, line in enumerate(lines)
        if "last_placed_layout_rects:" in line and "new()" not in line
    ]
    init_hits = [
        i
        for i, line in enumerate(lines)
        if "last_placed_layout_rects:" in line and "new()" in line
    ]
    if len(field_hits) != 1 or len(init_hits) != 1:
        raise RuntimeError(
            f"state.rs: field hits={field_hits}, initializer hits={init_hits}"
        )
    field = field_hits[0]
    indent = lines[field][: len(lines[field]) - len(lines[field].lstrip())]
    lines.insert(
        field + 1,
        indent
        + "pub(crate) last_region_clip_specs: Vec<leopardwm_platform_win32::WindowRegionClipSpec>,",
    )
    # Insertion before initializer shifts its original index by one.
    init = init_hits[0] + 1
    indent = lines[init][: len(lines[init]) - len(lines[init].lstrip())]
    lines.insert(init + 1, indent + "last_region_clip_specs: Vec::new(),")
    write(path, "\n".join(lines) + "\n")


def insert_drag_region_restore() -> None:
    candidates = [
        "crates/daemon/src/event_handler.rs",
        "crates/daemon/src/drag.rs",
    ]
    matches: list[tuple[str, re.Match[str]]] = []
    pattern = re.compile(
        r"(?m)^(\s*(?:pub\(crate\)\s+)?fn\s+\w*move[_]?size[_]?start\w*\s*\([^\)]*\bhwnd\b[^\)]*\)[^{]*\{)"
    )
    for path in candidates:
        text = read(path)
        for match in pattern.finditer(text):
            matches.append((path, match))
    if len(matches) != 1:
        raise RuntimeError(f"expected one move-size-start handler, found {[(p, m.group(1)) for p, m in matches]}")
    path, match = matches[0]
    text = read(path)
    indent = re.match(r"\s*", match.group(1)).group(0) + "    "
    insertion = (
        match.group(1)
        + "\n"
        + indent
        + "let _ = leopardwm_platform_win32::restore_window_region(hwnd, true);"
    )
    write(path, text[: match.start(1)] + insertion + text[match.end(1) :])


# ---------------------------------------------------------------------------
# Win32 region ownership module and public API
# ---------------------------------------------------------------------------
shutil.copyfile(CONTROL / "window_region.rs", ROOT / "crates/platform_win32/src/window_region.rs")
replace_once(
    "crates/platform_win32/src/lib.rs",
    "mod window_style;\n",
    "mod window_style;\nmod window_region;\n",
)
replace_once(
    "crates/platform_win32/src/lib.rs",
    "pub use window_style::{\n",
    "pub use window_region::{forget_window_region, restore_all_window_regions, restore_window_region};\npub use window_style::{\n",
)

# ---------------------------------------------------------------------------
# Platform configuration carries per-HWND clip and fail-safe geometry.
# ---------------------------------------------------------------------------
replace_once(
    "crates/platform_win32/src/types.rs",
    "use leopardwm_core_layout::{Rect, WindowId};",
    "use leopardwm_core_layout::{Rect, Visibility, WindowId};",
)
replace_once(
    "crates/platform_win32/src/types.rs",
    """/// Configuration for the Win32 platform layer.
#[derive(Debug, Clone, Default)]
pub struct PlatformConfig {
    pub animation_placement_policy: AnimationPlacementPolicy,
}
""",
    """/// Region clip requested for one tiled HWND in the current placement batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowRegionClipSpec {
    pub window_id: WindowId,
    pub clip_bounds: Rect,
    /// Preferred fallback: focused windows remain contained and visible.
    pub fallback_rect: Rect,
    pub fallback_visibility: Visibility,
    /// Last-resort fallback when a target rejects the preferred geometry.
    pub safe_fallback_rect: Rect,
    pub safe_fallback_visibility: Visibility,
}

/// Configuration for the Win32 platform layer.
#[derive(Debug, Clone, Default)]
pub struct PlatformConfig {
    pub animation_placement_policy: AnimationPlacementPolicy,
    pub region_clips: Vec<WindowRegionClipSpec>,
}
""",
)
replace_once(
    "crates/platform_win32/src/lib.rs",
    "AnimationPlacementPolicy, MonitorId, MonitorInfo, PlatformConfig, Win32Error, WindowInfo,\n",
    "AnimationPlacementPolicy, MonitorId, MonitorInfo, PlatformConfig, Win32Error, WindowInfo,\n    WindowRegionClipSpec,\n",
)

# ---------------------------------------------------------------------------
# User configuration and serialization.
# ---------------------------------------------------------------------------
replace_once(
    "crates/daemon/src/config.rs",
    """    #[serde(default = "default_false")]
    pub center_past_edges: bool,

    /// Width presets for cycling (fractions of usable viewport width).
""",
    """    #[serde(default = "default_false")]
    pub center_past_edges: bool,

    /// How tiled windows crossing a physical monitor edge are rendered.
    #[serde(default)]
    pub monitor_overflow: MonitorOverflowConfig,

    /// Width presets for cycling (fractions of usable viewport width).
""",
)
replace_once(
    "crates/daemon/src/config.rs",
    """            centering_mode: CenteringModeConfig::default(),
            center_past_edges: false,
            width_presets: default_width_presets(),
""",
    """            centering_mode: CenteringModeConfig::default(),
            center_past_edges: false,
            monitor_overflow: MonitorOverflowConfig::default(),
            width_presets: default_width_presets(),
""",
)
replace_once(
    "crates/daemon/src/config.rs",
    """impl From<CenteringModeConfig> for CenteringMode {
    fn from(config: CenteringModeConfig) -> Self {
        match config {
            CenteringModeConfig::Center => CenteringMode::Center,
            CenteringModeConfig::JustInView => CenteringMode::JustInView,
            CenteringModeConfig::OnOverflow => CenteringMode::OnOverflow,
        }
    }
}

/// Appearance-related configuration.
""",
    """impl From<CenteringModeConfig> for CenteringMode {
    fn from(config: CenteringModeConfig) -> Self {
        match config {
            CenteringModeConfig::Center => CenteringMode::Center,
            CenteringModeConfig::JustInView => CenteringMode::JustInView,
            CenteringModeConfig::OnOverflow => CenteringMode::OnOverflow,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MonitorOverflowConfig {
    /// Preserve partial neighbor previews and clip only pixels crossing the
    /// owning physical monitor. Unsafe targets use the whole-window fallback.
    #[default]
    Clip,
    /// Hide the tiled window as a unit when it crosses a neighboring monitor.
    Hide,
}

/// Appearance-related configuration.
""",
)
with (ROOT / "crates/daemon/src/config.rs").open("a", encoding="utf-8", newline="\n") as file:
    file.write(
        """

#[cfg(test)]
mod monitor_overflow_config_tests {
    use super::{Config, MonitorOverflowConfig};

    #[test]
    fn monitor_overflow_defaults_to_clip_and_round_trips() {
        let defaulted: Config = toml::from_str("[layout]\n").unwrap();
        assert_eq!(defaulted.layout.monitor_overflow, MonitorOverflowConfig::Clip);

        for value in [MonitorOverflowConfig::Clip, MonitorOverflowConfig::Hide] {
            let mut config = Config::default();
            config.layout.monitor_overflow = value;
            let encoded = toml::to_string(&config).unwrap();
            let decoded: Config = toml::from_str(&encoded).unwrap();
            assert_eq!(decoded.layout.monitor_overflow, value);
        }
    }
}
"""
    )

# ---------------------------------------------------------------------------
# Daemon policy: preserve scrolling geometry and emit clip specifications.
# ---------------------------------------------------------------------------
policy = r'''/// Reconcile tiled placements against the physical owning monitor.
/// Clip mode preserves partial horizontal previews. Hide mode, vertical
/// crossings, and unsafe Win32 targets use the existing whole-window fallback.
fn clamp_horizontally_inside(
    rect: leopardwm_core_layout::Rect,
    bounds: leopardwm_core_layout::Rect,
) -> leopardwm_core_layout::Rect {
    let width = rect.width.max(1).min(bounds.width.max(1));
    let max_x = bounds
        .x
        .saturating_add(bounds.width.max(1).saturating_sub(width));
    leopardwm_core_layout::Rect::new(rect.x.clamp(bounds.x, max_x), rect.y, width, rect.height)
}

fn upsert_region_clip(
    clips: &mut Vec<leopardwm_platform_win32::WindowRegionClipSpec>,
    clip: leopardwm_platform_win32::WindowRegionClipSpec,
) {
    if let Some(existing) = clips.iter_mut().find(|item| item.window_id == clip.window_id) {
        *existing = clip;
    } else {
        clips.push(clip);
    }
}

fn park_offscreen_avoiding_neighbors(
    placements: &mut [leopardwm_core_layout::WindowPlacement],
    owner_id: leopardwm_platform_win32::MonitorId,
    focused_column: Option<usize>,
    mode: crate::config::MonitorOverflowConfig,
    monitors: &std::collections::HashMap<
        leopardwm_platform_win32::MonitorId,
        leopardwm_platform_win32::MonitorInfo,
    >,
    monitor_rects: &[leopardwm_core_layout::Rect],
    region_clips: &mut Vec<leopardwm_platform_win32::WindowRegionClipSpec>,
) {
    use crate::config::MonitorOverflowConfig;
    use leopardwm_core_layout::Visibility;

    let Some(owner) = monitors.get(&owner_id) else {
        return;
    };
    let owner_rect = owner.rect;
    let intersects_neighbor = |rect: leopardwm_core_layout::Rect| {
        monitors
            .iter()
            .filter(|(id, monitor)| **id != owner_id && monitor.rect != owner_rect)
            .any(|(_, monitor)| rect.intersects(&monitor.rect))
    };

    for placement in placements {
        if !intersects_neighbor(placement.rect) {
            continue;
        }
        if placement.visibility != Visibility::Visible {
            placement.rect = offscreen_park_rect(placement.rect, owner_rect, monitor_rects);
            continue;
        }

        let crosses_horizontal =
            placement.rect.x < owner_rect.x || placement.rect.right() > owner_rect.right();
        let crosses_vertical =
            placement.rect.y < owner_rect.y || placement.rect.bottom() > owner_rect.bottom();
        if placement.column_index == usize::MAX || (!crosses_horizontal && !crosses_vertical) {
            continue;
        }

        let safe_rect = offscreen_park_rect(placement.rect, owner_rect, monitor_rects);
        let safe_visibility = if placement.rect.x < owner_rect.x {
            Visibility::OffScreenLeft
        } else {
            Visibility::OffScreenRight
        };
        let (fallback_rect, fallback_visibility) =
            if focused_column == Some(placement.column_index) && crosses_horizontal {
                (
                    clamp_horizontally_inside(placement.rect, owner.work_area),
                    Visibility::Visible,
                )
            } else {
                (safe_rect, safe_visibility)
            };

        if mode == MonitorOverflowConfig::Clip && crosses_horizontal && !crosses_vertical {
            upsert_region_clip(
                region_clips,
                leopardwm_platform_win32::WindowRegionClipSpec {
                    window_id: placement.window_id,
                    clip_bounds: owner_rect,
                    fallback_rect,
                    fallback_visibility,
                    safe_fallback_rect: safe_rect,
                    safe_fallback_visibility: safe_visibility,
                },
            );
        } else {
            placement.rect = fallback_rect;
            placement.visibility = fallback_visibility;
        }
    }
}

'''
replace_section(
    "crates/daemon/src/layout_apply.rs",
    "/// Keep tiled placements isolated to their owning monitor.",
    "/// Pick an off-screen rect for `window`",
    policy,
)

layout_path = "crates/daemon/src/layout_apply.rs"
layout = read(layout_path)
start = layout.index("    pub(crate) fn send_animation_frame(")
end = layout.index("    fn collect_layout_apply_timeout_candidates", start)
section = layout[start:end]
section = section.replace(
    "        let mut all_placements = Vec::new();\n",
    "        let mut all_placements = Vec::new();\n        let mut region_clips = Vec::new();\n",
    1,
)
section = section.replace(
    """                focused_column,
                &self.monitors,
                &monitor_rects,
            );""",
    """                focused_column,
                self.config.layout.monitor_overflow,
                &self.monitors,
                &monitor_rects,
                &mut region_clips,
            );""",
)
section = section.replace(
    """                    None,
                    &self.monitors,
                    &monitor_rects,
                );""",
    """                    None,
                    self.config.layout.monitor_overflow,
                    &self.monitors,
                    &monitor_rects,
                    &mut region_clips,
                );""",
)
section = section.replace(
    "        let mut platform_config = self.platform_config.clone();\n",
    "        let mut platform_config = self.platform_config.clone();\n        platform_config.region_clips = region_clips;\n",
    1,
)
layout = layout[:start] + section + layout[end:]
write(layout_path, layout)

collect_start = "    /// Collect animated placements for every monitor's active workspace, with debug logging.\n"
collect_end = "    /// Fast-path check: every placement matches the last applied rect and the visible-set is unchanged.\n"
collect = r'''    /// Collect placements and partial-window region requests for every monitor.
    fn collect_apply_placements(
        &self,
    ) -> (
        Vec<leopardwm_core_layout::WindowPlacement>,
        Vec<leopardwm_platform_win32::WindowRegionClipSpec>,
    ) {
        let mut all_placements = Vec::new();
        let mut region_clips = Vec::new();
        let monitor_rects: Vec<_> = self.monitors.values().map(|monitor| monitor.rect).collect();

        for (monitor_id, ws_vec) in &self.workspaces {
            let idx = self.active_workspace_idx(*monitor_id);
            if let Some(workspace) = ws_vec.get(idx) {
                if self.monitors.contains_key(monitor_id) {
                    let viewport = self.layout_viewport(*monitor_id);
                    let mut placements = workspace.compute_placements_animated(viewport);
                    let focused_column = (*monitor_id == self.focused_monitor)
                        .then(|| workspace.focused_column_index());
                    park_offscreen_avoiding_neighbors(
                        &mut placements,
                        *monitor_id,
                        focused_column,
                        self.config.layout.monitor_overflow,
                        &self.monitors,
                        &monitor_rects,
                        &mut region_clips,
                    );
                    debug!(
                        "Monitor {}: {} placements for viewport {}x{} (animating: {}, scroll: {:.1}, minimized: {})",
                        monitor_id,
                        placements.len(),
                        viewport.width,
                        viewport.height,
                        workspace.is_animating(),
                        workspace.effective_scroll_offset(),
                        workspace.minimized_count()
                    );
                    all_placements.extend(placements);
                }
            }
        }

        (all_placements, region_clips)
    }

'''
replace_section(layout_path, collect_start, collect_end, collect)
replace_once(
    layout_path,
    "        let mut all_placements = self.collect_apply_placements();\n",
    "        let (mut all_placements, region_clips) = self.collect_apply_placements();\n",
)
replace_once(
    layout_path,
    "        let placements_unchanged = self.placements_match_last_applied(&all_placements);\n",
    """        let region_clips_unchanged = self.last_region_clip_specs == region_clips;
        let placements_unchanged = self.placements_match_last_applied(&all_placements)
            && region_clips_unchanged;
""",
)
replace_once(
    layout_path,
    "        let (rx, worker_handle) = match self.spawn_apply_worker(all_placements) {\n",
    """        let applied_region_clips = region_clips.clone();
        let (rx, worker_handle) = match self.spawn_apply_worker(all_placements, region_clips) {
""",
)
replace_once(
    layout_path,
    """    fn spawn_apply_worker(
        &mut self,
        all_placements: Vec<leopardwm_core_layout::WindowPlacement>,
    ) -> Result<(
""",
    """    fn spawn_apply_worker(
        &mut self,
        all_placements: Vec<leopardwm_core_layout::WindowPlacement>,
        region_clips: Vec<leopardwm_platform_win32::WindowRegionClipSpec>,
    ) -> Result<(
""",
)
replace_once(
    layout_path,
    "        let platform_config = self.platform_config.clone();\n",
    "        let mut platform_config = self.platform_config.clone();\n        platform_config.region_clips = region_clips;\n",
)
replace_once(
    layout_path,
    """    pub(crate) fn begin_shutdown_or_revert(&mut self) -> Vec<std::thread::JoinHandle<()>> {
        self.apply_worker_cancelled.store(true, Ordering::SeqCst);
""",
    """    pub(crate) fn begin_shutdown_or_revert(&mut self) -> Vec<std::thread::JoinHandle<()>> {
        leopardwm_platform_win32::restore_all_window_regions();
        self.apply_worker_cancelled.store(true, Ordering::SeqCst);
""",
)

# Store region specs only after a successful placement batch; invalidate on failure.
layout = read(layout_path)
apply_start = layout.index("    pub(crate) fn apply_layout(&mut self) -> Result<()> {")
apply_end = layout.index("    /// Collect placements and partial-window region requests", apply_start)
apply_section = layout[apply_start:apply_end]
marker = "        if result.is_err() {\n"
if apply_section.count(marker) != 1:
    raise RuntimeError("apply_layout result marker mismatch")
apply_section = apply_section.replace(
    marker,
    """        if result.is_ok() {
            self.last_region_clip_specs = applied_region_clips;
        } else {
            self.last_region_clip_specs.clear();
        }
        if result.is_err() {
""",
)
layout = layout[:apply_start] + apply_section + layout[apply_end:]
write(layout_path, layout)

# Existing direct policy tests continue to exercise the conservative hide mode.
layout = read(layout_path)
layout = layout.replace(
    "park_offscreen_avoiding_neighbors(&mut placements, 1, None, &monitors, &monitor_rects);",
    "park_offscreen_avoiding_neighbors(\n            &mut placements,\n            1,\n            None,\n            crate::config::MonitorOverflowConfig::Hide,\n            &monitors,\n            &monitor_rects,\n            &mut Vec::new(),\n        );",
)
layout = layout.replace(
    "park_offscreen_avoiding_neighbors(placements, owner_id, None, &monitors, &rects);",
    "park_offscreen_avoiding_neighbors(\n            placements,\n            owner_id,\n            None,\n            crate::config::MonitorOverflowConfig::Hide,\n            &monitors,\n            &rects,\n            &mut Vec::new(),\n        );",
)
write(layout_path, layout)

insert_state_field()
insert_drag_region_restore()

# ---------------------------------------------------------------------------
# Settings GUI load/save wiring.
# ---------------------------------------------------------------------------
settings_path = "crates/daemon/src/settings/settings.html"
settings = read(settings_path)
row = '''        <div class="setting-row">
          <div class="setting-info">
            <div class="setting-title">Monitor overflow</div>
            <div class="setting-description">Clip tiled previews at the owning monitor edge, or hide the whole window as a compatibility fallback.</div>
          </div>
          <div class="setting-control">
            <select id="layout-monitor_overflow">
              <option value="clip">Clip at monitor edge</option>
              <option value="hide">Hide whole window</option>
            </select>
          </div>
        </div>'''
settings = insert_after_balanced_div(settings, 'id="layout-center_past_edges"', row)
load_marker = "setChecked('layout-center_past_edges', cfg.layout.center_past_edges);"
if settings.count(load_marker) != 1:
    raise RuntimeError("settings load marker mismatch")
settings = settings.replace(
    load_marker,
    load_marker
    + "\n      document.getElementById('layout-monitor_overflow').value = cfg.layout.monitor_overflow || 'clip';",
)
save_marker = "center_past_edges: checked('layout-center_past_edges'),"
if settings.count(save_marker) != 1:
    raise RuntimeError("settings save marker mismatch")
settings = settings.replace(
    save_marker,
    save_marker
    + "\n          monitor_overflow: document.getElementById('layout-monitor_overflow').value,",
)
write(settings_path, settings)
with (ROOT / "crates/daemon/src/settings/html.rs").open("a", encoding="utf-8", newline="\n") as file:
    file.write(
        """

#[cfg(test)]
mod monitor_overflow_settings_tests {
    use super::SETTINGS_HTML;

    #[test]
    fn monitor_overflow_control_is_present_and_round_trip_wired() {
        for marker in [
            "id=\"layout-monitor_overflow\"",
            "value=\"clip\"",
            "value=\"hide\"",
            "cfg.layout.monitor_overflow || 'clip'",
            "monitor_overflow: document.getElementById('layout-monitor_overflow').value",
        ] {
            assert!(SETTINGS_HTML.contains(marker), "missing Settings marker: {marker}");
        }
    }
}
"""
    )

print("SetWindowRgn v12 source patch applied")
