from pathlib import Path
import shutil

root = Path.cwd()
control = root.parent / "control" / ".github" / "real-v10"
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
print("monitor-region policy tests installed")
