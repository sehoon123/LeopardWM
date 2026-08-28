//! Embedded HTML/CSS/JS for the WebView2-based settings panel.
//!
//! WinUI 3 / Fluent Design System v2 single-page app with NavigationView
//! sidebar, seven settings sections, and IPC back to the Rust host.
//! Color tokens, typography, spacing, and component styling match the
//! official WinUI 3 theme resources.

pub const SETTINGS_HTML: &str = include_str!("settings.html");

#[cfg(test)]
mod tests {
    use super::SETTINGS_HTML;

    #[test]
    fn reduce_motion_on_battery_is_wired_into_settings_config() {
        assert!(SETTINGS_HTML.contains("id=\"animation-reduce_motion_on_battery\""));
        assert!(SETTINGS_HTML.contains(
            "setChecked('animation-reduce_motion_on_battery', anim.reduce_motion_on_battery !== false);"
        ));
        assert!(SETTINGS_HTML
            .contains("reduce_motion_on_battery: checked('animation-reduce_motion_on_battery')"));
    }

    #[test]
    fn floating_and_scratchpad_sizes_are_wired_into_settings_config() {
        for (field, default) in [
            ("floating_width", 800),
            ("floating_height", 600),
            ("scratchpad_width", 900),
            ("scratchpad_height", 600),
        ] {
            let id = format!("layout-default_{field}");
            assert!(
                SETTINGS_HTML.contains(&format!("<input type=\"number\" id=\"{id}\" min=\"1\">")),
                "missing settings input {id}"
            );
            assert!(
                SETTINGS_HTML.contains(&format!(
                    "setVal('{id}', cfg.layout.default_{field} || {default});"
                )),
                "missing settings initialization for {field}"
            );
            assert!(
                SETTINGS_HTML.contains(&format!("default_{field}: num('{id}')")),
                "missing settings serialization for {field}"
            );
        }
    }

    #[test]
    fn session_size_memory_and_drag_behavior_are_wired() {
        for field in ["remember_floating_sizes", "remember_scratchpad_size"] {
            let id = format!("layout-{field}");
            assert!(SETTINGS_HTML.contains(&format!("id=\"{id}\"")));
            assert!(SETTINGS_HTML.contains(&format!(
                "setChecked('{id}', cfg.layout.{field} !== false);"
            )));
            assert!(SETTINGS_HTML.contains(&format!("{field}: checked('{id}')")));
        }
        assert!(SETTINGS_HTML.contains("id=\"behavior-cross_monitor_drag\""));
        assert!(SETTINGS_HTML.contains(
            "setChecked('behavior-cross_monitor_drag', cfg.behavior.cross_monitor_drag !== false);"
        ));
        assert!(
            SETTINGS_HTML.contains("cross_monitor_drag: checked('behavior-cross_monitor_drag')")
        );
        assert!(SETTINGS_HTML.contains("id=\"behavior-drag_to_merge\""));
        assert!(SETTINGS_HTML.contains(
            "setChecked('behavior-drag_to_merge', cfg.behavior.drag_to_merge !== false);"
        ));
        assert!(SETTINGS_HTML.contains("drag_to_merge: checked('behavior-drag_to_merge')"));
        assert!(SETTINGS_HTML.contains(r#"id="behavior-compositor_safe_mode""#));
        assert!(SETTINGS_HTML.contains(
            "setChecked('behavior-compositor_safe_mode', cfg.behavior.compositor_safe_mode !== false);"
        ));
        assert!(SETTINGS_HTML
            .contains("compositor_safe_mode: checked('behavior-compositor_safe_mode')"));
    }

    #[test]
    fn update_check_is_editable_from_the_gui() {
        assert!(SETTINGS_HTML.contains("window._initConfig = cfg;"));
        assert!(SETTINGS_HTML.contains("id=\"behavior-check_for_updates\""));
        assert!(SETTINGS_HTML.contains(
            "setChecked('behavior-check_for_updates', cfg.behavior.check_for_updates !== false);"
        ));
        assert!(
            SETTINGS_HTML.contains("check_for_updates: checked('behavior-check_for_updates')"),
            "the update-check toggle must be written back instead of echoing the loaded value"
        );
    }

    #[test]
    fn every_config_backed_control_is_read_back_on_save() {
        // Guard against a control that renders but silently drops its value:
        // each id below must appear in the markup and in `readConfig`.
        for (id, reader) in [
            (
                "behavior-check_for_updates",
                "checked('behavior-check_for_updates')",
            ),
            (
                "behavior-hide_offscreen_taskbar_buttons",
                "checked('behavior-hide_offscreen_taskbar_buttons')",
            ),
            (
                "behavior-swap_chain_ghost_animation",
                "checked('behavior-swap_chain_ghost_animation')",
            ),
            (
                "behavior-fullscreen_follows_focus",
                "checked('behavior-fullscreen_follows_focus')",
            ),
            (
                "behavior-workspace_edge_wrap",
                "checked('behavior-workspace_edge_wrap')",
            ),
            (
                "behavior-mouse_follows_focus",
                "checked('behavior-mouse_follows_focus')",
            ),
        ] {
            assert!(
                SETTINGS_HTML.contains(&format!("id=\"{id}\"")),
                "missing control {id}"
            );
            assert!(
                SETTINGS_HTML.contains(reader),
                "control {id} is never read back on save"
            );
        }
    }

    #[test]
    fn gestures_can_be_turned_off_individually_from_the_gui() {
        assert!(SETTINGS_HTML.contains(">No action</div>'"));
        assert!(SETTINGS_HTML
            .contains("var currentLabel = current === '' ? 'No action' : cmdLabel(current);"));
        for axis in [
            "swipe_left",
            "swipe_right",
            "swipe_up",
            "swipe_down",
            "scroll_up",
            "scroll_down",
        ] {
            assert!(
                SETTINGS_HTML.contains(&format!(
                    "setCb('cb-gestures-{axis}', cfg.gestures.{axis}, true);"
                )),
                "gesture {axis} must keep an unlisted or empty action visible"
            );
        }
    }

    #[test]
    fn behavior_section_is_grouped_into_labeled_subsections() {
        for heading in [
            "<h3 class=\"section-subtitle\">Startup and updates</h3>",
            "<h3 class=\"section-subtitle\">Focus</h3>",
            "<h3 class=\"section-subtitle\">Windows and dragging</h3>",
            "<h3 class=\"section-subtitle\">Rendering</h3>",
            "<h3 class=\"section-subtitle\">Diagnostics</h3>",
            "<h3 class=\"section-subtitle\">Animation</h3>",
        ] {
            assert!(SETTINGS_HTML.contains(heading), "missing heading {heading}");
        }
        // Each grouped control must exist exactly once: a duplicated id would
        // make the second copy read stale values back into the config.
        for id in [
            "cb-behavior-log_level",
            "cb-behavior-tab_close_action",
            "cb-overview-render",
            "behavior-compositor_safe_mode",
        ] {
            assert_eq!(
                SETTINGS_HTML.matches(&format!("id=\"{id}\"")).count(),
                1,
                "control {id} must appear exactly once"
            );
        }
    }

    #[test]
    fn every_section_explains_itself() {
        for section in [
            "sec-layout",
            "sec-appearance",
            "sec-behavior",
            "sec-hotkeys",
            "sec-rules",
            "sec-gestures",
            "sec-snaphints",
        ] {
            let start = SETTINGS_HTML
                .find(&format!("id=\"{section}\""))
                .unwrap_or_else(|| panic!("missing section {section}"));
            let body = &SETTINGS_HTML[start..];
            let end = body.find("</h2>").unwrap_or(body.len());
            let after_title = &body[end..];
            assert!(
                after_title
                    .get(..400)
                    .unwrap_or(after_title)
                    .contains("class=\"section-desc\""),
                "section {section} has no description under its title"
            );
        }
    }

    #[test]
    fn settings_saves_include_a_revision_and_handle_conflicts() {
        for marker in [
            "window._configRevision = cfg.config_revision || '';",
            "action: 'save', revision: window._configRevision || '', config: readConfig()",
            "function handleSaveResult(result)",
            "External settings change kept.",
        ] {
            assert!(
                SETTINGS_HTML.contains(marker),
                "missing optimistic-save marker: {marker}"
            );
        }
    }

    #[test]
    fn default_width_preset_is_a_bounded_dynamic_select() {
        assert!(SETTINGS_HTML.contains("class=\"card\" id=\"default-width-preset-card\""));
        assert!(SETTINGS_HTML
            .contains("<select id=\"layout-default_width_preset\" aria-label=\"Default width preset for new windows\"></select>"));
        assert!(
            !SETTINGS_HTML.contains("<input type=\"number\" id=\"layout-default_width_preset\"")
        );
        assert!(SETTINGS_HTML
            .contains("refreshDefaultWidthPresetOptions(cfg.layout.default_width_preset || 1);"));
        assert!(SETTINGS_HTML.contains("option.presetRow = entry.row;"));
        assert!(SETTINGS_HTML.contains("'Preset ' + (index + 1) + ' ('"));
        assert!(SETTINGS_HTML
            .contains("row.querySelector('.row-delete').disabled = row === onlyValidRow;"));
        assert!(SETTINGS_HTML.contains("widthPresets = lastValidWidthPresets.slice();"));
        assert!(SETTINGS_HTML.contains("default_width_preset: defaultWidthPreset"));
    }
}

#[cfg(test)]
mod monitor_overflow_settings_tests {
    use super::SETTINGS_HTML;

    #[test]
    fn monitor_overflow_control_is_complete_and_round_trippable() {
        for marker in [
            "id=\"layout-monitor_overflow\"",
            "value=\"clip\"",
            "value=\"hide\"",
            "cfg.layout.monitor_overflow || 'clip'",
            "monitor_overflow: document.getElementById('layout-monitor_overflow').value",
        ] {
            assert!(
                SETTINGS_HTML.contains(marker),
                "missing Settings marker: {marker}"
            );
        }
    }
}
