from pathlib import Path
import re

ROOT = Path('.')


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding='utf-8')


def write(path: str, text: str) -> None:
    (ROOT / path).write_text(text, encoding='utf-8', newline='\n')


def replace_once(path: str, old: str, new: str) -> None:
    text = read(path)
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f'{path}: expected one occurrence, found {count}: {old[:120]!r}')
    write(path, text.replace(old, new))


def replace_in_section(path: str, start: str, end: str, old: str, new: str) -> None:
    text = read(path)
    a = text.find(start)
    b = text.find(end, a + len(start))
    if a < 0 or b < 0:
        raise RuntimeError(f'{path}: section not found: {start!r} .. {end!r}')
    section = text[a:b]
    count = section.count(old)
    if count != 1:
        raise RuntimeError(f'{path}: expected one section occurrence, found {count}: {old[:120]!r}')
    write(path, text[:a] + section.replace(old, new) + text[b:])


# ---------------------------------------------------------------------------
# platform_win32 public surface
# ---------------------------------------------------------------------------
replace_once(
    'crates/platform_win32/src/lib.rs',
    'mod window_style;\n',
    'mod window_style;\nmod window_region;\n',
)
replace_once(
    'crates/platform_win32/src/lib.rs',
    '    apply_placements, clear_inset_cache, dwm_cloak_window, dwm_uncloak_all, dwm_uncloak_window,\n',
    '    apply_placements, apply_placements_with_regions, clear_inset_cache, dwm_cloak_window,\n    dwm_uncloak_all, dwm_uncloak_window,\n',
)
replace_once(
    'crates/platform_win32/src/lib.rs',
    'pub use window_style::{\n',
    'pub use window_region::{forget_window_region, restore_all_window_regions, WindowRegionClip};\npub use window_style::{\n',
)

# ---------------------------------------------------------------------------
# Config model: clip by default, hide as an explicit conservative fallback.
# ---------------------------------------------------------------------------
replace_once(
    'crates/daemon/src/config.rs',
    '''    #[serde(default = "default_false")]\n    pub center_past_edges: bool,\n\n    /// Width presets for cycling (fractions of usable viewport width).\n''',
    '''    #[serde(default = "default_false")]\n    pub center_past_edges: bool,\n\n    /// How tiled windows that cross an adjacent monitor boundary are handled.\n    #[serde(default)]\n    pub monitor_overflow: MonitorOverflowModeConfig,\n\n    /// Width presets for cycling (fractions of usable viewport width).\n''',
)
replace_once(
    'crates/daemon/src/config.rs',
    '''            centering_mode: CenteringModeConfig::default(),\n            center_past_edges: false,\n            width_presets: default_width_presets(),\n''',
    '''            centering_mode: CenteringModeConfig::default(),\n            center_past_edges: false,\n            monitor_overflow: MonitorOverflowModeConfig::default(),\n            width_presets: default_width_presets(),\n''',
)
replace_once(
    'crates/daemon/src/config.rs',
    '''impl From<CenteringModeConfig> for CenteringMode {\n    fn from(config: CenteringModeConfig) -> Self {\n        match config {\n            CenteringModeConfig::Center => CenteringMode::Center,\n            CenteringModeConfig::JustInView => CenteringMode::JustInView,\n            CenteringModeConfig::OnOverflow => CenteringMode::OnOverflow,\n        }\n    }\n}\n\n/// Appearance-related configuration.\n''',
    '''impl From<CenteringModeConfig> for CenteringMode {\n    fn from(config: CenteringModeConfig) -> Self {\n        match config {\n            CenteringModeConfig::Center => CenteringMode::Center,\n            CenteringModeConfig::JustInView => CenteringMode::JustInView,\n            CenteringModeConfig::OnOverflow => CenteringMode::OnOverflow,\n        }\n    }\n}\n\n/// Multi-monitor overflow policy for tiled windows.\n#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]\n#[serde(rename_all = "snake_case")]\npub enum MonitorOverflowModeConfig {\n    /// Preserve partially visible neighboring columns and clip only the pixels\n    /// that would be painted on another monitor.\n    #[default]\n    Clip,\n    /// Conservative fallback: hide a tiled window as a whole when it would\n    /// intersect another monitor.\n    Hide,\n}\n\n/// Appearance-related configuration.\n''',
)
config = read('crates/daemon/src/config.rs')
config += '''\n\n#[cfg(test)]\nmod monitor_overflow_mode_tests {\n    use super::{Config, MonitorOverflowModeConfig};\n\n    #[test]\n    fn monitor_overflow_defaults_to_clip_for_existing_configs() {\n        let config: Config = toml::from_str("[layout]\\ncentering_mode = \\"center\\"\\n").unwrap();\n        assert_eq!(config.layout.monitor_overflow, MonitorOverflowModeConfig::Clip);\n    }\n\n    #[test]\n    fn monitor_overflow_hide_round_trips() {\n        let mut config = Config::default();\n        config.layout.monitor_overflow = MonitorOverflowModeConfig::Hide;\n        let encoded = toml::to_string(&config).unwrap();\n        assert!(encoded.contains("monitor_overflow = \\"hide\\""));\n        let decoded: Config = toml::from_str(&encoded).unwrap();\n        assert_eq!(decoded.layout.monitor_overflow, MonitorOverflowModeConfig::Hide);\n    }\n}\n'''
write('crates/daemon/src/config.rs', config)

# ---------------------------------------------------------------------------
# Settings GUI wiring. Insert a standard row immediately after the existing
# center-past-edges row without coupling to unrelated layout rows.
# ---------------------------------------------------------------------------
settings_path = 'crates/daemon/src/settings/settings.html'
settings = read(settings_path)
control_id = 'id="layout-center_past_edges"'
pos = settings.find(control_id)
if pos < 0:
    raise RuntimeError('settings.html: center-past-edges control not found')
next_row = settings.find('<div class="setting-row">', pos)
if next_row < 0:
    raise RuntimeError('settings.html: next layout setting row not found')
monitor_row = '''        <div class="setting-row">\n          <div class="setting-info">\n            <div class="setting-label">Monitor overflow</div>\n            <div class="setting-description">Keep neighboring columns partially visible while preventing them from painting on another monitor.</div>\n          </div>\n          <select id="layout-monitor_overflow">\n            <option value="clip">Clip at monitor edge</option>\n            <option value="hide">Hide whole window</option>\n          </select>\n        </div>\n\n'''
if 'id="layout-monitor_overflow"' in settings:
    raise RuntimeError('settings.html: monitor overflow control already exists')
settings = settings[:next_row] + monitor_row + settings[next_row:]
load_marker = "setChecked('layout-center_past_edges', cfg.layout.center_past_edges);"
if settings.count(load_marker) != 1:
    raise RuntimeError('settings.html: center-past-edges load marker mismatch')
settings = settings.replace(
    load_marker,
    load_marker + "\n      document.getElementById('layout-monitor_overflow').value = cfg.layout.monitor_overflow || 'clip';",
)
save_marker = "center_past_edges: checked('layout-center_past_edges'),"
if settings.count(save_marker) != 1:
    raise RuntimeError('settings.html: center-past-edges save marker mismatch')
settings = settings.replace(
    save_marker,
    save_marker + "\n          monitor_overflow: document.getElementById('layout-monitor_overflow').value,",
)
write(settings_path, settings)

settings_html_rs = read('crates/daemon/src/settings/html.rs')
settings_html_rs += '''\n\n#[cfg(test)]\nmod monitor_overflow_settings_tests {\n    use super::SETTINGS_HTML;\n\n    #[test]\n    fn monitor_overflow_control_is_complete_and_round_trippable() {\n        for marker in [\n            "id=\\\"layout-monitor_overflow\\\"",\n            "value=\\\"clip\\\"",\n            "value=\\\"hide\\\"",\n            "cfg.layout.monitor_overflow || 'clip'",\n            "monitor_overflow: document.getElementById('layout-monitor_overflow').value",\n        ] {\n            assert!(SETTINGS_HTML.contains(marker), "missing Settings marker: {marker}");\n        }\n    }\n}\n'''
write('crates/daemon/src/settings/html.rs', settings_html_rs)

# ---------------------------------------------------------------------------
# Daemon monitor-overflow policy and per-frame region plans.
# ---------------------------------------------------------------------------
layout_path = 'crates/daemon/src/layout_apply.rs'
layout = read(layout_path)
insert_after = '''pub(crate) fn park_offscreen_avoiding_neighbors(\n'''
start = layout.find(insert_after)
if start < 0:
    raise RuntimeError('layout_apply.rs: fallback function not found')
next_fn = layout.find('/// Pick an off-screen rect for `window`', start)
if next_fn < 0:
    raise RuntimeError('layout_apply.rs: fallback function end marker not found')
policy_code = '''fn upsert_region_clip(\n    clips: &mut Vec<leopardwm_platform_win32::WindowRegionClip>,\n    clip: leopardwm_platform_win32::WindowRegionClip,\n) {\n    if let Some(existing) = clips\n        .iter_mut()\n        .find(|existing| existing.window_id == clip.window_id)\n    {\n        *existing = clip;\n    } else {\n        clips.push(clip);\n    }\n}\n\n/// Apply the configured multi-monitor overflow policy to one owner monitor.\n/// `clip` preserves partial columns and emits a Win32 region plan; `hide` uses\n/// the existing whole-window fallback directly. Fully off-screen placements\n/// are always parked clear of every monitor.\nfn prepare_monitor_overflow(\n    placements: &mut [leopardwm_core_layout::WindowPlacement],\n    owner_id: leopardwm_platform_win32::MonitorId,\n    focused_column: Option<usize>,\n    mode: crate::config::MonitorOverflowModeConfig,\n    monitors: &std::collections::HashMap<\n        leopardwm_platform_win32::MonitorId,\n        leopardwm_platform_win32::MonitorInfo,\n    >,\n    monitor_rects: &[leopardwm_core_layout::Rect],\n    region_clips: &mut Vec<leopardwm_platform_win32::WindowRegionClip>,\n) {\n    use crate::config::MonitorOverflowModeConfig;\n    use leopardwm_core_layout::Visibility;\n\n    if mode == MonitorOverflowModeConfig::Hide {\n        park_offscreen_avoiding_neighbors(\n            placements,\n            owner_id,\n            focused_column,\n            monitors,\n            monitor_rects,\n        );\n        return;\n    }\n\n    let Some(owner) = monitors.get(&owner_id) else {\n        return;\n    };\n    let owner_rect = owner.rect;\n    for placement in placements {\n        if placement.column_index == usize::MAX {\n            continue;\n        }\n        let intersects_neighbor = monitors\n            .iter()\n            .filter(|(id, _)| **id != owner_id)\n            .any(|(_, monitor)| placement.rect.intersects(&monitor.rect));\n        if !intersects_neighbor {\n            continue;\n        }\n\n        if placement.visibility != Visibility::Visible {\n            placement.rect = offscreen_park_rect(placement.rect, owner_rect, monitor_rects);\n            continue;\n        }\n\n        let crosses_owner = placement.rect.x < owner_rect.x\n            || placement.rect.right() > owner_rect.right()\n            || placement.rect.y < owner_rect.y\n            || placement.rect.bottom() > owner_rect.bottom();\n        // Mirrored outputs can overlap in virtual coordinates. A tiled window\n        // wholly inside its owner is valid even if another monitor overlaps it.\n        if !crosses_owner {\n            continue;\n        }\n\n        let (fallback_rect, fallback_visibility) =\n            if focused_column == Some(placement.column_index) {\n                (placement.rect.clamped_inside(owner.work_area), Visibility::Visible)\n            } else {\n                let visibility = if placement.rect.x < owner_rect.x {\n                    Visibility::OffScreenLeft\n                } else {\n                    Visibility::OffScreenRight\n                };\n                (\n                    offscreen_park_rect(placement.rect, owner_rect, monitor_rects),\n                    visibility,\n                )\n            };\n\n        upsert_region_clip(\n            region_clips,\n            leopardwm_platform_win32::WindowRegionClip {\n                window_id: placement.window_id,\n                clip_bounds: owner_rect,\n                fallback_rect,\n                fallback_visibility,\n            },\n        );\n    }\n}\n\n'''
layout = layout[:next_fn] + policy_code + layout[next_fn:]
write(layout_path, layout)

# send_animation_frame: collect region plans after interpolation.
replace_in_section(
    layout_path,
    'pub(crate) fn send_animation_frame(',
    'fn collect_layout_apply_timeout_candidates(',
    '        let mut all_placements = Vec::new();\n',
    '        let mut all_placements = Vec::new();\n        let mut region_clips = Vec::new();\n',
)
replace_in_section(
    layout_path,
    'pub(crate) fn send_animation_frame(',
    'fn collect_layout_apply_timeout_candidates(',
    '''            park_offscreen_avoiding_neighbors(\n                &mut all_placements[start..end],\n                owner_id,\n                focused_column,\n                &self.monitors,\n                &monitor_rects,\n            );\n''',
    '''            prepare_monitor_overflow(\n                &mut all_placements[start..end],\n                owner_id,\n                focused_column,\n                self.config.layout.monitor_overflow,\n                &self.monitors,\n                &monitor_rects,\n                &mut region_clips,\n            );\n''',
)
replace_in_section(
    layout_path,
    'pub(crate) fn send_animation_frame(',
    'fn collect_layout_apply_timeout_candidates(',
    '''                park_offscreen_avoiding_neighbors(\n                    std::slice::from_mut(placement),\n                    owner_id,\n                    None,\n                    &self.monitors,\n                    &monitor_rects,\n                );\n''',
    '''                prepare_monitor_overflow(\n                    std::slice::from_mut(placement),\n                    owner_id,\n                    None,\n                    self.config.layout.monitor_overflow,\n                    &self.monitors,\n                    &monitor_rects,\n                    &mut region_clips,\n                );\n''',
)
replace_in_section(
    layout_path,
    'pub(crate) fn send_animation_frame(',
    'fn collect_layout_apply_timeout_candidates(',
    '''        let request = animation_worker::FrameRequest {\n            placements: live_placements,\n            ghost_updates,\n            platform_config,\n        };\n''',
    '''        let request = animation_worker::FrameRequest {\n            placements: live_placements,\n            region_clips,\n            ghost_updates,\n            platform_config,\n        };\n''',
)

# Exact landing path and worker.
replace_once(
    layout_path,
    '        let mut all_placements = self.collect_apply_placements();\n',
    '        let (mut all_placements, region_clips) = self.collect_apply_placements();\n',
)
replace_once(
    layout_path,
    '        let placements_unchanged = self.placements_match_last_applied(&all_placements);\n',
    '''        // Region ownership is verified for every clipped landing. Do not let\n        // an unchanged rectangle bypass recovery after an app replaces a region.\n        let placements_unchanged = region_clips.is_empty()\n            && self.placements_match_last_applied(&all_placements);\n''',
)
replace_once(
    layout_path,
    '        let (rx, worker_handle) = match self.spawn_apply_worker(all_placements) {\n',
    '        let (rx, worker_handle) = match self.spawn_apply_worker(all_placements, region_clips) {\n',
)
replace_once(
    layout_path,
    '''    fn collect_apply_placements(&self) -> Vec<leopardwm_core_layout::WindowPlacement> {\n        let mut all_placements = Vec::new();\n''',
    '''    fn collect_apply_placements(\n        &self,\n    ) -> (\n        Vec<leopardwm_core_layout::WindowPlacement>,\n        Vec<leopardwm_platform_win32::WindowRegionClip>,\n    ) {\n        let mut all_placements = Vec::new();\n        let mut region_clips = Vec::new();\n''',
)
replace_in_section(
    layout_path,
    'fn collect_apply_placements(',
    '/// Fast-path check:',
    '''                    park_offscreen_avoiding_neighbors(\n                        &mut placements,\n                        *monitor_id,\n                        focused_column,\n                        &self.monitors,\n                        &monitor_rects,\n                    );\n''',
    '''                    prepare_monitor_overflow(\n                        &mut placements,\n                        *monitor_id,\n                        focused_column,\n                        self.config.layout.monitor_overflow,\n                        &self.monitors,\n                        &monitor_rects,\n                        &mut region_clips,\n                    );\n''',
)
replace_in_section(
    layout_path,
    'fn collect_apply_placements(',
    '/// Fast-path check:',
    '        all_placements\n    }\n',
    '        (all_placements, region_clips)\n    }\n',
)
replace_once(
    layout_path,
    '''    fn spawn_apply_worker(\n        &mut self,\n        all_placements: Vec<leopardwm_core_layout::WindowPlacement>,\n    ) -> Result<(\n''',
    '''    fn spawn_apply_worker(\n        &mut self,\n        all_placements: Vec<leopardwm_core_layout::WindowPlacement>,\n        region_clips: Vec<leopardwm_platform_win32::WindowRegionClip>,\n    ) -> Result<(\n''',
)
replace_once(
    layout_path,
    '''                    match leopardwm_platform_win32::apply_placements(\n                        &all_placements,\n                        &platform_config,\n                        None,\n                        post_animation_nudge,\n                    ) {\n''',
    '''                    match leopardwm_platform_win32::apply_placements_with_regions(\n                        &all_placements,\n                        &region_clips,\n                        &platform_config,\n                        None,\n                        post_animation_nudge,\n                    ) {\n''',
)

# ---------------------------------------------------------------------------
# Animation worker carries region plans to the platform placement layer.
# ---------------------------------------------------------------------------
animation_path = 'crates/daemon/src/animation_worker.rs'
animation = read(animation_path)
frame_pattern = re.compile(
    r'(pub\(crate\) struct FrameRequest \{\n\s*pub placements: Vec<leopardwm_core_layout::WindowPlacement>,\n)'
)
animation, count = frame_pattern.subn(
    r'\1    pub region_clips: Vec<leopardwm_platform_win32::WindowRegionClip>,\n',
    animation,
    count=1,
)
if count != 1:
    raise RuntimeError('animation_worker.rs: FrameRequest marker mismatch')
animation = animation.replace(
    '''leopardwm_platform_win32::apply_placements(\n                    &request.placements,\n                    &request.platform_config,\n''',
    '''leopardwm_platform_win32::apply_placements_with_regions(\n                    &request.placements,\n                    &request.region_clips,\n                    &request.platform_config,\n''',
)
if 'apply_placements_with_regions' not in animation:
    raise RuntimeError('animation_worker.rs: placement call was not updated')
write(animation_path, animation)

# ---------------------------------------------------------------------------
# Platform placement integration. Existing callers keep the original wrapper;
# daemon frames opt into the region-aware variant.
# ---------------------------------------------------------------------------
placement_path = 'crates/platform_win32/src/placement.rs'
placement = read(placement_path)
placement = placement.replace(
    'use crate::window_id_to_hwnd;\n',
    '''use crate::window_id_to_hwnd;\nuse crate::window_region::{\n    apply_window_region_clip, can_clip_window_region, reconcile_window_regions,\n    restore_all_window_regions, restore_window_region, WindowRegionClip,\n};\n''',
    1,
)
placement = placement.replace(
    '''    flags: windows::Win32::UI::WindowsAndMessaging::SET_WINDOW_POS_FLAGS,\n    column_index: usize,\n}\n''',
    '''    flags: windows::Win32::UI::WindowsAndMessaging::SET_WINDOW_POS_FLAGS,\n    column_index: usize,\n    region_clip_bounds: Option<Rect>,\n    fallback_rect: Option<Rect>,\n    fallback_visibility: Option<Visibility>,\n}\n''',
    1,
)
old_signature = '''pub fn apply_placements(\n    placements: &[WindowPlacement],\n    config: &PlatformConfig,\n    mut cache: Option<&mut PlacementCache>,\n    nudge_sticky_compositors: bool,\n) -> Result<ApplyPlacementsResult, Win32Error> {\n'''
if placement.count(old_signature) != 1:
    raise RuntimeError('placement.rs: apply_placements signature mismatch')
new_signature = '''pub fn apply_placements(\n    placements: &[WindowPlacement],\n    config: &PlatformConfig,\n    cache: Option<&mut PlacementCache>,\n    nudge_sticky_compositors: bool,\n) -> Result<ApplyPlacementsResult, Win32Error> {\n    apply_placements_with_regions(\n        placements,\n        &[],\n        config,\n        cache,\n        nudge_sticky_compositors,\n    )\n}\n\npub fn apply_placements_with_regions(\n    placements: &[WindowPlacement],\n    region_clips: &[WindowRegionClip],\n    config: &PlatformConfig,\n    mut cache: Option<&mut PlacementCache>,\n    nudge_sticky_compositors: bool,\n) -> Result<ApplyPlacementsResult, Win32Error> {\n'''
placement = placement.replace(old_signature, new_signature)
placement = placement.replace(
    '''        // Uncloak all tracked windows — no placements means all previous\n        // windows have left this layout (e.g., workspace switch to empty workspace).\n        uncloak_all_tracked();\n''',
    '''        // Empty layout is also a hard region-lifecycle boundary.\n        restore_all_window_regions();\n        // Uncloak all tracked windows — no placements means all previous\n        // windows have left this layout (e.g., workspace switch to empty workspace).\n        uncloak_all_tracked();\n''',
    1,
)
placement = placement.replace(
    '''    let animation_frame = cache.is_some();\n\n    // Prepare all window entries''',
    '''    let animation_frame = cache.is_some();\n    let managed_window_ids: HashSet<WindowId> =\n        placements.iter().map(|placement| placement.window_id).collect();\n    let clipped_window_ids: HashSet<WindowId> =\n        region_clips.iter().map(|clip| clip.window_id).collect();\n    reconcile_window_regions(\n        &managed_window_ids,\n        &clipped_window_ids,\n        !animation_frame,\n    );\n\n    // Prepare all window entries''',
    1,
)
placement = placement.replace(
    '''    let (entries, skipped) = build_defer_entries(\n        placements,\n        &mut cache,\n''',
    '''    let (mut entries, skipped) = build_defer_entries(\n        placements,\n        region_clips,\n        &mut cache,\n''',
    1,
)
placement = placement.replace(
    '    let (applied, failed_window_ids) = position_entries(&entries);\n',
    '''    let (applied, mut failed_window_ids) = position_entries(&entries);\n    let region_fallbacks = apply_entry_region_clips(\n        &mut entries,\n        &mut failed_window_ids,\n        animation_frame,\n    );\n''',
    1,
)
old_cache = '''        // Update entries for windows that were actually positioned\n        let positioned: std::collections::HashSet<u64> = entries\n            .iter()\n            .filter(|e| !failed_window_ids.contains(&e.window_id))\n            .map(|e| e.window_id)\n            .collect();\n        for p in placements {\n            if positioned.contains(&p.window_id) {\n                cache.positions.insert(p.window_id, (p.rect, p.visibility));\n            }\n        }\n'''
new_cache = '''        // Record the effective entry. A region failure may have synchronously\n        // switched this HWND to its safe fallback geometry.\n        for entry in &entries {\n            if !failed_window_ids.contains(&entry.window_id) {\n                cache.positions.insert(\n                    entry.window_id,\n                    (entry.layout_rect, entry.visibility),\n                );\n            }\n        }\n'''
if placement.count(old_cache) != 1:
    raise RuntimeError('placement.rs: cache update block mismatch')
placement = placement.replace(old_cache, new_cache)
placement = placement.replace(
    '''        "Applied {} placements ({} skipped unchanged), {} off-screen total",\n        applied,\n        skipped,\n        offscreen_count,\n''',
    '''        "Applied {} placements ({} skipped unchanged), {} region fallback(s), {} off-screen total",\n        applied,\n        skipped,\n        region_fallbacks,\n        offscreen_count,\n''',
    1,
)
placement = placement.replace(
    '''fn build_defer_entries(\n    placements: &[WindowPlacement],\n    cache: &mut Option<&mut PlacementCache>,\n''',
    '''fn build_defer_entries(\n    placements: &[WindowPlacement],\n    region_clips: &[WindowRegionClip],\n    cache: &mut Option<&mut PlacementCache>,\n''',
    1,
)
loop_old = '''    for placement in placements {\n        let previous = cache\n            .as_ref()\n            .and_then(|cache| cache.positions.get(&placement.window_id).copied());\n        if previous == Some((placement.rect, placement.visibility)) {\n            skipped += 1;\n            continue;\n        }\n        let position_only = animation_move_is_position_only(previous, placement);\n        let Ok(hwnd) = window_id_to_hwnd(placement.window_id) else {\n            continue;\n        };\n'''
loop_new = '''    for requested in placements {\n        let region_clip = region_clips\n            .iter()\n            .find(|clip| clip.window_id == requested.window_id);\n        let clip_supported = region_clip\n            .is_some_and(|_| can_clip_window_region(requested.window_id));\n        let placement = if let Some(clip) = region_clip.filter(|_| !clip_supported) {\n            WindowPlacement {\n                window_id: requested.window_id,\n                rect: clip.fallback_rect,\n                visibility: clip.fallback_visibility,\n                column_index: requested.column_index,\n            }\n        } else {\n            requested.clone()\n        };\n        let previous = cache\n            .as_ref()\n            .and_then(|cache| cache.positions.get(&placement.window_id).copied());\n        // A requested clip is deliberately revalidated even when geometry is\n        // unchanged, so an application-owned region replacement is detected.\n        if region_clip.is_none() && previous == Some((placement.rect, placement.visibility)) {\n            skipped += 1;\n            continue;\n        }\n        let position_only = animation_move_is_position_only(previous, &placement);\n        let Ok(hwnd) = window_id_to_hwnd(placement.window_id) else {\n            continue;\n        };\n'''
if placement.count(loop_old) != 1:
    raise RuntimeError('placement.rs: build loop mismatch')
placement = placement.replace(loop_old, loop_new)
# The loop now owns a local WindowPlacement, so references need borrowing at one helper call.
placement = placement.replace(
    '            let (x, y) = offscreen_position(placement, inset_l, inset_t);\n',
    '            let (x, y) = offscreen_position(&placement, inset_l, inset_t);\n',
    1,
)
entry_tail = '''                flags,\n                column_index: placement.column_index,\n            });\n'''
entry_new = '''                flags,\n                column_index: placement.column_index,\n                region_clip_bounds: region_clip\n                    .filter(|_| clip_supported)\n                    .map(|clip| clip.clip_bounds),\n                fallback_rect: region_clip.map(|clip| clip.fallback_rect),\n                fallback_visibility: region_clip.map(|clip| clip.fallback_visibility),\n            });\n'''
if placement.count(entry_tail) != 2:
    raise RuntimeError(f'placement.rs: expected two entry tails, found {placement.count(entry_tail)}')
placement = placement.replace(entry_tail, entry_new)

region_helper = '''\nfn set_entry_to_fallback(entry: &mut DeferEntry, animation_frame: bool) -> bool {\n    let (Some(rect), Some(visibility)) = (entry.fallback_rect, entry.fallback_visibility) else {\n        return false;\n    };\n    let (inset_l, inset_t, inset_r, inset_b) = entry.used_insets;\n    entry.layout_rect = rect;\n    entry.visibility = visibility;\n    entry.region_clip_bounds = None;\n    entry.x = rect.x.saturating_sub(inset_l);\n    entry.y = rect.y.saturating_sub(inset_t);\n    if visibility == Visibility::Visible {\n        entry.w = rect.width.saturating_add(inset_l).saturating_add(inset_r);\n        entry.h = rect.height.saturating_add(inset_t).saturating_add(inset_b);\n        entry.flags = SWP_NOZORDER | SWP_NOACTIVATE;\n        if !animation_frame {\n            entry.flags |= SWP_FRAMECHANGED;\n        }\n    } else {\n        entry.w = 0;\n        entry.h = 0;\n        entry.flags = SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE;\n    }\n    unsafe {\n        SetWindowPos(\n            entry.hwnd,\n            None,\n            entry.x,\n            entry.y,\n            entry.w,\n            entry.h,\n            entry.flags,\n        )\n        .is_ok()\n    }\n}\n\n/// Commit requested regions after the HWND batch lands. A rare ownership or\n/// Win32 failure is converted immediately to the precomputed safe fallback so\n/// no frame is allowed to leak into a neighboring monitor.\nfn apply_entry_region_clips(\n    entries: &mut [DeferEntry],\n    failed_window_ids: &mut HashSet<u64>,\n    animation_frame: bool,\n) -> u32 {\n    let mut fallback_count = 0;\n    for entry in entries {\n        let Some(clip_bounds) = entry.region_clip_bounds else {\n            continue;\n        };\n        if failed_window_ids.contains(&entry.window_id) {\n            continue;\n        }\n        let outer_rect = Rect::new(entry.x, entry.y, entry.w.max(1), entry.h.max(1));\n        let result = apply_window_region_clip(\n            entry.window_id,\n            outer_rect,\n            entry.layout_rect,\n            clip_bounds,\n            !animation_frame,\n        );\n        if result.succeeded() {\n            continue;\n        }\n\n        let _ = restore_window_region(entry.window_id, false);\n        fallback_count += 1;\n        if !set_entry_to_fallback(entry, animation_frame) {\n            failed_window_ids.insert(entry.window_id);\n        }\n    }\n    fallback_count\n}\n\n'''
marker = '/// Uncloak entries becoming visible and drop them from the tracking set.\n'
if placement.count(marker) != 1:
    raise RuntimeError('placement.rs: region helper insertion marker mismatch')
placement = placement.replace(marker, region_helper + marker)
write(placement_path, placement)

# ---------------------------------------------------------------------------
# Region lifecycle recovery: position/off-screen transitions, shutdown and
# destroyed-HWND cleanup must never leave a stale shape behind.
# ---------------------------------------------------------------------------
visibility_path = 'crates/platform_win32/src/visibility.rs'
visibility = read(visibility_path)
for signature in [
    'pub fn move_window_offscreen(window_id: WindowId)',
    'pub fn restore_window_moved_offscreen(window_id: WindowId)',
    'pub fn position_window(window_id: WindowId, rect: Rect)',
]:
    at = visibility.find(signature)
    if at < 0:
        raise RuntimeError(f'visibility.rs: function not found: {signature}')
    brace = visibility.find('{', at)
    insertion = '\n    let _ = crate::window_region::restore_window_region(window_id, false);'
    visibility = visibility[: brace + 1] + insertion + visibility[brace + 1 :]
write(visibility_path, visibility)

# `dwm_uncloak_all` is the common shutdown/panic/emergency recovery path.
placement = read(placement_path)
replace_target = 'pub fn dwm_uncloak_all() {\n'
if placement.count(replace_target) != 1:
    raise RuntimeError('placement.rs: dwm_uncloak_all marker mismatch')
placement = placement.replace(
    replace_target,
    replace_target + '    restore_all_window_regions();\n',
)
write(placement_path, placement)

# Add destroy cleanup next to the existing recycled-handle cleanup.
cleanup_count = 0
for path in (ROOT / 'crates/daemon/src').rglob('*.rs'):
    text = path.read_text(encoding='utf-8')
    pattern = re.compile(
        r'(leopardwm_platform_win32::clear_suspected_oversize\(([^)]+)\);)'
    )
    def add_cleanup(match: re.Match[str]) -> str:
        nonlocal_cleanup[0] += 1
        value = match.group(2)
        return match.group(1) + f'\n        leopardwm_platform_win32::forget_window_region({value});'
    nonlocal_cleanup = [0]
    updated = pattern.sub(add_cleanup, text)
    if nonlocal_cleanup[0]:
        path.write_text(updated, encoding='utf-8', newline='\n')
        cleanup_count += nonlocal_cleanup[0]
if cleanup_count == 0:
    raise RuntimeError('daemon: no destroyed-window cleanup site found')

print('SetWindowRgn v12 integration patch applied')
