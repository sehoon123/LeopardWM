from copy import deepcopy
from pathlib import Path
import re
import shutil
from bs4 import BeautifulSoup

ROOT = Path.cwd()
CONTROL = ROOT.parent / "control" / ".github" / "v10"


def replace_once(path: Path, old: str, new: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: expected one replacement target, found {count}")
    path.write_text(text.replace(old, new), encoding="utf-8", newline="\n")


def replace_section(path: Path, start: str, end: str, replacement: str) -> None:
    text = path.read_text(encoding="utf-8")
    start_at = text.find(start)
    end_at = text.find(end, start_at)
    if start_at < 0 or end_at < 0:
        raise RuntimeError(f"{path}: section markers not found")
    path.write_text(text[:start_at] + replacement + text[end_at:], encoding="utf-8", newline="\n")


def function_span(text: str, name: str) -> tuple[int, int]:
    start = text.find(f"fn {name}(")
    if start < 0:
        raise RuntimeError(f"function {name} not found")
    brace = text.find("{", start)
    depth = 0
    for index in range(brace, len(text)):
        if text[index] == "{":
            depth += 1
        elif text[index] == "}":
            depth -= 1
            if depth == 0:
                return start, index + 1
    raise RuntimeError(f"unbalanced function {name}")


# New, independently testable modules.
shutil.copyfile(CONTROL / "window_region.rs", ROOT / "crates/platform_win32/src/window_region.rs")
shutil.copyfile(CONTROL / "layout_region_tests.rs", ROOT / "crates/daemon/src/layout_region_tests.rs")
shutil.copyfile(CONTROL / "edge_peek_behavior.rs", ROOT / "crates/core_layout/tests/edge_peek_behavior.rs")

# Platform public types.
types = ROOT / "crates/platform_win32/src/types.rs"
replace_once(
    types,
    "use leopardwm_core_layout::{Rect, WindowId};",
    "use leopardwm_core_layout::{Rect, Visibility, WindowId};",
)
replace_once(
    types,
    """#[derive(Debug, Clone, Default)]
pub struct PlatformConfig {
    pub animation_placement_policy: AnimationPlacementPolicy,
}
""",
    """/// Screen-space clipping request for one tiled placement.
///
/// `bounds` is the owner monitor work area. If SetWindowRgn cannot be used
/// safely, the platform applies the supplied whole-window fallback instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowRegionClip {
    pub window_id: WindowId,
    pub bounds: Rect,
    pub fallback_rect: Rect,
    pub fallback_visibility: Visibility,
}

#[derive(Debug, Clone, Default)]
pub struct PlatformConfig {
    pub animation_placement_policy: AnimationPlacementPolicy,
    pub window_region_clips: Vec<WindowRegionClip>,
}
""",
)
replace_once(
    types,
    """        assert_eq!(
            config.animation_placement_policy,
            AnimationPlacementPolicy::AdaptiveCompositorSafe
        );
""",
    """        assert_eq!(
            config.animation_placement_policy,
            AnimationPlacementPolicy::AdaptiveCompositorSafe
        );
        assert!(config.window_region_clips.is_empty());
""",
)

lib = ROOT / "crates/platform_win32/src/lib.rs"
replace_once(lib, "mod window_style;", "mod window_style;\nmod window_region;")
replace_once(
    lib,
    """pub use types::{
    AnimationPlacementPolicy, MonitorId, MonitorInfo, PlatformConfig, Win32Error, WindowInfo,
};""",
    """pub use types::{
    AnimationPlacementPolicy, MonitorId, MonitorInfo, PlatformConfig, Win32Error, WindowInfo,
    WindowRegionClip,
};""",
)
replace_once(
    lib,
    "pub use window_style::{",
    """pub use window_region::{
    forget_managed_window_region, managed_regions_match, restore_all_managed_window_regions,
};
pub use window_style::{""",
)

# Backward-compatible config with clipping default and explicit safe fallback.
config = ROOT / "crates/daemon/src/config.rs"
replace_once(
    config,
    "/// Layout-related configuration.\n",
    """/// How tiled windows that cross into another monitor are handled.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MonitorOverflowModeConfig {
    /// Clip the HWND to its owner work area. Applications with their own
    /// window region automatically use the whole-window fallback.
    #[default]
    Clip,
    /// Never mutate a window region; hide the whole overflowing tiled window.
    Hide,
}

/// Layout-related configuration.
""",
)
replace_once(
    config,
    """    #[serde(default = "default_false")]
    pub center_past_edges: bool;
""".replace(";", ","),
    """    #[serde(default = "default_false")]
    pub center_past_edges: bool,

    /// How tiled windows are isolated from adjacent monitors.
    #[serde(default)]
    pub monitor_overflow: MonitorOverflowModeConfig,
""",
)
replace_once(
    config,
    """            centering_mode: CenteringModeConfig::default(),
            center_past_edges: false,
            width_presets: default_width_presets(),""",
    """            centering_mode: CenteringModeConfig::default(),
            center_past_edges: false,
            monitor_overflow: MonitorOverflowModeConfig::default(),
            width_presets: default_width_presets(),""",
)
with config.open("a", encoding="utf-8", newline="\n") as handle:
    handle.write(
        """

#[cfg(test)]
mod monitor_overflow_mode_tests {
    use super::*;

    #[test]
    fn monitor_overflow_defaults_to_clip() {
        assert_eq!(
            LayoutConfig::default().monitor_overflow,
            MonitorOverflowModeConfig::Clip
        );
    }

    #[test]
    fn monitor_overflow_modes_round_trip() {
        for (text, expected) in [
            ("clip", MonitorOverflowModeConfig::Clip),
            ("hide", MonitorOverflowModeConfig::Hide),
        ] {
            let config: Config = toml::from_str(&format!(
                "[layout]\\nmonitor_overflow = \\"{text}\\"\\n"
            ))
            .unwrap();
            assert_eq!(config.layout.monitor_overflow, expected);
            let serialized = toml::to_string(&config).unwrap();
            assert!(serialized.contains(&format!("monitor_overflow = \\"{text}\\"")));
        }
    }
}
"""
    )

# Settings UI: clone the existing select row so native styling/responsiveness stay identical.
settings = ROOT / "crates/daemon/src/settings/settings.html"
html = settings.read_text(encoding="utf-8")
soup = BeautifulSoup(html, "html.parser")
source = soup.find(id="layout-centering_mode")
if source is None:
    raise RuntimeError("settings centering select not found")
row = source.find_parent(class_=lambda value: value and "setting-row" in value)
if row is None:
    raise RuntimeError("settings row not found")
clone = deepcopy(row)
select = clone.find("select")
if select is None:
    raise RuntimeError("settings source select not found")
select["id"] = "layout-monitor_overflow"
select.clear()
for value, label in [("clip", "Clip at monitor edge"), ("hide", "Hide whole window")]:
    option = soup.new_tag("option", value=value)
    option.string = label
    select.append(option)
semantic = []
for tag in clone.find_all(["div", "span", "label", "p"]):
    classes = " ".join(tag.get("class", [])).lower()
    if tag.get_text(strip=True) and any(
        key in classes for key in ("title", "label", "name", "description", "desc")
    ):
        semantic.append((tag, classes))
title = next((tag for tag, classes in semantic if any(key in classes for key in ("title", "label", "name"))), None)
description = next((tag for tag, classes in semantic if any(key in classes for key in ("description", "desc"))), None)
if title is None or description is None or title is description:
    raise RuntimeError("settings semantic labels not found")
title.string = "Monitor overflow"
description.string = (
    "Clip partial tiled windows at monitor edges. Apps with custom regions use safe whole-window hiding."
)
new_row = "\n" + str(clone) + "\n"
needle = 'id="layout-center_past_edges"'
position = html.find(needle)
if position < 0:
    raise RuntimeError("center-past-edges control not found")
start = html.rfind("<div", 0, position)
while start >= 0:
    open_end = html.find(">", start)
    if "setting-row" in html[start : open_end + 1]:
        break
    start = html.rfind("<div", 0, start)
if start < 0:
    raise RuntimeError("center-past-edges row start not found")
depth = 0
end = None
for match in re.finditer(r"<div\b|</div\s*>", html[start:], re.IGNORECASE):
    token = match.group(0).lower()
    depth += 1 if token.startswith("<div") else -1
    if depth == 0:
        end = start + match.end()
        break
if end is None:
    raise RuntimeError("center-past-edges row end not found")
html = html[:end] + new_row + html[end:]
load = "setChecked('layout-center_past_edges', cfg.layout.center_past_edges);"
if html.count(load) != 1:
    raise RuntimeError("settings load marker mismatch")
html = html.replace(
    load,
    load + "\n      setVal('layout-monitor_overflow', cfg.layout.monitor_overflow || 'clip');",
)
save = "center_past_edges: checked('layout-center_past_edges'),"
if html.count(save) != 1:
    raise RuntimeError("settings save marker mismatch")
html = html.replace(
    save,
    save + "\n          monitor_overflow: val('layout-monitor_overflow'),",
)
settings.write_text(html, encoding="utf-8", newline="\n")

settings_rs = ROOT / "crates/daemon/src/settings/html.rs"
with settings_rs.open("a", encoding="utf-8", newline="\n") as handle:
    handle.write(
        """

#[cfg(test)]
mod monitor_overflow_settings_tests {
    use super::SETTINGS_HTML;

    #[test]
    fn monitor_overflow_control_is_present_and_round_trips() {
        assert!(SETTINGS_HTML.contains("id=\\\"layout-monitor_overflow\\\""));
        assert!(SETTINGS_HTML.contains("<option value=\\\"clip\\\">"));
        assert!(SETTINGS_HTML.contains("<option value=\\\"hide\\\">"));
        assert!(SETTINGS_HTML.contains(
            "setVal('layout-monitor_overflow', cfg.layout.monitor_overflow || 'clip');"
        ));
        assert!(SETTINGS_HTML.contains(
            "monitor_overflow: val('layout-monitor_overflow')"
        ));
    }
}
"""
    )

# Daemon monitor policy and clip propagation.
layout = ROOT / "crates/daemon/src/layout_apply.rs"
replace_section(
    layout,
    "/// Keep tiled placements isolated to their owning monitor.",
    "/// Pick an off-screen rect for `window`",
    """/// Apply the selected cross-monitor overflow policy to one owner batch.
///
/// `Clip` leaves visible tiled placements at their strip coordinates and emits
/// screen-space region requests. `Hide` retains the conservative whole-window
/// fallback. Existing off-screen placements are always parked clear of every
/// monitor, and floating windows may span monitors intentionally.
fn apply_monitor_overflow_policy(
    placements: &mut [leopardwm_core_layout::WindowPlacement],
    owner_id: leopardwm_platform_win32::MonitorId,
    focused_column: Option<usize>,
    mode: crate::config::MonitorOverflowModeConfig,
    monitors: &std::collections::HashMap<
        leopardwm_platform_win32::MonitorId,
        leopardwm_platform_win32::MonitorInfo,
    >,
    monitor_rects: &[leopardwm_core_layout::Rect],
    region_clips: &mut Vec<leopardwm_platform_win32::WindowRegionClip>,
) {
    use crate::config::MonitorOverflowModeConfig;
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

        let crosses_owner_edge = placement.rect.x < owner_rect.x
            || placement.rect.right() > owner_rect.right()
            || placement.rect.y < owner_rect.y
            || placement.rect.bottom() > owner_rect.bottom();
        if placement.column_index == usize::MAX || !crosses_owner_edge {
            continue;
        }

        let focused = focused_column == Some(placement.column_index);
        let fallback_rect = if focused {
            placement.rect.clamped_inside(owner.work_area)
        } else {
            offscreen_park_rect(placement.rect, owner_rect, monitor_rects)
        };
        let fallback_visibility = if focused {
            Visibility::Visible
        } else if placement.rect.x < owner_rect.x {
            Visibility::OffScreenLeft
        } else {
            Visibility::OffScreenRight
        };

        if mode == MonitorOverflowModeConfig::Clip {
            region_clips.push(leopardwm_platform_win32::WindowRegionClip {
                window_id: placement.window_id,
                bounds: owner.work_area,
                fallback_rect,
                fallback_visibility,
            });
        } else {
            placement.rect = fallback_rect;
            placement.visibility = fallback_visibility;
        }
    }
}

""",
)
replace_once(
    layout,
    """        let mut all_placements = Vec::new();
        let monitor_rects: Vec<_> = self.monitors.values().map(|monitor| monitor.rect).collect();""",
    """        let mut all_placements = Vec::new();
        let mut region_clips = Vec::new();
        let monitor_rects: Vec<_> = self.monitors.values().map(|monitor| monitor.rect).collect();""",
)
replace_once(
    layout,
    """        for (owner_id, focused_column, start, end) in owner_ranges {
            park_offscreen_avoiding_neighbors(
                &mut all_placements[start..end],
                owner_id,
                focused_column,
                &self.monitors,
                &monitor_rects,
            );
        }""",
    """        for (owner_id, focused_column, start, end) in owner_ranges {
            apply_monitor_overflow_policy(
                &mut all_placements[start..end],
                owner_id,
                focused_column,
                self.config.layout.monitor_overflow,
                &self.monitors,
                &monitor_rects,
                &mut region_clips,
            );
        }""",
)
replace_once(
    layout,
    """                park_offscreen_avoiding_neighbors(
                    std::slice::from_mut(placement),
                    owner_id,
                    None,
                    &self.monitors,
                    &monitor_rects,
                );""",
    """                apply_monitor_overflow_policy(
                    std::slice::from_mut(placement),
                    owner_id,
                    None,
                    self.config.layout.monitor_overflow,
                    &self.monitors,
                    &monitor_rects,
                    &mut region_clips,
                );""",
)
replace_once(
    layout,
    """        let mut platform_config = self.platform_config.clone();
        platform_config.animation_placement_policy = if self.config.behavior.compositor_safe_mode {""",
    """        let mut platform_config = self.platform_config.clone();
        platform_config.window_region_clips = region_clips;
        platform_config.animation_placement_policy = if self.config.behavior.compositor_safe_mode {""",
)
replace_once(
    layout,
    "        let mut all_placements = self.collect_apply_placements();",
    "        let (mut all_placements, region_clips) = self.collect_apply_placements();",
)
replace_once(
    layout,
    "        let placements_unchanged = self.placements_match_last_applied(&all_placements);",
    """        let placements_unchanged = self.placements_match_last_applied(&all_placements)
            && leopardwm_platform_win32::managed_regions_match(
                all_placements.iter().map(|placement| placement.window_id),
                &region_clips,
            );""",
)
replace_once(
    layout,
    "        let (rx, worker_handle) = match self.spawn_apply_worker(all_placements) {",
    "        let (rx, worker_handle) = match self.spawn_apply_worker(all_placements, region_clips) {",
)
replace_section(
    layout,
    """    /// Collect animated placements for every monitor's active workspace, with debug logging.
    fn collect_apply_placements(&self) -> Vec<leopardwm_core_layout::WindowPlacement> {""",
    "    /// Fast-path check: every placement matches the last applied rect and the visible-set is unchanged.",
    """    /// Collect animated placements and monitor-region clips for every active workspace.
    fn collect_apply_placements(
        &self,
    ) -> (
        Vec<leopardwm_core_layout::WindowPlacement>,
        Vec<leopardwm_platform_win32::WindowRegionClip>,
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
                    apply_monitor_overflow_policy(
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

""",
)
replace_once(
    layout,
    """    fn spawn_apply_worker(
        &mut self,
        all_placements: Vec<leopardwm_core_layout::WindowPlacement>,
    ) -> Result<(""",
    """    fn spawn_apply_worker(
        &mut self,
        all_placements: Vec<leopardwm_core_layout::WindowPlacement>,
        region_clips: Vec<leopardwm_platform_win32::WindowRegionClip>,
    ) -> Result<(""",
)
replace_once(
    layout,
    "        let platform_config = self.platform_config.clone();",
    """        let mut platform_config = self.platform_config.clone();
        platform_config.window_region_clips = region_clips;""",
)
text = layout.read_text(encoding="utf-8")
text = text.replace("use super::park_offscreen_avoiding_neighbors;", "use super::apply_monitor_overflow_policy;")
text = text.replace("park_offscreen_avoiding_neighbors(", "apply_monitor_overflow_policy(")
pattern = re.compile(
    r"apply_monitor_overflow_policy\(\n(?P<i>\s*)(?P<p>[^\n]+),\n(?P=i)(?P<o>[^\n]+),\n(?P=i)(?P<f>[^\n]+),\n(?P=i)(?P<m>[^\n]+),\n(?P=i)(?P<r>[^\n]+),\n(?P=i)\);"
)

def adapt(match: re.Match[str]) -> str:
    indent = match.group("i")
    return (
        f"apply_monitor_overflow_policy(\n{indent}{match.group('p')},\n{indent}{match.group('o')},\n"
        f"{indent}{match.group('f')},\n{indent}crate::config::MonitorOverflowModeConfig::Hide,\n"
        f"{indent}{match.group('m')},\n{indent}{match.group('r')},\n{indent}&mut Vec::new(),\n{indent});"
    )

text, count = pattern.subn(adapt, text)
if count < 2:
    raise RuntimeError(f"expected old layout tests to adapt, changed {count}")
module_marker = """#[cfg(test)]
#[path = "layout_apply_edge_tests.rs"]
mod edge_safety_audit_tests;
"""
if module_marker not in text:
    raise RuntimeError("edge safety module marker missing")
text = text.replace(
    module_marker,
    module_marker
    + """
#[cfg(test)]
#[path = "layout_region_tests.rs"]
mod layout_region_tests;
""",
)
layout.write_text(text, encoding="utf-8", newline="\n")

# Win32 placement cache, region ownership, fallback, and cleanup.
placement = ROOT / "crates/platform_win32/src/placement.rs"
replace_once(
    placement,
    "use crate::types::{AnimationPlacementPolicy, PlatformConfig, Win32Error};",
    "use crate::types::{AnimationPlacementPolicy, PlatformConfig, Win32Error, WindowRegionClip};",
)
replace_once(
    placement,
    """    compositor_sensitive: HashMap<WindowId, bool>,
    /// Generation of `GLOBAL_INSET_CACHE` reflected by `insets`.""",
    """    compositor_sensitive: HashMap<WindowId, bool>,
    /// Desired clip is part of the animation cache key. `None` is stored
    /// explicitly so removing a clip invalidates an unchanged rectangle.
    region_clips: HashMap<WindowId, Option<WindowRegionClip>>,
    /// Generation of `GLOBAL_INSET_CACHE` reflected by `insets`.""",
)
replace_once(
    placement,
    """            compositor_sensitive: HashMap::new(),
            inset_generation: INSET_CACHE_GENERATION.load(Ordering::Acquire),""",
    """            compositor_sensitive: HashMap::new(),
            region_clips: HashMap::new(),
            inset_generation: INSET_CACHE_GENERATION.load(Ordering::Acquire),""",
)
replace_once(
    placement,
    """        self.positions.clear();
        self.compositor_sensitive.clear();""",
    """        self.positions.clear();
        self.compositor_sensitive.clear();
        self.region_clips.clear();""",
)
replace_once(
    placement,
    """            self.positions.clear();
            self.inset_generation = current;""",
    """            self.positions.clear();
            self.region_clips.clear();
            self.inset_generation = current;""",
)
replace_once(
    placement,
    """    flags: windows::Win32::UI::WindowsAndMessaging::SET_WINDOW_POS_FLAGS,
    column_index: usize,
}""",
    """    flags: windows::Win32::UI::WindowsAndMessaging::SET_WINDOW_POS_FLAGS,
    column_index: usize,
    region_clip: Option<WindowRegionClip>,
}""",
)
replace_once(
    placement,
    """        // Uncloak all tracked windows — no placements means all previous
        // windows have left this layout (e.g., workspace switch to empty workspace).
        uncloak_all_tracked();
        return Ok(empty_result);""",
    """        uncloak_all_tracked();
        crate::window_region::restore_all_managed_window_regions();
        return Ok(empty_result);""",
)
replace_once(
    placement,
    """        config.animation_placement_policy,
        high_contrast,
    );""",
    """        config.animation_placement_policy,
        &config.window_region_clips,
        high_contrast,
    );""",
)
replace_once(
    placement,
    "    // Uncloak windows that are becoming visible BEFORE positioning,",
    """    let region_preparation = prepare_window_regions(&mut entries, animation_frame);

    // Uncloak windows that are becoming visible BEFORE positioning,""",
)
replace_once(
    placement,
    """        cache
            .compositor_sensitive
            .retain(|id, _| current_ids.contains(id));
        // Update entries for windows that were actually positioned
        let positioned: std::collections::HashSet<u64> = entries
            .iter()
            .filter(|e| !failed_window_ids.contains(&e.window_id))
            .map(|e| e.window_id)
            .collect();
        for p in placements {
            if positioned.contains(&p.window_id) {
                cache.positions.insert(p.window_id, (p.rect, p.visibility));
            }
        }""",
    """        cache
            .compositor_sensitive
            .retain(|id, _| current_ids.contains(id));
        cache.region_clips.retain(|id, _| current_ids.contains(id));
        let positioned: std::collections::HashSet<u64> = entries
            .iter()
            .filter(|entry| {
                !failed_window_ids.contains(&entry.window_id)
                    && !region_preparation.retry_ids.contains(&entry.window_id)
            })
            .map(|entry| entry.window_id)
            .collect();
        for placement in placements {
            if positioned.contains(&placement.window_id) {
                let clip = config
                    .window_region_clips
                    .iter()
                    .find(|clip| clip.window_id == placement.window_id)
                    .copied();
                cache
                    .positions
                    .insert(placement.window_id, (placement.rect, placement.visibility));
                cache.region_clips.insert(placement.window_id, clip);
            }
        }""",
)
replace_once(
    placement,
    "    sync_cloak_state(&entries, placements, &failed_window_ids);",
    """    sync_cloak_state(
        placements,
        &region_preparation.visibility_overrides,
        &failed_window_ids,
    );
    let current_ids: HashSet<_> = placements.iter().map(|placement| placement.window_id).collect();
    crate::window_region::prune_managed_regions(&current_ids);""",
)
replace_once(
    placement,
    """    policy: AnimationPlacementPolicy,
    high_contrast: bool,
) -> (Vec<DeferEntry>, u32) {""",
    """    policy: AnimationPlacementPolicy,
    region_clips: &[WindowRegionClip],
    high_contrast: bool,
) -> (Vec<DeferEntry>, u32) {""",
)
replace_once(
    placement,
    """        let previous = cache
            .as_ref()
            .and_then(|cache| cache.positions.get(&placement.window_id).copied());
        if previous == Some((placement.rect, placement.visibility)) {
            skipped += 1;
            continue;
        }""",
    """        let previous = cache
            .as_ref()
            .and_then(|cache| cache.positions.get(&placement.window_id).copied());
        let region_clip = region_clips
            .iter()
            .find(|clip| clip.window_id == placement.window_id)
            .copied();
        let previous_clip = cache
            .as_ref()
            .and_then(|cache| cache.region_clips.get(&placement.window_id).copied())
            .flatten();
        if previous == Some((placement.rect, placement.visibility))
            && previous_clip == region_clip
        {
            skipped += 1;
            continue;
        }""",
)
text = placement.read_text(encoding="utf-8")
entry_tail = """                flags,
                column_index: placement.column_index,
            });"""
if text.count(entry_tail) != 2:
    raise RuntimeError("unexpected DeferEntry constructor count")
text = text.replace(
    entry_tail,
    """                flags,
                column_index: placement.column_index,
                region_clip,
            });""",
)
marker = """/// Uncloak entries becoming visible and drop them from the tracking set.
fn uncloak_becoming_visible"""
if marker not in text:
    raise RuntimeError("uncloak marker missing")
text = text.replace(
    marker,
    """#[derive(Default)]
struct RegionPreparation {
    visibility_overrides: HashMap<WindowId, Visibility>,
    retry_ids: HashSet<WindowId>,
}

fn apply_region_fallback(entry: &mut DeferEntry, clip: WindowRegionClip) {
    entry.visibility = clip.fallback_visibility;
    entry.x = clip.fallback_rect.x.saturating_sub(entry.used_insets.0);
    entry.y = clip.fallback_rect.y.saturating_sub(entry.used_insets.1);
    entry.layout_rect = clip.fallback_rect;
    entry.flags = SWP_NOZORDER | SWP_NOACTIVATE;
    if clip.fallback_visibility != Visibility::Visible {
        entry.flags |= SWP_NOSIZE;
        entry.h = 0;
    }
}

fn prepare_window_regions(
    entries: &mut [DeferEntry],
    animation_frame: bool,
) -> RegionPreparation {
    let mut result = RegionPreparation::default();
    for entry in entries {
        let Some(clip) = entry.region_clip else {
            if !crate::window_region::clear_managed_region(entry.window_id, !animation_frame) {
                result.retry_ids.insert(entry.window_id);
            }
            continue;
        };
        if entry.visibility != Visibility::Visible {
            let _ = crate::window_region::clear_managed_region(entry.window_id, !animation_frame);
            continue;
        }

        let outer = Rect::new(entry.x, entry.y, entry.w, entry.h);
        match crate::window_region::apply_managed_region(
            entry.window_id,
            outer,
            clip.bounds,
            !animation_frame,
        ) {
            crate::window_region::RegionApplyOutcome::Applied => {}
            crate::window_region::RegionApplyOutcome::Unsupported => {
                apply_region_fallback(entry, clip);
                result
                    .visibility_overrides
                    .insert(entry.window_id, clip.fallback_visibility);
            }
            crate::window_region::RegionApplyOutcome::Retry => {
                apply_region_fallback(entry, clip);
                result
                    .visibility_overrides
                    .insert(entry.window_id, clip.fallback_visibility);
                result.retry_ids.insert(entry.window_id);
            }
        }
    }
    result
}

/// Uncloak entries becoming visible and drop them from the tracking set.
fn uncloak_becoming_visible""",
)
start, end = function_span(text, "sync_cloak_state")
new_sync = """fn sync_cloak_state(
    placements: &[WindowPlacement],
    visibility_overrides: &HashMap<WindowId, Visibility>,
    failed_window_ids: &HashSet<WindowId>,
) {
    let _commit = lock_cloak_commit();
    let current_ids: HashSet<_> = placements.iter().map(|placement| placement.window_id).collect();
    let mut changed = Vec::new();
    {
        let mut guard = lock_cloaked();
        let set = guard.get_or_insert_with(HashSet::new);
        let stale: Vec<_> = set
            .iter()
            .filter(|window_id| !current_ids.contains(window_id))
            .copied()
            .collect();
        for window_id in stale {
            set.remove(&window_id);
            changed.push(window_id);
        }
        for placement in placements {
            if failed_window_ids.contains(&placement.window_id) {
                continue;
            }
            let visibility = visibility_overrides
                .get(&placement.window_id)
                .copied()
                .unwrap_or(placement.visibility);
            let changed_here = if visibility == Visibility::Visible {
                set.remove(&placement.window_id)
            } else {
                set.insert(placement.window_id)
            };
            if changed_here {
                changed.push(placement.window_id);
            }
        }
    }
    changed.sort_unstable();
    changed.dedup();
    for window_id in changed {
        let _ = apply_cloak_state_locked(window_id);
    }
}"""
text = text[:start] + new_sync + text[end:]
old = """pub fn dwm_uncloak_all() {
    let _commit = lock_cloak_commit();"""
if text.count(old) != 1:
    raise RuntimeError("dwm_uncloak_all marker mismatch")
text = text.replace(
    old,
    """pub fn dwm_uncloak_all() {
    crate::window_region::restore_all_managed_window_regions();
    let _commit = lock_cloak_commit();""",
)
placement.write_text(text, encoding="utf-8", newline="\n")

# Destroyed HWNDs must be forgotten without touching a potentially recycled handle.
cleanup_sites = 0
for source in (ROOT / "crates/daemon/src").rglob("*.rs"):
    text = source.read_text(encoding="utf-8")
    marker = "leopardwm_platform_win32::forget_recycled_ghost_cloak(hwnd);"
    if marker in text and "forget_managed_window_region(hwnd);" not in text:
        text = text.replace(
            marker,
            marker + "\n        leopardwm_platform_win32::forget_managed_window_region(hwnd);",
        )
        source.write_text(text, encoding="utf-8", newline="\n")
        cleanup_sites += 1
if cleanup_sites != 1:
    raise RuntimeError(f"expected one destroyed-HWND cleanup site, found {cleanup_sites}")

print("window-region v10 patch applied")
