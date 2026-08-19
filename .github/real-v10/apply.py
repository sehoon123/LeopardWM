from __future__ import annotations

import re
import shutil
from pathlib import Path

ROOT = Path.cwd()
CONTROL = ROOT.parent / "control" / ".github" / "real-v10"


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def write(path: str, text: str) -> None:
    (ROOT / path).write_text(text, encoding="utf-8", newline="\n")


def replace_once(path: str, old: str, new: str) -> None:
    text = read(path)
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: expected one occurrence, found {count}: {old[:100]!r}")
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


# ---------------------------------------------------------------------------
# Win32 region module and public API
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
    "pub use window_region::{forget_window_region, restore_all_window_regions};\npub use window_style::{\n",
)

# ---------------------------------------------------------------------------
# Dynamic clip specifications carried with each placement batch
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
/// `fallback_*` is used when the target has an application-owned region or a
/// SetWindowRgn operation fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowRegionClipSpec {
    pub window_id: WindowId,
    pub clip_bounds: Rect,
    pub fallback_rect: Rect,
    pub fallback_visibility: Visibility,
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
# User configuration
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
    /// Preserve partial neighbor previews and clip only the pixels crossing the
    /// owning monitor. Unsupported/custom-region windows use the safe fallback.
    #[default]
    Clip,
    /// Hide a tiled window as a unit when it crosses into another monitor.
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
# Daemon monitor-overflow policy
# ---------------------------------------------------------------------------
policy = r'''/// Reconcile a tiled placement against its owning physical monitor.
///
/// Clip mode preserves partial previews and emits a platform region request.
/// Hide mode, vertical overlap, and unsupported-region fallback use the existing
/// whole-window park/contain behavior. Floating windows are never constrained.
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
            .filter(|(id, _)| **id != owner_id)
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

        let mut fallback = placement.clone();
        if focused_column == Some(placement.column_index) && crosses_horizontal {
            fallback.rect = clamp_horizontally_inside(fallback.rect, owner.work_area);
        } else {
            fallback.visibility = if fallback.rect.x < owner_rect.x {
                Visibility::OffScreenLeft
            } else {
                Visibility::OffScreenRight
            };
            fallback.rect = offscreen_park_rect(fallback.rect, owner_rect, monitor_rects);
        }

        if mode == MonitorOverflowConfig::Clip && crosses_horizontal && !crosses_vertical {
            upsert_region_clip(
                region_clips,
                leopardwm_platform_win32::WindowRegionClipSpec {
                    window_id: placement.window_id,
                    clip_bounds: owner_rect,
                    fallback_rect: fallback.rect,
                    fallback_visibility: fallback.visibility,
                },
            );
        } else {
            placement.rect = fallback.rect;
            placement.visibility = fallback.visibility;
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

# Scope modifications to send_animation_frame.
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

# Replace the collection helper as one coherent unit.
collect_start = "    /// Collect animated placements for every monitor's active workspace, with debug logging.\n"
collect_end = "    /// Fast-path check: every placement matches the last applied rect and the visible-set is unchanged.\n"
collect = r'''    /// Collect placements and the region requests associated with partial tiled windows.
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
    "        let (rx, worker_handle) = match self.spawn_apply_worker(all_placements) {\n",
    "        let (rx, worker_handle) = match self.spawn_apply_worker(all_placements, region_clips) {\n",
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

# Existing tests call the policy helper directly; retain hide-mode behavior.
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

# ---------------------------------------------------------------------------
# Platform placement integration
# ---------------------------------------------------------------------------
replace_once(
    "crates/platform_win32/src/placement.rs",
    "use crate::types::{AnimationPlacementPolicy, PlatformConfig, Win32Error};",
    "use crate::types::{AnimationPlacementPolicy, PlatformConfig, Win32Error, WindowRegionClipSpec};",
)
replace_once(
    "crates/platform_win32/src/placement.rs",
    """    if placements.is_empty() {
        if let Some(cache) = cache {
""",
    """    if placements.is_empty() {
        crate::window_region::restore_all_window_regions();
        if let Some(cache) = cache {
""",
)

placement_path = "crates/platform_win32/src/placement.rs"
placement = read(placement_path)
apply_marker = "pub fn apply_placements(\n"
apply_at = placement.index(apply_marker)
insert = r'''fn clip_spec_for(
    specs: &[WindowRegionClipSpec],
    window_id: WindowId,
) -> Option<WindowRegionClipSpec> {
    specs.iter().rev().find(|spec| spec.window_id == window_id).copied()
}

fn prepare_region_clipped_placements(
    placements: &[WindowPlacement],
    specs: &[WindowRegionClipSpec],
    high_contrast: bool,
) -> (Vec<WindowPlacement>, HashSet<WindowId>) {
    let mut effective = Vec::with_capacity(placements.len());
    let mut active = HashSet::with_capacity(specs.len());

    for placement in placements {
        let Some(spec) = clip_spec_for(specs, placement.window_id) else {
            effective.push(placement.clone());
            continue;
        };
        if placement.visibility != Visibility::Visible || placement.column_index == usize::MAX {
            effective.push(placement.clone());
            continue;
        }

        let Ok(hwnd) = window_id_to_hwnd(placement.window_id) else {
            effective.push(placement.clone());
            continue;
        };
        let (left, top, right, bottom) = if high_contrast {
            (0, 0, 0, 0)
        } else {
            invisible_border_insets(hwnd)
        };
        let outer = Rect::new(
            placement.rect.x.saturating_sub(left),
            placement.rect.y.saturating_sub(top),
            placement
                .rect
                .width
                .saturating_add(left)
                .saturating_add(right),
            placement
                .rect
                .height
                .saturating_add(top)
                .saturating_add(bottom),
        );

        if crate::window_region::apply_window_region_clip(
            placement.window_id,
            outer,
            spec.clip_bounds,
            false,
        ) {
            active.insert(placement.window_id);
            effective.push(placement.clone());
        } else {
            let _ = crate::window_region::restore_window_region(placement.window_id, false);
            let mut fallback = placement.clone();
            fallback.rect = spec.fallback_rect;
            fallback.visibility = spec.fallback_visibility;
            effective.push(fallback);
        }
    }

    (effective, active)
}

'''
placement = placement[:apply_at] + insert + placement[apply_at:]
write(placement_path, placement)

replace_once(
    placement_path,
    """    if let Some(ref mut cache) = cache {
        cache.sync_inset_generation();
    }

    // Cache presence identifies an intermediate animation frame.
""",
    """    if let Some(ref mut cache) = cache {
        cache.sync_inset_generation();
    }

    let high_contrast = crate::is_high_contrast_enabled();
    let (effective_placements, mut active_region_ids) = prepare_region_clipped_placements(
        placements,
        &config.region_clips,
        high_contrast,
    );
    let placements = effective_placements.as_slice();

    // Cache presence identifies an intermediate animation frame.
""",
)
# Remove the now-duplicate high-contrast declaration while retaining comments.
replace_once(
    placement_path,
    """    // In high contrast mode, DWM paints a visible border in the normally-invisible
    // frame area.  If we expand by the usual insets, adjacent windows' visible borders
    // overlap and the layout gaps disappear.  Zero the insets to keep correct spacing.
    let high_contrast = crate::is_high_contrast_enabled();

""",
    """    // In high contrast mode, DWM paints a visible border in the normally-invisible
    // frame area. The pre-pass and entry builder both use zero insets so region
    // and position geometry stay in the same coordinate system.

""",
)
replace_once(
    placement_path,
    "    let (applied, failed_window_ids) = position_entries(&entries);\n",
    """    let (applied, mut failed_window_ids) = position_entries(&entries);
    let failed_clips: Vec<WindowId> = active_region_ids
        .iter()
        .filter(|window_id| failed_window_ids.contains(window_id))
        .copied()
        .collect();
    for window_id in failed_clips {
        let _ = crate::window_region::restore_window_region(window_id, true);
        active_region_ids.remove(&window_id);
    }
    crate::window_region::restore_window_regions_not_in(&active_region_ids);
""",
)
replace_once(
    placement_path,
    """        detect_size_violations(&entries, &failed_window_ids, &mut cache)
""",
    """        detect_size_violations(
            &entries,
            &failed_window_ids,
            &active_region_ids,
            &mut cache,
        )
""",
)
replace_once(
    placement_path,
    """                e.visibility == Visibility::Visible
                    && e.w > 1
""",
    """                e.visibility == Visibility::Visible
                    && !active_region_ids.contains(&e.window_id)
                    && e.w > 1
""",
)
replace_once(
    placement_path,
    """fn detect_size_violations(
    entries: &[DeferEntry],
    failed_window_ids: &HashSet<u64>,
    cache: &mut Option<&mut PlacementCache>,
""",
    """fn detect_size_violations(
    entries: &[DeferEntry],
    failed_window_ids: &HashSet<u64>,
    region_clipped_ids: &HashSet<u64>,
    cache: &mut Option<&mut PlacementCache>,
""",
)
replace_once(
    placement_path,
    """            || failed_window_ids.contains(&entry.window_id)
        {
""",
    """            || failed_window_ids.contains(&entry.window_id)
            || region_clipped_ids.contains(&entry.window_id)
        {
""",
)
replace_once(
    placement_path,
    """pub fn clear_suspected_oversize(window_id: WindowId) {
    let mut guard = lock_suspected_oversize();
""",
    """pub fn clear_suspected_oversize(window_id: WindowId) {
    crate::window_region::forget_window_region(window_id);
    let mut guard = lock_suspected_oversize();
""",
)
replace_once(
    placement_path,
    """pub fn dwm_uncloak_all() {
    let _commit = lock_cloak_commit();
""",
    """pub fn dwm_uncloak_all() {
    crate::window_region::restore_all_window_regions();
    let _commit = lock_cloak_commit();
""",
)
replace_once(
    placement_path,
    """fn uncloak_all_tracked() {
    let _commit = lock_cloak_commit();
""",
    """fn uncloak_all_tracked() {
    crate::window_region::restore_all_window_regions();
    let _commit = lock_cloak_commit();
""",
)

# ---------------------------------------------------------------------------
# Settings GUI wiring
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

print("real SetWindowRgn v10 patch applied")
