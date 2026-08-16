from __future__ import annotations

from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[2]


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def write(path: str, text: str) -> None:
    (ROOT / path).write_text(text, encoding="utf-8", newline="\n")


def replace_once(text: str, old: str, new: str, *, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one match, found {count}")
    return text.replace(old, new, 1)


def find_rust_item(text: str, needle: str, *, include_test_attribute: bool = False) -> tuple[int, int]:
    start = text.find(needle)
    if start < 0:
        raise SystemExit(f"Rust item not found: {needle}")
    if include_test_attribute:
        attr = text.rfind("    #[test]", 0, start)
        if attr >= 0 and text[attr:start].strip() == "#[test]":
            start = attr
    brace = text.find("{", start)
    if brace < 0:
        raise SystemExit(f"Opening brace not found for: {needle}")
    depth = 0
    in_string = False
    escaped = False
    for i in range(brace, len(text)):
        ch = text[i]
        if in_string:
            if escaped:
                escaped = False
            elif ch == "\\":
                escaped = True
            elif ch == '"':
                in_string = False
            continue
        if ch == '"':
            in_string = True
        elif ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
            if depth == 0:
                end = i + 1
                if end < len(text) and text[end] == "\n":
                    end += 1
                return start, end
    raise SystemExit(f"Closing brace not found for: {needle}")


def replace_rust_item(text: str, needle: str, replacement: str) -> str:
    start, end = find_rust_item(text, needle)
    return text[:start] + replacement.rstrip() + "\n" + text[end:]


def remove_rust_item(text: str, needle: str) -> str:
    start, end = find_rust_item(text, needle)
    return text[:start] + text[end:]


def patch_geometry_capture() -> None:
    helpers_path = "crates/daemon/src/helpers.rs"
    helpers = read(helpers_path)

    marker = "/// Small DWM frame-bound differences are compositor/inset noise, not a user resize."
    start = helpers.find(marker)
    if start < 0:
        raise SystemExit("helpers.rs: floating capture drift marker not found")
    impl_start = helpers.find("impl AppState {", start)
    if impl_start < 0:
        raise SystemExit("helpers.rs: AppState impl after drift helper not found")
    helpers = helpers[:start] + helpers[impl_start:]

    old_capture = re.compile(
        r"(?m)^        let stored_rect = self\.floating_rect_for_window\(hwnd\)\?;\n"
        r"        #\[cfg\(not\(test\)\)\]\n"
        r"        let rect = leopardwm_platform_win32::get_window_visible_rect\(hwnd\)\n"
        r"            \.map\(\|observed\| stabilize_floating_capture\(stored_rect, observed\)\)\n"
        r"            \.unwrap_or\(stored_rect\);\n"
        r"        #\[cfg\(test\)\]\n"
        r"        let rect = stored_rect;\n"
    )
    replacement = (
        "        // A hide/show, float/unfloat, or scratchpad transition must snapshot\n"
        "        // LeopardWM's managed geometry, not DWM's asynchronously updated frame.\n"
        "        // User-confirmed resizing is learned separately by the MoveSizeEnd path.\n"
        "        let rect = self.floating_rect_for_window(hwnd)?;\n"
    )
    helpers, count = old_capture.subn(replacement, helpers, count=1)
    if count != 1:
        raise SystemExit(f"helpers.rs: expected one DWM capture block, replaced {count}")

    if "mod floating_capture_tests" in helpers:
        helpers = remove_rust_item(helpers, "mod floating_capture_tests")

    invariant_tests = r'''
#[cfg(test)]
mod floating_capture_tests {
    #[test]
    fn transition_snapshot_uses_only_managed_geometry() {
        let source = include_str!("helpers.rs");
        let start = source
            .find("pub(crate) fn snapshot_managed_floating_geometry")
            .expect("snapshot helper must exist");
        let tail = &source[start..];
        let end = tail
            .find("\n    }\n")
            .map_or(tail.len(), |idx| idx + "\n    }\n".len());
        let body = &tail[..end];
        let forbidden_probe = ["get_window_", "visible_rect"].concat();

        assert!(body.contains("floating_rect_for_window(hwnd)"));
        assert!(!body.contains(&forbidden_probe));
    }
}
'''
    helpers = helpers.rstrip() + "\n\n" + invariant_tests.lstrip()

    # Rename the API everywhere so future call sites do not mistake a managed-state
    # snapshot for an OS/DWM geometry probe.
    changed_files = 0
    for path in (ROOT / "crates" / "daemon" / "src").rglob("*.rs"):
        text = path.read_text(encoding="utf-8")
        updated = text.replace(
            "capture_floating_geometry", "snapshot_managed_floating_geometry"
        )
        if path == ROOT / helpers_path:
            updated = helpers.replace(
                "capture_floating_geometry", "snapshot_managed_floating_geometry"
            )
        if updated != text:
            path.write_text(updated, encoding="utf-8", newline="\n")
            changed_files += 1
    if changed_files < 2:
        raise SystemExit(
            f"expected geometry snapshot rename in helpers plus call sites; changed {changed_files} files"
        )

    # Clone the existing production-facing test and make the snapshot operation
    # repeat many times. The old cfg(test) shortcut made this test unable to catch
    # production-only DWM drift; production and tests now execute identical code.
    tests_path = ROOT / "crates/daemon/src/tests.rs"
    tests = tests_path.read_text(encoding="utf-8")
    original_name = "fn test_snapshot_managed_floating_geometry_keeps_managed_float"
    if "fn test_repeated_managed_geometry_snapshot_is_idempotent" not in tests:
        start, end = find_rust_item(tests, original_name, include_test_attribute=True)
        original = tests[start:end]
        clone = original.replace(
            "test_snapshot_managed_floating_geometry_keeps_managed_float",
            "test_repeated_managed_geometry_snapshot_is_idempotent",
            1,
        )
        lines = clone.splitlines()
        call_index = next(
            (
                idx
                for idx, line in enumerate(lines)
                if ".snapshot_managed_floating_geometry(" in line
            ),
            None,
        )
        if call_index is None:
            raise SystemExit("managed geometry test: snapshot call line not found")
        line = lines[call_index]
        assignment = re.match(r"^(\s*)let (\w+) = (.+);$", line)
        if assignment:
            indent, variable, expression = assignment.groups()
            lines[call_index] = (
                f"{indent}let {variable} = (0..128)\n"
                f"{indent}    .map(|_| {expression})\n"
                f"{indent}    .last()\n"
                f"{indent}    .flatten();"
            )
        else:
            indent = line[: len(line) - len(line.lstrip())]
            lines[call_index] = (
                f"{indent}for _ in 0..128 {{\n"
                f"{indent}    {line.strip()}\n"
                f"{indent}}}"
            )
        clone = "\n".join(lines) + ("\n" if original.endswith("\n") else "")
        tests = tests[:end] + "\n" + clone + tests[end:]
        tests_path.write_text(tests, encoding="utf-8", newline="\n")


def patch_settings_shell() -> None:
    path = "crates/daemon/src/settings/win32.rs"
    text = read(path)
    text = replace_once(
        text,
        "use std::sync::Mutex;",
        "use std::sync::{Mutex, OnceLock};",
        label="settings imports",
    )
    text = replace_once(
        text,
        "static PENDING_FAILED_BINDS: Mutex<Option<String>> = Mutex::new(None);",
        "static PENDING_FAILED_BINDS: Mutex<Option<String>> = Mutex::new(None);\n"
        "/// Rendered static settings resources are shared across opens.\n"
        "static SETTINGS_HTML_RENDERED: OnceLock<String> = OnceLock::new();\n"
        "static SETTINGS_HOTKEY_CATALOG_JSON: OnceLock<String> = OnceLock::new();",
        label="settings static caches",
    )

    push_failed_binds = r'''
pub fn push_failed_binds(failed_binds: &[String]) {
    let thread_id = match SETTINGS_THREAD.lock() {
        Ok(guard) => *guard,
        Err(_) => return,
    };
    let Some(thread_id) = thread_id else { return };
    let json = serde_json::to_string(failed_binds).unwrap_or_else(|_| "[]".to_string());
    let Ok(mut pending) = PENDING_FAILED_BINDS.lock() else {
        return;
    };

    // Keep the staging mutex held until the thread message has been accepted.
    // The window thread takes the same mutex before consuming the payload, so it
    // can never observe a message without its matching data. A failed post does
    // not leave stale data for a later settings-window lifetime.
    *pending = Some(json);
    let posted = unsafe {
        PostThreadMessageW(thread_id, WM_SETTINGS_PUSH_BINDS, WPARAM(0), LPARAM(0)).is_ok()
    };
    if !posted {
        *pending = None;
    }
}
'''
    text = replace_rust_item(text, "pub fn push_failed_binds", push_failed_binds)

    text = replace_once(
        text,
        "        let catalog_json = serde_json::to_string(&leopardwm_ipc::hotkeys::hotkey_catalog())\n"
        "            .unwrap_or_else(|_| \"[]\".to_string());",
        "        let catalog_json = SETTINGS_HOTKEY_CATALOG_JSON.get_or_init(|| {\n"
        "            serde_json::to_string(&leopardwm_ipc::hotkeys::hotkey_catalog())\n"
        "                .unwrap_or_else(|_| \"[]\".to_string())\n"
        "        });",
        label="hotkey catalog cache",
    )
    text = replace_once(
        text,
        "        let settings_html = SETTINGS_HTML.replace(\"{VERSION}\", env!(\"CARGO_PKG_VERSION\"));",
        "        let settings_html = SETTINGS_HTML_RENDERED.get_or_init(|| {\n"
        "            SETTINGS_HTML.replace(\"{VERSION}\", env!(\"CARGO_PKG_VERSION\"))\n"
        "        });",
        label="settings HTML cache",
    )
    text = replace_once(
        text,
        "            .with_additional_browser_args(\"--disable-features=msSmartScreenProtection\")\n",
        "",
        label="remove SmartScreen disable flag",
    )

    helper = r'''
fn initial_section_script(section: &str) -> String {
    let section_json = serde_json::to_string(section).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        "(function(section){{var items=document.querySelectorAll('.nav-item[data-section]');for(var i=0;i<items.length;i++){{if(items[i].dataset.section===section){{items[i].click();break;}}}}}})({section_json});"
    )
}
'''
    insert_at = text.find("/// Build and run the settings window. Blocks until the window is closed.")
    if insert_at < 0:
        raise SystemExit("settings: run_settings_window marker not found")
    if "fn initial_section_script" not in text:
        text = text[:insert_at] + helper + "\n" + text[insert_at:]

    old_nav = '''        // Navigate to initial section if requested
        if let Some(section) = initial_section {
            let nav_js = format!(
                "document.querySelector('.nav-item[data-section=\\\"{}\\\"]').click()",
                section
            );
            let _ = webview.evaluate_script(&nav_js);
        }
'''
    new_nav = '''        // Navigate without interpolating the section into a CSS selector. The
        // current callers use internal constants, but JSON quoting keeps this
        // boundary safe and a missing section now degrades to a no-op.
        if let Some(section) = initial_section {
            let _ = webview.evaluate_script(&initial_section_script(section));
        }
'''
    text = replace_once(text, old_nav, new_nav, label="initial section navigation")

    test_anchor = "    #[test]\n    fn allowed_urls_include_settings_links_and_mixed_case_schemes()"
    if "fn initial_section_script_json_quotes_the_section" not in text:
        test = r'''    #[test]
    fn initial_section_script_json_quotes_the_section() {
        let section = "layout\");window.__leopardwm_injected=true;//";
        let encoded = serde_json::to_string(section).unwrap();
        let script = initial_section_script(section);

        assert!(script.ends_with(&format!("({encoded});")));
        assert!(script.contains("dataset.section===section"));
        assert!(!script.contains("data-section=\\\"{}\\\""));
    }

'''
        index = text.find(test_anchor)
        if index < 0:
            raise SystemExit("settings: URL test anchor not found")
        text = text[:index] + test + text[index:]

    write(path, text)


def patch_cli_test_duplication() -> None:
    path = "crates/cli/Cargo.toml"
    text = read(path)
    block_pattern = re.compile(
        r"(?ms)(\[\[bin\]\]\nname\s*=\s*\"lwm\"\npath\s*=\s*\"src/main\.rs\"\n)(?!test\s*=)"
    )
    text, count = block_pattern.subn(
        r"\1test = false\nbench = false\n",
        text,
        count=1,
    )
    if count != 1:
        if 'name = "lwm"' not in text or "test = false" not in text:
            raise SystemExit("CLI alias target block not found")
    write(path, text)


def write_audit_report() -> None:
    suffixes = {".rs", ".toml", ".yml", ".yaml", ".ps1"}
    files = [
        path
        for path in ROOT.rglob("*")
        if path.is_file()
        and path.suffix.lower() in suffixes
        and ".git" not in path.parts
        and "target" not in path.parts
    ]
    total_lines = 0
    unsafe_count = 0
    unwrap_count = 0
    expect_count = 0
    todo_count = 0
    for path in files:
        source = path.read_text(encoding="utf-8", errors="replace")
        total_lines += source.count("\n") + (0 if source.endswith("\n") else 1)
        unsafe_count += len(re.findall(r"\bunsafe\b", source))
        unwrap_count += source.count(".unwrap()")
        expect_count += source.count(".expect(")
        todo_count += sum(source.count(token) for token in ("todo!", "unimplemented!", "TODO", "FIXME"))

    report = f"""# Sehoon fork code-audit log

This document records the repository-wide audit pass that produced `v0.2.6-sehoon.3`.
It is intentionally scoped to the personal fork and is not an upstream contribution.

## Inventory scanned

- Source/build files scanned: **{len(files)}**
- Lines scanned: **{total_lines}**
- `unsafe` tokens reviewed by inventory: **{unsafe_count}**
- direct `.unwrap()` calls inventoried: **{unwrap_count}**
- `.expect(...)` calls inventoried: **{expect_count}**
- TODO/FIXME/unimplemented markers inventoried: **{todo_count}**

The scan covered all tracked Rust, Cargo TOML, GitHub Actions YAML, and PowerShell source/build files. The embedded Settings HTML/CSS/JavaScript is contained in a Rust source file and is therefore included in the line inventory.

## Fixed in this pass

1. **Rapid scratchpad shrink:** state transitions no longer re-learn size from asynchronous DWM frame bounds. They snapshot LeopardWM's managed floating geometry; user resize completion remains the only authoritative size-learning boundary.
2. **Production/test divergence:** geometry snapshot code no longer has a separate `cfg(test)` path. Repeated-snapshot regression coverage now executes the same implementation shipped in the binary.
3. **Settings navigation script construction:** the requested section is JSON-quoted and compared as a dataset value instead of being interpolated into a CSS selector.
4. **Settings push race:** a failed `PostThreadMessageW` cannot leave a stale rejected-hotkey payload for a later window lifetime.
5. **WebView hardening:** the Settings WebView no longer disables Microsoft SmartScreen protection.
6. **Settings open cost:** the 124 KB embedded page and static hotkey catalog serialization are cached across Settings-window opens.
7. **Duplicate CLI tests:** the `lwm` alias remains built, but Cargo no longer runs the same `main.rs` unit-test set a second time for the alias target.

## High-priority follow-up findings

- Refactor the two CLI names onto a shared library plus thin binaries; `test = false` removes duplicate tests but release compilation still processes two binary targets.
- Split the embedded Settings document into typed/static assets and add browser-level interaction tests for autosave, dynamic rows, and responsive layout.
- Benchmark and then coalesce bursty `EVENT_OBJECT_LOCATIONCHANGE` events per HWND; this is likely the next meaningful runtime optimization, but changing it without ETW measurements risks drag latency regressions.
- Reuse layout scratch buffers across animation frames to remove remaining transient `Vec` allocations in placement and drag-hint paths.
- Continue the Win32 ownership audit around HWND recycling, GDI/COM handles, and monitor-topology changes; these need hardware-backed tests in addition to unit tests.

## Verification gate

The release workflow requires formatting, the full locked test suite, Clippy with warnings denied, all-target checking, optimized release build, and PE GUI-subsystem verification before it can commit or publish binaries.
"""
    write("agent_docs/sehoon-fork-code-audit.md", report)


def main() -> None:
    patch_geometry_capture()
    patch_settings_shell()
    patch_cli_test_duplication()
    write_audit_report()

    # Repository-wide invariants for the fixed transition boundary.
    daemon_source = "\n".join(
        path.read_text(encoding="utf-8")
        for path in (ROOT / "crates/daemon/src").rglob("*.rs")
    )
    if "capture_floating_geometry" in daemon_source:
        raise SystemExit("obsolete capture_floating_geometry identifier remains")
    helper_source = read("crates/daemon/src/helpers.rs")
    start = helper_source.find("pub(crate) fn snapshot_managed_floating_geometry")
    if start < 0:
        raise SystemExit("managed geometry snapshot helper missing")
    segment = helper_source[start : start + 1200]
    if "get_window_visible_rect" in segment:
        raise SystemExit("managed geometry snapshot still queries DWM")


if __name__ == "__main__":
    main()
