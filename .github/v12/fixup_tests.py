from pathlib import Path
import shutil

root = Path.cwd()
control = root.parent / "control" / ".github" / "v12"

# Adapt the existing monitor-isolation tests to exercise the explicit Hide mode.
legacy = root / "crates/daemon/src/layout_apply_edge_tests.rs"
text = legacy.read_text(encoding="utf-8")
old = "use super::park_offscreen_avoiding_neighbors;\n"
if text.count(old) != 1:
    raise RuntimeError("legacy edge test import marker mismatch")
wrapper = """use super::park_offscreen_avoiding_neighbors as apply_monitor_overflow;

fn park_offscreen_avoiding_neighbors(
    placements: &mut [WindowPlacement],
    owner_id: MonitorId,
    focused_column: Option<usize>,
    monitors: &HashMap<MonitorId, MonitorInfo>,
    monitor_rects: &[Rect],
) {
    apply_monitor_overflow(
        placements,
        owner_id,
        focused_column,
        crate::config::MonitorOverflowConfig::Hide,
        monitors,
        monitor_rects,
        &mut Vec::new(),
    );
}
"""
legacy.write_text(text.replace(old, wrapper), encoding="utf-8", newline="\n")

# Install dedicated Clip-mode policy tests.
shutil.copyfile(
    control / "layout_apply_region_tests.rs",
    root / "crates/daemon/src/layout_apply_region_tests.rs",
)
layout = root / "crates/daemon/src/layout_apply.rs"
text = layout.read_text(encoding="utf-8")
marker = '#[path = "layout_apply_region_tests.rs"]\nmod monitor_region_policy_tests;'
if marker not in text:
    text += (
        '\n#[cfg(test)]\n'
        '#[path = "layout_apply_region_tests.rs"]\n'
        'mod monitor_region_policy_tests;\n'
    )
layout.write_text(text, encoding="utf-8", newline="\n")
print("SetWindowRgn v12 test fixtures installed")
