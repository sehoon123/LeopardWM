from pathlib import Path

root = Path.cwd()
path = root / "crates/daemon/src/layout_apply_edge_tests.rs"
text = path.read_text(encoding="utf-8")
old = "use super::park_offscreen_avoiding_neighbors;\n"
if text.count(old) != 1:
    raise RuntimeError("edge test import marker mismatch")
new = """use super::park_offscreen_avoiding_neighbors as apply_monitor_overflow;

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
path.write_text(text.replace(old, new), encoding="utf-8", newline="\n")
print("external edge tests adapted to the hide fallback contract")
