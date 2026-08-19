from pathlib import Path

script_path = Path('../control/.github/v12/apply_window_region_v12.py')
script = script_path.read_text(encoding='utf-8')

old = '''control_id = 'id="layout-center_past_edges"'
pos = settings.find(control_id)
if pos < 0:
    raise RuntimeError('settings.html: center-past-edges control not found')
next_row = settings.find('<div class="setting-row">', pos)
if next_row < 0:
    raise RuntimeError('settings.html: next layout setting row not found')
monitor_row = ''' + "'''" + '''        <div class="setting-row">\\n          <div class="setting-info">\\n            <div class="setting-label">Monitor overflow</div>\\n            <div class="setting-description">Keep neighboring columns partially visible while preventing them from painting on another monitor.</div>\\n          </div>\\n          <select id="layout-monitor_overflow">\\n            <option value="clip">Clip at monitor edge</option>\\n            <option value="hide">Hide whole window</option>\\n          </select>\\n        </div>\\n\\n''' + "'''" + '''
if 'id="layout-monitor_overflow"' in settings:
    raise RuntimeError('settings.html: monitor overflow control already exists')
settings = settings[:next_row] + monitor_row + settings[next_row:]
'''

new = '''center_field = ''' + "'''" + '''          <div class="field">\\n            <div class="field-info"><div class="field-label">Center past edges</div><div class="field-desc">Allow centering to scroll past content boundaries</div></div>\\n            <label class="toggle"><input type="checkbox" id="layout-center_past_edges"><span class="track"></span><span class="thumb"></span></label>\\n          </div>\\n''' + "'''" + '''
monitor_field = ''' + "'''" + '''          <div class="field">\\n            <div class="field-info"><div class="field-label">Monitor overflow</div><div class="field-desc">Show partial neighboring columns without painting on another monitor</div></div>\\n            <select id="layout-monitor_overflow">\\n              <option value="clip">Clip at monitor edge</option>\\n              <option value="hide">Hide whole window</option>\\n            </select>\\n          </div>\\n''' + "'''" + '''
if settings.count(center_field) != 1:
    raise RuntimeError('settings.html: center-past-edges field mismatch')
if 'id="layout-monitor_overflow"' in settings:
    raise RuntimeError('settings.html: monitor overflow control already exists')
settings = settings.replace(center_field, center_field + monitor_field)
'''

if script.count(old) != 1:
    raise RuntimeError('base integration script does not contain the expected Settings block')
script = script.replace(old, new)

old_save = '''save_marker = "center_past_edges: checked('layout-center_past_edges'),"
if settings.count(save_marker) != 1:
    raise RuntimeError('settings.html: center-past-edges save marker mismatch')
settings = settings.replace(
    save_marker,
    save_marker + "\\n          monitor_overflow: document.getElementById('layout-monitor_overflow').value,",
)
'''
new_save = '''save_marker = "center_past_edges: checked('layout-center_past_edges')"
if settings.count(save_marker) != 1:
    raise RuntimeError('settings.html: center-past-edges save marker mismatch')
settings = settings.replace(
    save_marker,
    save_marker + ",\\n          monitor_overflow: document.getElementById('layout-monitor_overflow').value",
)
'''
if script.count(old_save) != 1:
    raise RuntimeError('base integration script does not contain the expected save block')
script = script.replace(old_save, new_save)

old_animation = '''frame_pattern = re.compile(
    r'(pub\\(crate\\) struct FrameRequest \\{\\n\\s*pub placements: Vec<leopardwm_core_layout::WindowPlacement>,\\n)'
)
animation, count = frame_pattern.subn(
    r'\\1    pub region_clips: Vec<leopardwm_platform_win32::WindowRegionClip>,\\n',
    animation,
    count=1,
)
if count != 1:
    raise RuntimeError('animation_worker.rs: FrameRequest marker mismatch')
'''
new_animation = '''frame_marker = "    pub placements: Vec<WindowPlacement>,\\n"
if animation.count(frame_marker) != 1:
    raise RuntimeError('animation_worker.rs: FrameRequest placements marker mismatch')
animation = animation.replace(
    frame_marker,
    frame_marker + "    pub region_clips: Vec<leopardwm_platform_win32::WindowRegionClip>,\\n",
    1,
)
'''
if script.count(old_animation) != 1:
    raise RuntimeError('base integration script does not contain the expected animation block')
script = script.replace(old_animation, new_animation)

old_animation_call = '''animation = animation.replace(
    ''' + "'''" + '''leopardwm_platform_win32::apply_placements(\\n                    &request.placements,\\n                    &request.platform_config,\\n''' + "'''" + ''',
    ''' + "'''" + '''leopardwm_platform_win32::apply_placements_with_regions(\\n                    &request.placements,\\n                    &request.region_clips,\\n                    &request.platform_config,\\n''' + "'''" + ''',
)
if 'apply_placements_with_regions' not in animation:
    raise RuntimeError('animation_worker.rs: placement call was not updated')
'''
new_animation_call = '''call_marker = "leopardwm_platform_win32::apply_placements("
if animation.count(call_marker) != 1:
    raise RuntimeError('animation_worker.rs: placement call marker mismatch')
animation = animation.replace(
    call_marker,
    "leopardwm_platform_win32::apply_placements_with_regions(",
    1,
)
argument_marker = "                        &request.placements,\\n"
if animation.count(argument_marker) != 1:
    raise RuntimeError('animation_worker.rs: placements argument marker mismatch')
animation = animation.replace(
    argument_marker,
    argument_marker + "                        &request.region_clips,\\n",
    1,
)
'''
if script.count(old_animation_call) != 1:
    raise RuntimeError('base integration script does not contain the expected animation call block')
script = script.replace(old_animation_call, new_animation_call)

compiled = compile(script, str(script_path), 'exec')
exec(compiled, {'__name__': '__main__', '__file__': str(script_path)})
