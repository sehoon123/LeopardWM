from pathlib import Path


def read(path: str) -> str:
    return Path(path).read_text(encoding="utf-8")


def write(path: str, text: str) -> None:
    Path(path).write_text(text, encoding="utf-8", newline="\n")


def replace_once(path: str, old: str, new: str) -> None:
    text = read(path)
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: expected one match, found {count}: {old[:120]!r}")
    write(path, text.replace(old, new, 1))


def remove_between(path: str, start: str, end: str) -> None:
    text = read(path)
    start_at = text.find(start)
    end_at = text.find(end, start_at + len(start))
    if start_at < 0 or end_at < 0:
        raise RuntimeError(f"{path}: section markers not found")
    write(path, text[:start_at] + text[end_at:])


# ---------------------------------------------------------------------------
# Win32 region ownership: GetWindowRgn returns ERROR when a valid window has no
# region. NULLREGION means an explicitly empty region. The previous inversion
# made ordinary windows fail the clip preflight and fall back to whole-window
# hiding, which produced 50/25 instead of 25/50/25 on a rightmost monitor.
# ---------------------------------------------------------------------------
region_path = "crates/platform_win32/src/window_region.rs"
replace_once(
    region_path,
    "const ERROR_REGION_KIND: i32 = 0;\nconst NULL_REGION_KIND: i32 = 1;\n",
    "const ERROR_REGION_KIND: i32 = 0;\nconst NULL_REGION_KIND: i32 = 1;\nconst SIMPLE_REGION_KIND: i32 = 2;\nconst COMPLEX_REGION_KIND: i32 = 3;\n",
)
replace_once(
    region_path,
    '''fn current_region_kind(hwnd: HWND) -> i32 {
    let Some(region) = create_region(Rect::new(0, 0, 1, 1)) else {
        return ERROR_REGION_KIND;
    };
    let kind = unsafe { GetWindowRgn(hwnd, region) };
    delete_region(region);
    kind.0
}
''',
    '''#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowRegionKind {
    /// ERROR means no region for this normal case. NULLREGION means an
    /// explicitly empty region, not the absence of a region.
    NoRegion,
    Empty,
    Simple,
    Complex,
}

fn classify_window_region_kind(raw: i32) -> Option<WindowRegionKind> {
    match raw {
        ERROR_REGION_KIND => Some(WindowRegionKind::NoRegion),
        NULL_REGION_KIND => Some(WindowRegionKind::Empty),
        SIMPLE_REGION_KIND => Some(WindowRegionKind::Simple),
        COMPLEX_REGION_KIND => Some(WindowRegionKind::Complex),
        _ => None,
    }
}

fn current_region_kind(hwnd: HWND) -> Option<WindowRegionKind> {
    let region = create_region(Rect::new(0, 0, 1, 1))?;
    let raw = unsafe { GetWindowRgn(hwnd, region) }.0;
    delete_region(region);
    classify_window_region_kind(raw)
}
''',
)
replace_once(
    region_path,
    '''fn window_has_no_region(hwnd: HWND) -> bool {
    current_region_kind(hwnd) == NULL_REGION_KIND
}
''',
    '''fn window_has_no_region(hwnd: HWND) -> bool {
    matches!(current_region_kind(hwnd), Some(WindowRegionKind::NoRegion))
}
''',
)

# The placement path performs the region operation once after the atomic HWND
# batch. Keep all ownership checks in that transaction rather than retaining a
# second, racy preflight query that doubles GDI work at every monitor edge.
remove_between(
    region_path,
    "/// Whether an HWND can be safely clipped without replacing an application-owned\n",
    "/// Install or update a LeopardWM-owned region. `redraw` should be false for an\n",
)

# Extend the existing test module with a real Win32 HWND test and exact
# 25/50/25 + 12.5/75/12.5 clipping geometry tests.
replace_once(
    region_path,
    '''mod tests {
    use super::{decode_coordinate, encode_coordinate, relative_clip_region};
    use leopardwm_core_layout::Rect;
''',
    '''mod tests {
    use super::{
        apply_window_region_clip, classify_window_region_kind, decode_coordinate,
        encode_coordinate, relative_clip_region, restore_window_region, window_has_no_region,
        WindowRegionKind, ERROR_REGION_KIND,
    };
    use leopardwm_core_layout::Rect;
    use std::sync::OnceLock;
    use windows::core::w;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassExW, WNDCLASSEXW,
        WINDOW_EX_STYLE, WS_OVERLAPPED,
    };

    unsafe extern "system" fn test_wndproc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    }

    fn test_window() -> HWND {
        static REGISTERED: OnceLock<()> = OnceLock::new();
        let instance = unsafe { GetModuleHandleW(None).unwrap() };
        REGISTERED.get_or_init(|| {
            let class = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(test_wndproc),
                hInstance: instance.into(),
                lpszClassName: w!("LeopardWMPreviewRegionTest"),
                ..Default::default()
            };
            assert_ne!(unsafe { RegisterClassExW(&class) }, 0);
        });
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("LeopardWMPreviewRegionTest"),
                w!(""),
                WS_OVERLAPPED,
                0,
                0,
                1000,
                800,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .unwrap()
        }
    }

    fn window_id(hwnd: HWND) -> u64 {
        hwnd.0 as usize as u64
    }

    struct TestWindow(HWND);

    impl TestWindow {
        fn new() -> Self {
            Self(test_window())
        }
    }

    impl Drop for TestWindow {
        fn drop(&mut self) {
            let _ = restore_window_region(window_id(self.0), false);
            unsafe {
                let _ = DestroyWindow(self.0);
            }
        }
    }
''',
)
replace_once(
    region_path,
    '''    #[test]
    fn coordinate_property_encoding_round_trips_extremes() {
''',
    '''    #[test]
    fn get_window_rgn_error_is_the_normal_unowned_state() {
        assert_eq!(
            classify_window_region_kind(ERROR_REGION_KIND),
            Some(WindowRegionKind::NoRegion)
        );

        let window = TestWindow::new();
        let id = window_id(window.0);
        assert!(window_has_no_region(window.0));

        let result = apply_window_region_clip(
            id,
            Rect::new(0, 0, 1000, 800),
            Rect::new(0, 0, 1000, 800),
            Rect::new(0, 0, 250, 800),
            false,
        );
        assert!(result.succeeded());
        assert!(restore_window_region(id, false));
        assert!(window_has_no_region(window.0));
    }

    #[test]
    fn centered_preview_regions_are_symmetric() {
        let viewport = Rect::new(0, 0, 1000, 800);
        for (column_width, preview_width) in [(500, 250), (750, 125)] {
            let left = Rect::new(preview_width - column_width, 0, column_width, 800);
            let right = Rect::new(1000 - preview_width, 0, column_width, 800);

            assert_eq!(
                relative_clip_region(left, left, viewport),
                Some(Rect::new(
                    column_width - preview_width,
                    0,
                    preview_width,
                    800,
                ))
            );
            assert_eq!(
                relative_clip_region(right, right, viewport),
                Some(Rect::new(0, 0, preview_width, 800))
            );
        }
    }

    #[test]
    fn coordinate_property_encoding_round_trips_extremes() {
''',
)

# ---------------------------------------------------------------------------
# Placement path: remove the duplicate region preflight. A clip is attempted
# exactly once after the atomic position batch; only a definitive failure uses
# the already-computed same-pass fallback. This halves region-query work for
# boundary windows and prevents transient preflight failures from hiding them.
# ---------------------------------------------------------------------------
placement_path = "crates/platform_win32/src/placement.rs"
replace_once(
    placement_path,
    '''use crate::window_region::{
    apply_window_region_clip, can_clip_window_region, reconcile_window_regions,
    restore_all_window_regions, restore_window_region, WindowRegionClip,
};
''',
    '''use crate::window_region::{
    apply_window_region_clip, reconcile_window_regions, restore_all_window_regions,
    restore_window_region, WindowRegionClip,
};
''',
)
replace_once(
    placement_path,
    '''    for requested in placements {
        let region_clip = region_clips
            .iter()
            .find(|clip| clip.window_id == requested.window_id);
        let clip_supported =
            region_clip.is_some_and(|_| can_clip_window_region(requested.window_id));
        let placement = if let Some(clip) = region_clip.filter(|_| !clip_supported) {
            WindowPlacement {
                window_id: requested.window_id,
                rect: clip.fallback_rect,
                visibility: clip.fallback_visibility,
                column_index: requested.column_index,
            }
        } else {
            requested.clone()
        };
''',
    '''    for requested in placements {
        let region_clip = region_clips
            .iter()
            .find(|clip| clip.window_id == requested.window_id);
        let placement = requested.clone();
''',
)
text = read(placement_path)
old = '''                region_clip_bounds: region_clip
                    .filter(|_| clip_supported)
                    .map(|clip| clip.clip_bounds),'''
count = text.count(old)
if count != 2:
    raise RuntimeError(f"{placement_path}: expected two clip-supported mappings, found {count}")
text = text.replace(old, '''                region_clip_bounds: region_clip.map(|clip| clip.clip_bounds),''')
write(placement_path, text)

# ---------------------------------------------------------------------------
# Core layout contract: centering must expose equal previews on both sides.
# ---------------------------------------------------------------------------
center_path = "crates/core_layout/tests/edge_centering.rs"
replace_once(
    center_path,
    "use leopardwm_core_layout::{CenteringMode, Rect, Workspace};\n",
    "use leopardwm_core_layout::{CenteringMode, Rect, Visibility, Workspace};\n",
)
center = read(center_path)
center += '''

fn assert_symmetric_preview(column_width: i32, expected_preview: i32) {
    let viewport = Rect::new(0, 0, 1000, 800);
    let mut workspace = Workspace::with_gaps(0, 0);
    for window_id in 1..=3 {
        workspace.insert_window(window_id, Some(column_width)).unwrap();
    }
    workspace.set_centering_mode(CenteringMode::Center);
    workspace.set_center_past_edges(true);
    workspace.set_focus(1, 0).unwrap();
    workspace.ensure_focused_visible(viewport.width);

    let mut placements = workspace.compute_placements(viewport);
    placements.sort_by_key(|placement| placement.window_id);
    assert_eq!(placements.len(), 3);
    assert!(placements
        .iter()
        .all(|placement| placement.visibility == Visibility::Visible));

    let visible_widths: Vec<i32> = placements
        .iter()
        .map(|placement| {
            placement.rect.right().min(viewport.right())
                - placement.rect.x.max(viewport.x)
        })
        .collect();
    assert_eq!(
        visible_widths,
        vec![expected_preview, column_width, expected_preview]
    );
    assert_eq!(placements[1].rect.x + column_width / 2, viewport.width / 2);
}

#[test]
fn centered_half_width_columns_show_25_50_25() {
    assert_symmetric_preview(500, 250);
}

#[test]
fn centered_three_quarter_width_columns_show_12_5_75_12_5() {
    assert_symmetric_preview(750, 125);
}
'''
write(center_path, center)

# ---------------------------------------------------------------------------
# Daemon policy contract: clip mode keeps all preview placements visible and
# emits a clip plan only for the side that intersects a physical neighbor.
# ---------------------------------------------------------------------------
edge_path = "crates/daemon/src/layout_apply_edge_tests.rs"
replace_once(
    edge_path,
    "use super::park_offscreen_avoiding_neighbors;\n",
    "use super::{park_offscreen_avoiding_neighbors, prepare_monitor_overflow};\nuse crate::config::MonitorOverflowModeConfig;\n",
)
edge = read(edge_path)
edge += '''

fn visible_width(rect: Rect, viewport: Rect) -> i32 {
    rect.right().min(viewport.right()) - rect.x.max(viewport.x)
}

fn assert_clip_preview_distribution(column_width: i32, expected_preview: i32) {
    // The owner is the rightmost physical monitor. The left preview intersects
    // monitor 1 and needs a region; the right preview extends only into empty
    // virtual-desktop space and must remain visible without a region.
    let monitors = side_by_side_monitors();
    let owner = monitors[&2].rect;
    let monitor_rects: Vec<_> = monitors.values().map(|monitor| monitor.rect).collect();
    let focused_x = owner.x + expected_preview;
    let mut placements = vec![
        WindowPlacement {
            window_id: 20,
            rect: Rect::new(focused_x - column_width, 0, column_width, 800),
            visibility: Visibility::Visible,
            column_index: 0,
        },
        WindowPlacement {
            window_id: 21,
            rect: Rect::new(focused_x, 0, column_width, 800),
            visibility: Visibility::Visible,
            column_index: 1,
        },
        WindowPlacement {
            window_id: 22,
            rect: Rect::new(focused_x + column_width, 0, column_width, 800),
            visibility: Visibility::Visible,
            column_index: 2,
        },
    ];
    let original = placements.clone();
    let mut clips = Vec::new();

    prepare_monitor_overflow(
        &mut placements,
        2,
        Some(1),
        MonitorOverflowModeConfig::Clip,
        &monitors,
        &monitor_rects,
        &mut clips,
    );

    assert_eq!(placements, original);
    assert!(placements
        .iter()
        .all(|placement| placement.visibility == Visibility::Visible));
    assert_eq!(
        placements
            .iter()
            .map(|placement| visible_width(placement.rect, owner))
            .collect::<Vec<_>>(),
        vec![expected_preview, column_width, expected_preview]
    );
    assert_eq!(clips.len(), 1);
    assert_eq!(clips[0].window_id, 20);
    assert_eq!(clips[0].clip_bounds, owner);
}

#[test]
fn clip_mode_preserves_25_50_25_on_a_rightmost_monitor() {
    assert_clip_preview_distribution(960, 480);
}

#[test]
fn clip_mode_preserves_12_5_75_12_5_on_a_rightmost_monitor() {
    assert_clip_preview_distribution(1440, 240);
}
'''
write(edge_path, edge)

print("symmetric preview v11 patch applied")
