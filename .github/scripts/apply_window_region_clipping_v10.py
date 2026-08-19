from __future__ import annotations

import re
from pathlib import Path


def replace_once(path: Path, old: str, new: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: expected one replacement target, found {count}: {old[:80]!r}")
    path.write_text(text.replace(old, new), encoding="utf-8", newline="\n")


def replace_section(path: Path, start: str, end: str, replacement: str) -> None:
    text = path.read_text(encoding="utf-8")
    start_at = text.find(start)
    end_at = text.find(end, start_at)
    if start_at < 0 or end_at < 0:
        raise RuntimeError(f"{path}: section markers not found: {start!r} -> {end!r}")
    path.write_text(text[:start_at] + replacement + text[end_at:], encoding="utf-8", newline="\n")


def insert_before_first(path: Path, marker: str, content: str) -> None:
    text = path.read_text(encoding="utf-8")
    at = text.find(marker)
    if at < 0:
        raise RuntimeError(f"{path}: marker not found: {marker!r}")
    path.write_text(text[:at] + content + text[at:], encoding="utf-8", newline="\n")


# ---------------------------------------------------------------------------
# Daemon configuration and Settings GUI
# ---------------------------------------------------------------------------
config = Path("crates/daemon/src/config.rs")
config_text = config.read_text(encoding="utf-8")
layout_struct_at = config_text.find("pub struct LayoutConfig")
if layout_struct_at < 0:
    raise RuntimeError("config.rs: LayoutConfig not found")
derive_at = config_text.rfind("#[derive", 0, layout_struct_at)
if derive_at < 0:
    raise RuntimeError("config.rs: LayoutConfig derive not found")
mode_enum = '''#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MonitorOverflowMode {
    /// Keep partially visible tiled windows in place and clip their HWND region
    /// to the owning monitor. Windows that cannot be clipped fall back to Hide.
    #[default]
    Clip,
    /// Stable fallback: keep the focused column inside its work area and park
    /// non-focused overflow clear of every monitor.
    Hide,
}

'''
if "pub enum MonitorOverflowMode" in config_text:
    raise RuntimeError("config.rs: MonitorOverflowMode already exists")
config_text = config_text[:derive_at] + mode_enum + config_text[derive_at:]
field_pattern = re.compile(r"(?m)^(\s*)pub center_past_edges: bool,\s*$")
config_text, count = field_pattern.subn(
    lambda m: m.group(0)
    + "\n"
    + m.group(1)
    + "#[serde(default)]\n"
    + m.group(1)
    + "pub monitor_overflow_mode: MonitorOverflowMode,",
    config_text,
    count=1,
)
if count != 1:
    raise RuntimeError("config.rs: center_past_edges field not found")
default_at = config_text.find("impl Default for LayoutConfig")
if default_at < 0:
    raise RuntimeError("config.rs: LayoutConfig default impl not found")
next_impl = config_text.find("\nimpl ", default_at + 1)
default_end = len(config_text) if next_impl < 0 else next_impl
default_block = config_text[default_at:default_end]
default_block, count = re.subn(
    r"(?m)^(\s*)center_past_edges:\s*(true|false),\s*$",
    lambda m: m.group(0)
    + "\n"
    + m.group(1)
    + "monitor_overflow_mode: MonitorOverflowMode::Clip,",
    default_block,
    count=1,
)
if count != 1:
    raise RuntimeError("config.rs: LayoutConfig default field not found")
config_text = config_text[:default_at] + default_block + config_text[default_end:]
config_text += '''

#[cfg(test)]
mod monitor_overflow_mode_tests {
    use super::MonitorOverflowMode;

    #[test]
    fn monitor_overflow_mode_defaults_to_clipping() {
        assert_eq!(MonitorOverflowMode::default(), MonitorOverflowMode::Clip);
    }

    #[test]
    fn monitor_overflow_mode_has_stable_serialized_names() {
        assert_eq!(
            serde_json::from_str::<MonitorOverflowMode>("\\\"clip\\\"").unwrap(),
            MonitorOverflowMode::Clip
        );
        assert_eq!(
            serde_json::from_str::<MonitorOverflowMode>("\\\"hide\\\"").unwrap(),
            MonitorOverflowMode::Hide
        );
        assert_eq!(
            serde_json::to_string(&MonitorOverflowMode::Clip).unwrap(),
            "\\\"clip\\\""
        );
    }
}
'''
config.write_text(config_text, encoding="utf-8", newline="\n")

settings = Path("crates/daemon/src/settings/settings.html")
html = settings.read_text(encoding="utf-8")
if "layout-monitor_overflow_mode" in html:
    raise RuntimeError("settings.html: monitor overflow control already exists")
marker = 'id="layout-center_past_edges"'
marker_at = html.find(marker)
if marker_at < 0:
    raise RuntimeError("settings.html: center-past-edges control not found")

# Find the nearest enclosing setting-like div and insert a sibling row after it.
tag_pattern = re.compile(r"<div\b[^>]*>|</div\s*>", re.IGNORECASE)
stack: list[tuple[int, str]] = []
for match in tag_pattern.finditer(html, 0, marker_at):
    tag = match.group(0)
    if tag.lower().startswith("</"):
        if stack:
            stack.pop()
    else:
        stack.append((match.start(), tag))
selected = None
for start, tag in reversed(stack):
    lowered = tag.lower()
    if any(name in lowered for name in ("setting", "field", "option", "row")):
        selected = (start, tag)
        break
if selected is None and stack:
    selected = stack[-1]
if selected is None:
    raise RuntimeError("settings.html: enclosing control row not found")
row_start, row_tag = selected
depth = 0
row_end = None
for match in tag_pattern.finditer(html, row_start):
    tag = match.group(0)
    if not tag.lower().startswith("</"):
        depth += 1
    else:
        depth -= 1
        if depth == 0:
            row_end = match.end()
            break
if row_end is None:
    raise RuntimeError("settings.html: enclosing control row is unbalanced")

row_class = re.search(r'class\s*=\s*"([^"]+)"', row_tag, re.IGNORECASE)
row_class = row_class.group(1) if row_class else "setting-row"
label_class = "label" if 'class="label"' in html else "setting-label"
hint_class = "hint" if 'class="hint"' in html else "setting-description"
control = f'''

          <div class="{row_class}">
            <div>
              <div class="{label_class}">Monitor edge overflow</div>
              <div class="{hint_class}">Clip partial tiled windows at the owning monitor edge. Hide whole window is the compatibility fallback.</div>
            </div>
            <select id="layout-monitor_overflow_mode" style="max-width: 280px; width: min(280px, 100%);">
              <option value="clip">Clip at monitor edge</option>
              <option value="hide">Hide whole window (fallback)</option>
            </select>
          </div>'''
html = html[:row_end] + control + html[row_end:]

center_load = re.search(
    r"(?m)^(?P<indent>\s*)(?P<setter>[A-Za-z_$][\w$]*)\('layout-centering_mode',\s*[^;]+\);\s*$",
    html,
)
if not center_load:
    raise RuntimeError("settings.html: centering-mode load setter not found")
edge_load = re.search(
    r"(?m)^(?P<indent>\s*)setChecked\('layout-center_past_edges',\s*cfg\.layout\.center_past_edges\);\s*$",
    html,
)
if not edge_load:
    raise RuntimeError("settings.html: center-past-edges load line not found")
load_line = (
    edge_load.group(0)
    + "\n"
    + edge_load.group("indent")
    + center_load.group("setter")
    + "('layout-monitor_overflow_mode', cfg.layout.monitor_overflow_mode || 'clip');"
)
html = html[: edge_load.start()] + load_line + html[edge_load.end() :]

center_save = re.search(
    r"centering_mode:\s*(?P<getter>[A-Za-z_$][\w$]*)\('layout-centering_mode'\)",
    html,
)
if not center_save:
    raise RuntimeError("settings.html: centering-mode save getter not found")
edge_save = re.search(
    r"(?m)^(?P<indent>\s*)center_past_edges:\s*checked\('layout-center_past_edges'\),?\s*$",
    html,
)
if not edge_save:
    raise RuntimeError("settings.html: center-past-edges save property not found")
existing = edge_save.group(0).rstrip()
if not existing.endswith(","):
    existing += ","
save_lines = (
    existing
    + "\n"
    + edge_save.group("indent")
    + "monitor_overflow_mode: "
    + center_save.group("getter")
    + "('layout-monitor_overflow_mode'),"
)
html = html[: edge_save.start()] + save_lines + html[edge_save.end() :]
settings.write_text(html, encoding="utf-8", newline="\n")

html_rs = Path("crates/daemon/src/settings/html.rs")
html_rs_text = html_rs.read_text(encoding="utf-8")
html_rs_text += '''

#[cfg(test)]
mod monitor_overflow_settings_tests {
    use super::SETTINGS_HTML;

    #[test]
    fn monitor_overflow_mode_is_visible_and_wired_for_round_trip() {
        for marker in [
            "id=\\\"layout-monitor_overflow_mode\\\"",
            "value=\\\"clip\\\"",
            "value=\\\"hide\\\"",
            "cfg.layout.monitor_overflow_mode || 'clip'",
            "monitor_overflow_mode:",
            "('layout-monitor_overflow_mode')",
        ] {
            assert!(SETTINGS_HTML.contains(marker), "missing Settings marker: {marker}");
        }
    }
}
'''
html_rs.write_text(html_rs_text, encoding="utf-8", newline="\n")

# ---------------------------------------------------------------------------
# Platform configuration and window-region module wiring
# ---------------------------------------------------------------------------
platform_types = Path("crates/platform_win32/src/types.rs")
replace_once(
    platform_types,
    "use leopardwm_core_layout::{Rect, WindowId};\n",
    "use leopardwm_core_layout::{Rect, WindowId};\nuse std::collections::HashMap;\nuse std::sync::Arc;\n",
)
replace_once(
    platform_types,
    """pub struct PlatformConfig {
    pub animation_placement_policy: AnimationPlacementPolicy,
}""",
    """pub struct PlatformConfig {
    pub animation_placement_policy: AnimationPlacementPolicy,
    /// Owner-monitor bounds for visible tiled HWNDs that need temporary
    /// SetWindowRgn clipping on this placement batch.
    pub window_clip_bounds: Arc<HashMap<WindowId, Rect>>,
}""",
)
replace_once(
    platform_types,
    """        assert_eq!(
            config.animation_placement_policy,
            AnimationPlacementPolicy::AdaptiveCompositorSafe
        );""",
    """        assert_eq!(
            config.animation_placement_policy,
            AnimationPlacementPolicy::AdaptiveCompositorSafe
        );
        assert!(config.window_clip_bounds.is_empty());""",
)

platform_lib = Path("crates/platform_win32/src/lib.rs")
replace_once(
    platform_lib,
    "mod window_style;\n",
    "mod window_style;\nmod window_region;\n",
)
insert_before_first(
    platform_lib,
    "pub use window_style::{",
    "pub use window_region::{\n    can_clip_window_region, forget_window_region, restore_all_window_regions,\n};\n",
)

# ---------------------------------------------------------------------------
# Win32 placement integration
# ---------------------------------------------------------------------------
placement = Path("crates/platform_win32/src/placement.rs")
text = placement.read_text(encoding="utf-8")
text = text.replace(
    """    compositor_sensitive: HashMap<WindowId, bool>,
    /// Generation of `GLOBAL_INSET_CACHE` reflected by `insets`.""",
    """    compositor_sensitive: HashMap<WindowId, bool>,
    /// Last owner bounds used for a LeopardWM-owned clipping region. Kept
    /// separate from positions so a mode/topology change cannot hit the
    /// unchanged-placement fast path with a stale window region.
    clip_bounds: HashMap<WindowId, Rect>,
    /// Generation of `GLOBAL_INSET_CACHE` reflected by `insets`.""",
    1,
)
text = text.replace(
    """            compositor_sensitive: HashMap::new(),
            inset_generation:""",
    """            compositor_sensitive: HashMap::new(),
            clip_bounds: HashMap::new(),
            inset_generation:""",
    1,
)
text = text.replace(
    """        self.positions.clear();
        self.compositor_sensitive.clear();""",
    """        self.positions.clear();
        self.compositor_sensitive.clear();
        self.clip_bounds.clear();""",
    1,
)
text = text.replace(
    """            self.positions.clear();
            self.inset_generation = current;""",
    """            self.positions.clear();
            self.clip_bounds.clear();
            self.inset_generation = current;""",
    1,
)
text = text.replace(
    """    flags: windows::Win32::UI::WindowsAndMessaging::SET_WINDOW_POS_FLAGS,
    column_index: usize,""",
    """    flags: windows::Win32::UI::WindowsAndMessaging::SET_WINDOW_POS_FLAGS,
    column_index: usize,
    clip_bounds: Option<Rect>,""",
    1,
)
text = text.replace(
    """        uncloak_all_tracked();
        return Ok(empty_result);""",
    """        uncloak_all_tracked();
        crate::window_region::restore_all_window_regions();
        return Ok(empty_result);""",
    1,
)
text = text.replace(
    """        config.animation_placement_policy,
        high_contrast,""",
    """        config.animation_placement_policy,
        config.window_clip_bounds.as_ref(),
        high_contrast,""",
    1,
)
text = text.replace(
    """    let (applied, failed_window_ids) = position_entries(&entries);

    // Detect size violations""",
    """    let (applied, failed_window_ids) = position_entries(&entries);
    let region_failures = synchronize_window_regions(
        &entries,
        &failed_window_ids,
        !animation_frame,
    );

    // Detect size violations""",
    1,
)
text = text.replace(
    "let (width_violations, height_violations, geometry_mismatches) = if !animation_frame {",
    "let (width_violations, height_violations, mut geometry_mismatches) = if !animation_frame {",
    1,
)
needle = "    }; // end: skip landing verification during async frames\n\n    // Update cache:"
if needle not in text:
    raise RuntimeError("placement.rs: landing verification marker not found")
text = text.replace(
    needle,
    "    }; // end: skip landing verification during async frames\n    geometry_mismatches.extend(region_failures.iter().copied());\n\n    // Update cache:",
    1,
)
text = text.replace(
    """    if let Some(cache) = cache {
        let current_ids: std::collections::HashSet<u64> =
            placements.iter().map(|p| p.window_id).collect();""",
    """    let current_ids: std::collections::HashSet<u64> =
        placements.iter().map(|p| p.window_id).collect();
    if let Some(cache) = cache {""",
    1,
)
text = text.replace(
    """        cache
            .compositor_sensitive
            .retain(|id, _| current_ids.contains(id));""",
    """        cache
            .compositor_sensitive
            .retain(|id, _| current_ids.contains(id));
        cache.clip_bounds.retain(|id, _| current_ids.contains(id));""",
    1,
)
text = text.replace(
    """            .filter(|e| !failed_window_ids.contains(&e.window_id))""",
    """            .filter(|e| {
                !failed_window_ids.contains(&e.window_id)
                    && !region_failures.contains(&e.window_id)
            })""",
    1,
)
text = text.replace(
    """            if positioned.contains(&p.window_id) {
                cache.positions.insert(p.window_id, (p.rect, p.visibility));
            }
        }
    }

    // Cloak off-screen windows AFTER positioning.""",
    """            if positioned.contains(&p.window_id) {
                cache.positions.insert(p.window_id, (p.rect, p.visibility));
                if let Some(bounds) = config.window_clip_bounds.get(&p.window_id) {
                    cache.clip_bounds.insert(p.window_id, *bounds);
                } else {
                    cache.clip_bounds.remove(&p.window_id);
                }
            }
        }
        for window_id in &region_failures {
            cache.positions.remove(window_id);
            cache.clip_bounds.remove(window_id);
        }
    }
    crate::window_region::restore_window_regions_not_in(&current_ids);

    // Cloak off-screen windows AFTER positioning.""",
    1,
)
text = text.replace(
    """    policy: AnimationPlacementPolicy,
    high_contrast: bool,""",
    """    policy: AnimationPlacementPolicy,
    clip_bounds: &HashMap<WindowId, Rect>,
    high_contrast: bool,""",
    1,
)
text = text.replace(
    """        let previous = cache
            .as_ref()
            .and_then(|cache| cache.positions.get(&placement.window_id).copied());
        if previous == Some((placement.rect, placement.visibility)) {
            skipped += 1;
            continue;
        }
        let position_only""",
    """        let previous = cache
            .as_ref()
            .and_then(|cache| cache.positions.get(&placement.window_id).copied());
        let current_clip = clip_bounds.get(&placement.window_id).copied();
        let previous_clip = cache
            .as_ref()
            .and_then(|cache| cache.clip_bounds.get(&placement.window_id).copied());
        if previous == Some((placement.rect, placement.visibility))
            && previous_clip == current_clip
        {
            skipped += 1;
            continue;
        }
        let position_only""",
    1,
)
# Both visible and off-screen DeferEntry literals share this tail.
text = text.replace(
    """                flags,
                column_index: placement.column_index,
            });""",
    """                flags,
                column_index: placement.column_index,
                clip_bounds: current_clip,
            });""",
)
if text.count("clip_bounds: current_clip") != 2:
    raise RuntimeError("placement.rs: expected two DeferEntry clip fields")
text = text.replace(
    """        if entry.column_index == usize::MAX
            || entry.visibility != Visibility::Visible""",
    """        if entry.column_index == usize::MAX
            || entry.visibility != Visibility::Visible
            || entry.clip_bounds.is_some()""",
    1,
)
sync_fn = '''
/// Synchronize temporary SetWindowRgn ownership with the current placement
/// batch. Region failures are fed into the daemon's existing guarded geometry
/// re-apply, where capability caching selects the whole-window fallback.
fn synchronize_window_regions(
    entries: &[DeferEntry],
    failed_window_ids: &HashSet<WindowId>,
    redraw: bool,
) -> Vec<WindowId> {
    let mut failures = Vec::new();
    for entry in entries {
        if failed_window_ids.contains(&entry.window_id) {
            continue;
        }
        let ok = if let Some(bounds) = entry.clip_bounds {
            crate::window_region::apply_window_region_clip(
                entry.window_id,
                Rect::new(entry.x, entry.y, entry.w, entry.h),
                entry.layout_rect,
                bounds,
                redraw,
            )
        } else {
            crate::window_region::restore_window_region(entry.window_id, redraw)
        };
        if !ok {
            failures.push(entry.window_id);
        }
    }
    failures
}

'''
marker = "/// Per-window suspect state for the size-violation two-pass confirmation:"
if marker not in text:
    raise RuntimeError("placement.rs: synchronize insertion marker missing")
text = text.replace(marker, sync_fn + marker, 1)
text = text.replace(
    """pub fn dwm_uncloak_all() {
    let _commit = lock_cloak_commit();""",
    """pub fn dwm_uncloak_all() {
    crate::window_region::restore_all_window_regions();
    let _commit = lock_cloak_commit();""",
    1,
)
placement.write_text(text, encoding="utf-8", newline="\n")

# ---------------------------------------------------------------------------
# AppState clip-cache identity
# ---------------------------------------------------------------------------
state = Path("crates/daemon/src/state.rs")
state_text = state.read_text(encoding="utf-8")
state_text, count = re.subn(
    r"(?m)^(\s*)(?:pub\(crate\)\s+)?last_placed_layout_rects:\s*HashMap<u64,\s*Rect>,\s*$",
    lambda m: m.group(0)
    + "\n"
    + m.group(1)
    + "pub(crate) last_placed_clip_bounds: HashMap<u64, Rect>,",
    state_text,
    count=1,
)
if count != 1:
    raise RuntimeError("state.rs: last_placed_layout_rects field not found")
state_text, count = re.subn(
    r"(?m)^(\s*)last_placed_layout_rects:\s*HashMap::new\(\),\s*$",
    lambda m: m.group(0)
    + "\n"
    + m.group(1)
    + "last_placed_clip_bounds: HashMap::new(),",
    state_text,
    count=1,
)
if count != 1:
    raise RuntimeError("state.rs: last_placed_layout_rects initializer not found")
state.write_text(state_text, encoding="utf-8", newline="\n")

# ---------------------------------------------------------------------------
# Daemon placement planning and fallback policy
# ---------------------------------------------------------------------------
layout = Path("crates/daemon/src/layout_apply.rs")
replacement = '''/// Keep tiled placements isolated to their owning monitor while preserving
/// partial-column peeks in clip mode.
fn clamp_horizontally_inside(
    rect: leopardwm_core_layout::Rect,
    bounds: leopardwm_core_layout::Rect,
) -> leopardwm_core_layout::Rect {
    let width = rect.width.max(1).min(bounds.width.max(1));
    let max_x = bounds.x.saturating_add(bounds.width.max(1).saturating_sub(width));
    leopardwm_core_layout::Rect::new(rect.x.clamp(bounds.x, max_x), rect.y, width, rect.height)
}

fn apply_monitor_overflow_policy_with<F>(
    placements: &mut [leopardwm_core_layout::WindowPlacement],
    owner_id: leopardwm_platform_win32::MonitorId,
    focused_column: Option<usize>,
    mode: crate::config::MonitorOverflowMode,
    monitors: &std::collections::HashMap<
        leopardwm_platform_win32::MonitorId,
        leopardwm_platform_win32::MonitorInfo,
    >,
    monitor_rects: &[leopardwm_core_layout::Rect],
    clip_bounds: &mut std::collections::HashMap<u64, leopardwm_core_layout::Rect>,
    mut can_clip: F,
) where
    F: FnMut(u64) -> bool,
{
    use crate::config::MonitorOverflowMode;
    use leopardwm_core_layout::Visibility;

    let Some(owner) = monitors.get(&owner_id) else {
        return;
    };
    let owner_rect = owner.rect;

    for placement in placements {
        let intersects_neighbor = monitors
            .iter()
            .filter(|(id, _)| **id != owner_id)
            .any(|(_, monitor)| placement.rect.intersects(&monitor.rect));
        if !intersects_neighbor {
            continue;
        }

        if placement.visibility == Visibility::Visible {
            let crosses_horizontal_edge = placement.rect.x < owner_rect.x
                || placement.rect.right() > owner_rect.right();
            let crosses_vertical_edge = placement.rect.y < owner_rect.y
                || placement.rect.bottom() > owner_rect.bottom();

            // Floating windows may span monitors intentionally. Mirrored
            // displays can share coordinates, so a tiled window wholly inside
            // its owner also remains untouched.
            if placement.column_index == usize::MAX
                || (!crosses_horizontal_edge && !crosses_vertical_edge)
            {
                continue;
            }

            if mode == MonitorOverflowMode::Clip && crosses_horizontal_edge {
                let visible_left = placement.rect.x.max(owner.work_area.x);
                let visible_right = placement.rect.right().min(owner.work_area.right());
                if visible_right > visible_left && can_clip(placement.window_id) {
                    clip_bounds.insert(placement.window_id, owner.work_area);
                    continue;
                }
            }

            // Stable fallback for protected/custom-region HWNDs and explicit
            // Hide mode. Focus stays usable; background overflow is parked.
            if focused_column == Some(placement.column_index) && crosses_horizontal_edge {
                placement.rect = clamp_horizontally_inside(placement.rect, owner.work_area);
                let still_intersects_neighbor = monitors
                    .iter()
                    .filter(|(id, _)| **id != owner_id)
                    .any(|(_, monitor)| placement.rect.intersects(&monitor.rect));
                if !crosses_vertical_edge && !still_intersects_neighbor {
                    continue;
                }
            }

            placement.visibility = if placement.rect.x < owner_rect.x {
                Visibility::OffScreenLeft
            } else {
                Visibility::OffScreenRight
            };
        }

        placement.rect = offscreen_park_rect(placement.rect, owner_rect, monitor_rects);
    }
}

fn apply_monitor_overflow_policy(
    placements: &mut [leopardwm_core_layout::WindowPlacement],
    owner_id: leopardwm_platform_win32::MonitorId,
    focused_column: Option<usize>,
    mode: crate::config::MonitorOverflowMode,
    monitors: &std::collections::HashMap<
        leopardwm_platform_win32::MonitorId,
        leopardwm_platform_win32::MonitorInfo,
    >,
    monitor_rects: &[leopardwm_core_layout::Rect],
    clip_bounds: &mut std::collections::HashMap<u64, leopardwm_core_layout::Rect>,
) {
    apply_monitor_overflow_policy_with(
        placements,
        owner_id,
        focused_column,
        mode,
        monitors,
        monitor_rects,
        clip_bounds,
        leopardwm_platform_win32::can_clip_window_region,
    );
}

#[cfg(test)]
fn park_offscreen_avoiding_neighbors(
    placements: &mut [leopardwm_core_layout::WindowPlacement],
    owner_id: leopardwm_platform_win32::MonitorId,
    focused_column: Option<usize>,
    monitors: &std::collections::HashMap<
        leopardwm_platform_win32::MonitorId,
        leopardwm_platform_win32::MonitorInfo,
    >,
    monitor_rects: &[leopardwm_core_layout::Rect],
) {
    let mut unused = std::collections::HashMap::new();
    apply_monitor_overflow_policy_with(
        placements,
        owner_id,
        focused_column,
        crate::config::MonitorOverflowMode::Hide,
        monitors,
        monitor_rects,
        &mut unused,
        |_| false,
    );
}

'''
replace_section(
    layout,
    "/// Keep tiled placements isolated to their owning monitor.",
    "/// Pick an off-screen rect for `window`",
    replacement,
)
layout_text = layout.read_text(encoding="utf-8")

# Animation path: establish a clip map, then apply policy after interpolation.
first_monitor_snapshot = "let monitor_rects: Vec<_> = self.monitors.values().map(|monitor| monitor.rect).collect();"
first_at = layout_text.find(first_monitor_snapshot)
if first_at < 0:
    raise RuntimeError("layout_apply.rs: animation monitor snapshot missing")
layout_text = (
    layout_text[:first_at]
    + first_monitor_snapshot
    + "\n        let mut window_clip_bounds = std::collections::HashMap::new();"
    + layout_text[first_at + len(first_monitor_snapshot) :]
)
layout_text = layout_text.replace(
    """            park_offscreen_avoiding_neighbors(
                &mut all_placements[start..end],
                owner_id,
                focused_column,
                &self.monitors,
                &monitor_rects,
            );""",
    """            apply_monitor_overflow_policy(
                &mut all_placements[start..end],
                owner_id,
                focused_column,
                self.config.layout.monitor_overflow_mode,
                &self.monitors,
                &monitor_rects,
                &mut window_clip_bounds,
            );""",
    1,
)
layout_text = layout_text.replace(
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
                    self.config.layout.monitor_overflow_mode,
                    &self.monitors,
                    &monitor_rects,
                    &mut window_clip_bounds,
                );""",
    1,
)
layout_text = layout_text.replace(
    """        platform_config.animation_placement_policy = if self.config.behavior.compositor_safe_mode {
            leopardwm_platform_win32::AnimationPlacementPolicy::AdaptiveCompositorSafe
        } else {
            leopardwm_platform_win32::AnimationPlacementPolicy::LegacyAsync
        };""",
    """        platform_config.animation_placement_policy = if self.config.behavior.compositor_safe_mode {
            leopardwm_platform_win32::AnimationPlacementPolicy::AdaptiveCompositorSafe
        } else {
            leopardwm_platform_win32::AnimationPlacementPolicy::LegacyAsync
        };
        platform_config.window_clip_bounds = std::sync::Arc::new(window_clip_bounds);""",
    1,
)

# Exact apply path now carries clip bounds into the platform worker and cache key.
layout_text = layout_text.replace(
    "let mut all_placements = self.collect_apply_placements();",
    "let (mut all_placements, window_clip_bounds) = self.collect_apply_placements();",
    1,
)
layout_text = layout_text.replace(
    """        let placements_unchanged = self.placements_match_last_applied(&all_placements);""",
    """        let placements_unchanged =
            self.placements_match_last_applied(&all_placements, &window_clip_bounds);""",
    1,
)
layout_text = layout_text.replace(
    """        self.record_last_placed_rects(&all_placements);""",
    """        self.record_last_placed_rects(&all_placements, &window_clip_bounds);""",
    1,
)
layout_text = layout_text.replace(
    """        let (rx, worker_handle) = match self.spawn_apply_worker(all_placements) {""",
    """        let (rx, worker_handle) =
            match self.spawn_apply_worker(all_placements, window_clip_bounds) {""",
    1,
)
layout_text = layout_text.replace(
    """                        self.last_placed_layout_rects.remove(hwnd);""",
    """                        self.last_placed_layout_rects.remove(hwnd);
                        self.last_placed_clip_bounds.remove(hwnd);""",
    1,
)
# Every full cache clear in this file also invalidates region-plan identity.
layout_text = layout_text.replace(
    "self.last_placed_layout_rects.clear();",
    "self.last_placed_layout_rects.clear();\n                self.last_placed_clip_bounds.clear();",
)
# Rustfmt will normalize the indentation for replacements outside this block.

# Replace placement collection function as a whole.
collect_start = layout_text.find(
    "    fn collect_apply_placements(&self) -> Vec<leopardwm_core_layout::WindowPlacement> {"
)
collect_end = layout_text.find(
    "    /// Fast-path check:", collect_start
)
if collect_start < 0 or collect_end < 0:
    raise RuntimeError("layout_apply.rs: collect_apply_placements section not found")
collect_fn = '''    fn collect_apply_placements(
        &self,
    ) -> (
        Vec<leopardwm_core_layout::WindowPlacement>,
        std::collections::HashMap<u64, leopardwm_core_layout::Rect>,
    ) {
        let mut all_placements = Vec::new();
        let mut window_clip_bounds = std::collections::HashMap::new();
        // Reuse one monitor-rect snapshot for every owner monitor in this
        // batch instead of allocating it again per monitor.
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
                        self.config.layout.monitor_overflow_mode,
                        &self.monitors,
                        &monitor_rects,
                        &mut window_clip_bounds,
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
                    for placement in &placements {
                        if placement.visibility == leopardwm_core_layout::Visibility::Visible {
                            debug!(
                                "  placement hwnd={:#x} col={} rect=({},{} {}x{}) vis={:?} clipped={}",
                                placement.window_id,
                                placement.column_index,
                                placement.rect.x,
                                placement.rect.y,
                                placement.rect.width,
                                placement.rect.height,
                                placement.visibility,
                                window_clip_bounds.contains_key(&placement.window_id),
                            );
                        }
                    }
                    all_placements.extend(placements);
                }
            }
        }

        (all_placements, window_clip_bounds)
    }

'''
layout_text = layout_text[:collect_start] + collect_fn + layout_text[collect_end:]

# Fast-path and record functions include clipping-plan identity.
layout_text = layout_text.replace(
    """    fn placements_match_last_applied(
        &self,
        all_placements: &[leopardwm_core_layout::WindowPlacement],
    ) -> bool {""",
    """    fn placements_match_last_applied(
        &self,
        all_placements: &[leopardwm_core_layout::WindowPlacement],
        clip_bounds: &std::collections::HashMap<u64, leopardwm_core_layout::Rect>,
    ) -> bool {
        if &self.last_placed_clip_bounds != clip_bounds {
            return false;
        }""",
    1,
)
layout_text = layout_text.replace(
    """    fn record_last_placed_rects(
        &mut self,
        all_placements: &[leopardwm_core_layout::WindowPlacement],
    ) {""",
    """    fn record_last_placed_rects(
        &mut self,
        all_placements: &[leopardwm_core_layout::WindowPlacement],
        clip_bounds: &std::collections::HashMap<u64, leopardwm_core_layout::Rect>,
    ) {
        self.last_placed_clip_bounds.clone_from(clip_bounds);""",
    1,
)

# Worker accepts the exact clip plan.
layout_text = layout_text.replace(
    """        all_placements: Vec<leopardwm_core_layout::WindowPlacement>,
    ) -> Result<(""",
    """        all_placements: Vec<leopardwm_core_layout::WindowPlacement>,
        window_clip_bounds: std::collections::HashMap<u64, leopardwm_core_layout::Rect>,
    ) -> Result<(""",
    1,
)
layout_text = layout_text.replace(
    """        let platform_config = self.platform_config.clone();""",
    """        let mut platform_config = self.platform_config.clone();
        platform_config.window_clip_bounds = std::sync::Arc::new(window_clip_bounds);""",
    1,
)
layout.write_text(layout_text, encoding="utf-8", newline="\n")

# Replace the v9 external policy tests with clip-mode + fallback coverage.
edge_tests = Path("crates/daemon/src/layout_apply_edge_tests.rs")
edge_tests.write_text(
    r'''use super::{apply_monitor_overflow_policy_with, park_offscreen_avoiding_neighbors};
use crate::config::MonitorOverflowMode;
use leopardwm_core_layout::{Rect, Visibility, WindowPlacement};
use leopardwm_platform_win32::{MonitorId, MonitorInfo};
use std::collections::HashMap;

fn monitor(id: MonitorId, x: i32) -> MonitorInfo {
    let rect = Rect::new(x, 0, 1920, 1080);
    MonitorInfo {
        id,
        rect,
        work_area: rect,
        is_primary: id == 1,
        device_name: format!("DISPLAY{id}"),
        scale_factor: 1.0,
    }
}

fn monitors() -> HashMap<MonitorId, MonitorInfo> {
    HashMap::from([(1, monitor(1, 0)), (2, monitor(2, 1920))])
}

fn placement(window_id: u64, rect: Rect, column_index: usize) -> WindowPlacement {
    WindowPlacement {
        window_id,
        rect,
        visibility: Visibility::Visible,
        column_index,
    }
}

fn apply(
    placements: &mut [WindowPlacement],
    owner_id: MonitorId,
    focused_column: Option<usize>,
    mode: MonitorOverflowMode,
    capability: bool,
) -> HashMap<u64, Rect> {
    let monitors = monitors();
    let monitor_rects: Vec<_> = monitors.values().map(|monitor| monitor.rect).collect();
    let mut clips = HashMap::new();
    apply_monitor_overflow_policy_with(
        placements,
        owner_id,
        focused_column,
        mode,
        &monitors,
        &monitor_rects,
        &mut clips,
        |_| capability,
    );
    clips
}

#[test]
fn clip_mode_preserves_partial_peeks_on_both_edges() {
    let mut placements = vec![
        placement(1, Rect::new(-300, 40, 600, 800), 0),
        placement(2, Rect::new(1700, 40, 600, 800), 2),
    ];
    let original = placements.clone();

    let clips = apply(&mut placements, 1, Some(1), MonitorOverflowMode::Clip, true);

    assert_eq!(placements[0].rect, original[0].rect);
    assert_eq!(placements[1].rect, original[1].rect);
    assert!(placements
        .iter()
        .all(|placement| placement.visibility == Visibility::Visible));
    assert_eq!(clips.get(&1), Some(&Rect::new(0, 0, 1920, 1080)));
    assert_eq!(clips.get(&2), Some(&Rect::new(0, 0, 1920, 1080)));
}

#[test]
fn clip_mode_keeps_focused_column_centered_instead_of_clamping_geometry() {
    let original = Rect::new(-300, 40, 600, 800);
    let mut placements = vec![placement(3, original, 0)];

    let clips = apply(&mut placements, 1, Some(0), MonitorOverflowMode::Clip, true);

    assert_eq!(placements[0].rect, original);
    assert_eq!(placements[0].visibility, Visibility::Visible);
    assert!(clips.contains_key(&3));
}

#[test]
fn custom_region_or_protected_window_uses_the_stable_fallback() {
    let mut focused = vec![placement(4, Rect::new(1700, 40, 600, 800), 1)];
    let clips = apply(
        &mut focused,
        1,
        Some(1),
        MonitorOverflowMode::Clip,
        false,
    );
    assert!(clips.is_empty());
    assert_eq!(focused[0].visibility, Visibility::Visible);
    assert_eq!(focused[0].rect, Rect::new(1320, 40, 600, 800));

    let mut background = vec![placement(5, Rect::new(1700, 40, 600, 800), 2)];
    let clips = apply(
        &mut background,
        1,
        Some(1),
        MonitorOverflowMode::Clip,
        false,
    );
    assert!(clips.is_empty());
    assert_ne!(background[0].visibility, Visibility::Visible);
    assert!(monitors()
        .values()
        .all(|monitor| !background[0].rect.intersects(&monitor.rect)));
}

#[test]
fn explicit_hide_mode_retains_the_v9_whole_window_policy() {
    let mut placements = vec![placement(6, Rect::new(1700, 40, 600, 800), 2)];
    let clips = apply(
        &mut placements,
        1,
        Some(1),
        MonitorOverflowMode::Hide,
        true,
    );
    assert!(clips.is_empty());
    assert_ne!(placements[0].visibility, Visibility::Visible);
    assert!(monitors()
        .values()
        .all(|monitor| !placements[0].rect.intersects(&monitor.rect)));
}

#[test]
fn floating_windows_remain_unregioned_and_may_span_monitors() {
    let original = Rect::new(1700, 40, 600, 800);
    let mut placements = vec![placement(7, original, usize::MAX)];
    let clips = apply(&mut placements, 1, Some(0), MonitorOverflowMode::Clip, true);
    assert!(clips.is_empty());
    assert_eq!(placements[0].rect, original);
    assert_eq!(placements[0].visibility, Visibility::Visible);
}

#[test]
fn fully_contained_and_mirrored_windows_are_not_regioned() {
    let original = Rect::new(100, 40, 800, 800);
    let mut placements = vec![placement(8, original, 0)];
    let clips = apply(&mut placements, 1, Some(0), MonitorOverflowMode::Clip, true);
    assert!(clips.is_empty());
    assert_eq!(placements[0].rect, original);

    let owner = monitor(1, 0);
    let mirror = monitor(2, 0);
    let monitor_map = HashMap::from([(1, owner), (2, mirror)]);
    let rects: Vec<_> = monitor_map.values().map(|monitor| monitor.rect).collect();
    let mut mirrored = vec![placement(9, original, 0)];
    let mut mirrored_clips = HashMap::new();
    apply_monitor_overflow_policy_with(
        &mut mirrored,
        1,
        Some(0),
        MonitorOverflowMode::Clip,
        &monitor_map,
        &rects,
        &mut mirrored_clips,
        |_| true,
    );
    assert!(mirrored_clips.is_empty());
    assert_eq!(mirrored[0].rect, original);
}

#[test]
fn horizontal_clip_matrix_preserves_geometry_and_never_requests_empty_regions() {
    let owner = Rect::new(0, 0, 1920, 1080);
    for width in [200, 600, 1920, 2500] {
        for x in (-2400..=2600).step_by(113) {
            let original = Rect::new(x, 40, width, 800);
            let intersects_owner = original.x < owner.right() && original.right() > owner.x;
            let leaks_right = original.intersects(&Rect::new(1920, 0, 1920, 1080));
            let mut placements = vec![placement(10, original, 0)];
            let clips = apply(&mut placements, 1, Some(0), MonitorOverflowMode::Clip, true);

            if leaks_right && intersects_owner {
                assert_eq!(placements[0].rect, original);
                assert_eq!(placements[0].visibility, Visibility::Visible);
                let bounds = clips.get(&10).expect("partial overlap must request clipping");
                let left = original.x.max(bounds.x);
                let right = original.right().min(bounds.right());
                assert!(right > left, "requested region must be non-empty");
            }
        }
    }
}

#[test]
fn old_test_wrapper_still_exercises_hide_fallback_only() {
    let monitor_map = monitors();
    let rects: Vec<_> = monitor_map.values().map(|monitor| monitor.rect).collect();
    let mut placements = vec![placement(11, Rect::new(1700, 40, 600, 800), 2)];
    park_offscreen_avoiding_neighbors(
        &mut placements,
        1,
        Some(1),
        &monitor_map,
        &rects,
    );
    assert_ne!(placements[0].visibility, Visibility::Visible);
}
''',
    encoding="utf-8",
    newline="\n",
)

print("SetWindowRgn clipping patch applied")
