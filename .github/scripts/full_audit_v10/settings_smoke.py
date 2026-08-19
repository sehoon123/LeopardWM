from pathlib import Path
import runpy

# Apply the product-side popup fix before exercising the exact document that
# will be compiled into the daemon and committed after the quality gate.
runpy.run_path(
    '../control/.github/scripts/full_audit_v10/fix_settings_popup.py',
    run_name='__main__',
)

source = Path('crates/daemon/src/settings/settings.html').read_text(encoding='utf-8')
source = source.replace(
    '<script>',
    '<script>window.ipc={postMessage:function(){}};window._hotkeyCatalog=[];\n',
    1,
)
smoke = r'''
<script>
(function () {
  var failures = [];
  function check(ok, message) { if (!ok) failures.push(message); }
  try {
    init({
      layout: {
        gap: 10, outer_gap_left: 10, outer_gap_right: 10,
        outer_gap_top: 10, outer_gap_bottom: 10,
        default_floating_width: 800, default_floating_height: 600,
        default_scratchpad_width: 900, default_scratchpad_height: 600,
        remember_floating_sizes: true, remember_scratchpad_size: true,
        width_presets: [0.333, 0.5, 0.667], height_presets: [0.333, 0.5, 0.667],
        default_width_preset: 1, centering_mode: 'center', center_past_edges: true
      },
      appearance: {
        active_border: true, active_border_color: '0078D4', active_border_width: 2,
        active_border_position: 'inside', tab_strip_height: 28,
        tab_strip_bg: '202020', tab_strip_active_bg: '404040',
        tab_strip_active_text: 'FFFFFF', tab_strip_inactive_text: 'C0C0C0',
        tab_strip_opacity: 0.95
      },
      behavior: {
        focus_new_windows: true, track_focus_changes: true,
        focus_follows_mouse: false, focus_follows_mouse_delay_ms: 150,
        mouse_follows_focus: false, workspace_edge_wrap: false,
        fullscreen_follows_focus: true, disable_snap_layouts: true,
        cross_monitor_drag: true, drag_to_merge: true,
        compositor_safe_mode: true, swap_chain_ghost_animation: false,
        hide_offscreen_taskbar_buttons: true, check_for_updates: true,
        log_level: 'info', tab_close_action: 'close_window',
        new_window_placement: 'new_column'
      },
      hotkeys: { bindings: {}, scroll_modifier: 'Ctrl+Alt', disabled: [] },
      window_rules: [],
      gestures: {
        enabled: false, swipe_left: 'focus_left', swipe_right: 'focus_right',
        swipe_up: 'focus_up', swipe_down: 'focus_down',
        scroll_up: 'scroll_left', scroll_down: 'scroll_right'
      },
      snap_hints: { enabled: true, duration_ms: 800, opacity: 0.65 },
      animation: {
        layout_duration_ms: 150, workspace_switch_duration_ms: 200,
        scroll_duration_ms: 200, overview_duration_ms: 150,
        easing: 'ease_out', reduce_motion_on_battery: true
      },
      overview: { render: 'live' }, workspaces: { names: [] },
      high_contrast: false, auto_start: false,
      future_top_level: { preserve_me: true }
    });

    var navItems = Array.from(document.querySelectorAll('.nav-item[data-section]'));
    check(navItems.length > 0, 'no navigation items');
    navItems.forEach(function (item) {
      item.click();
      var section = document.getElementById('sec-' + item.dataset.section);
      check(item.classList.contains('active'), 'nav not active: ' + item.dataset.section);
      check(section && section.classList.contains('active'), 'section not active: ' + item.dataset.section);
    });

    var rulesNav = document.querySelector('.nav-item[data-section="rules"]');
    check(!!rulesNav, 'window-rules navigation item missing');
    if (rulesNav) rulesNav.click();

    addRuleRow({
      match_title: 'Smoke', match_class: 'Chrome_WidgetWin_1',
      match_executable: 'msedge.exe', action: 'float',
      width: 800, height: 600, column_width: 0.5,
      open_on_workspace: 2, open_in_column: 1,
      open_maximized: false, sticky: false, corner_style: 'rounded'
    });
    var optionButton = document.querySelector('.rule-opts-btn');
    check(!!optionButton, 'window-rule options button missing');
    if (optionButton) optionButton.click();
    var popup = document.querySelector('.rule-opts.open .rule-opts-pop');
    check(!!popup, 'window-rule options popup missing');
    if (popup) {
      var rect = popup.getBoundingClientRect();
      check(rect.left >= -1, 'popup clips left');
      check(rect.right <= window.innerWidth + 1, 'popup clips right');
      check(rect.top >= -1, 'popup clips top');
      check(rect.bottom <= window.innerHeight + 1, 'popup clips bottom');
    }

    check(!!document.getElementById('behavior-check_for_updates'), 'update toggle missing');
    check(!!document.getElementById('layout-center_past_edges'), 'center-past-edges toggle missing');
    check(document.documentElement.scrollWidth <= window.innerWidth + 1, 'document horizontal overflow');
    check(document.body.scrollWidth <= window.innerWidth + 1, 'body horizontal overflow');
    var roundTrip = readConfig();
    check(roundTrip.behavior.check_for_updates === true, 'update preference did not round-trip');
    check(roundTrip.layout.center_past_edges === true, 'centering preference did not round-trip');
    check(roundTrip.future_top_level && roundTrip.future_top_level.preserve_me === true,
          'unknown top-level setting was discarded');
    check(roundTrip.window_rules[0].width === 800, 'rule width did not round-trip');
    check(roundTrip.window_rules[0].height === 600, 'rule height did not round-trip');
    check(roundTrip.window_rules[0].column_width === 0.5, 'rule column width did not round-trip');
  } catch (error) {
    failures.push(error && error.stack ? error.stack : String(error));
  }
  var result = document.createElement('pre');
  result.id = 'settings-smoke-result';
  result.textContent = failures.length ? 'FAIL\n' + failures.join('\n') : 'PASS';
  document.body.appendChild(result);
}());
</script>
'''
source = source.replace('</body>', smoke + '\n</body>', 1)
Path('../settings-smoke-v10.html').write_text(source, encoding='utf-8')
