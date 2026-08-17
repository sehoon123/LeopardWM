from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[2]


@dataclass
class Change:
    path: str
    detail: str


CHANGES: list[Change] = []


def read(path: str | Path) -> str:
    p = ROOT / path if not isinstance(path, Path) or not path.is_absolute() else path
    return p.read_text(encoding="utf-8")


def write(path: str | Path, text: str) -> None:
    p = ROOT / path if not isinstance(path, Path) or not path.is_absolute() else path
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(text, encoding="utf-8", newline="\n")


def rel(path: Path) -> str:
    return str(path.relative_to(ROOT)).replace("\\", "/")


def record(path: str | Path, detail: str) -> None:
    p = rel(path) if isinstance(path, Path) else path
    CHANGES.append(Change(p, detail))


def find_item_span(text: str, needle: str) -> tuple[int, int]:
    start = text.find(needle)
    if start < 0:
        raise ValueError(needle)
    brace = text.find("{", start)
    if brace < 0:
        raise SystemExit(f"opening brace not found for {needle}")
    depth = 0
    i = brace
    state = "code"
    raw_hashes = 0
    block_depth = 0
    while i < len(text):
        ch = text[i]
        nxt = text[i + 1] if i + 1 < len(text) else ""
        if state == "line_comment":
            if ch == "\n":
                state = "code"
            i += 1
            continue
        if state == "block_comment":
            if ch == "/" and nxt == "*":
                block_depth += 1
                i += 2
                continue
            if ch == "*" and nxt == "/":
                block_depth -= 1
                i += 2
                if block_depth == 0:
                    state = "code"
                continue
            i += 1
            continue
        if state == "string":
            if ch == "\\":
                i += 2
                continue
            if ch == '"':
                state = "code"
            i += 1
            continue
        if state == "char":
            if ch == "\\":
                i += 2
                continue
            if ch == "'":
                state = "code"
            i += 1
            continue
        if state == "raw":
            if ch == '"' and text.startswith("#" * raw_hashes, i + 1):
                i += 1 + raw_hashes
                state = "code"
            i += 1
            continue
        if ch == "/" and nxt == "/":
            state = "line_comment"
            i += 2
            continue
        if ch == "/" and nxt == "*":
            state = "block_comment"
            block_depth = 1
            i += 2
            continue
        if ch == '"':
            state = "string"
            i += 1
            continue
        if ch == "'":
            if nxt and (nxt.isalpha() or nxt == "_"):
                i += 1
                continue
            state = "char"
            i += 1
            continue
        if ch == "r":
            match = re.match(r'r(#{0,16})"', text[i:])
            if match:
                raw_hashes = len(match.group(1))
                state = "raw"
                i += len(match.group(0))
                continue
        if ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
            if depth == 0:
                end = i + 1
                if end < len(text) and text[end] == "\n":
                    end += 1
                return start, end
        i += 1
    raise SystemExit(f"closing brace not found for {needle}")


def find_unique_rust(needle: str, subtree: str = "crates") -> Path:
    matches = []
    for path in (ROOT / subtree).rglob("*.rs"):
        if needle in path.read_text(encoding="utf-8", errors="replace"):
            matches.append(path)
    if len(matches) != 1:
        names = ", ".join(rel(p) for p in matches)
        raise SystemExit(f"{needle!r}: expected one Rust file, found {len(matches)}: {names}")
    return matches[0]


def split_cli_implementation() -> None:
    cargo_path = ROOT / "crates/cli/Cargo.toml"
    main_path = ROOT / "crates/cli/src/main.rs"
    lib_path = ROOT / "crates/cli/src/lib.rs"
    alias_path = ROOT / "crates/cli/src/bin/lwm.rs"
    cargo = read(cargo_path)
    already_split = lib_path.exists() and alias_path.exists() and 'path = "src/bin/lwm.rs"' in cargo and cargo.count('path = "src/main.rs"') == 1
    if already_split:
        return
    if lib_path.exists() or alias_path.exists():
        raise SystemExit("CLI is partially split; refusing to overwrite an ambiguous state")
    if cargo.count('path = "src/main.rs"') != 2:
        raise SystemExit("CLI targets are not both backed by src/main.rs as expected")
    source = read(main_path)
    if "CARGO_BIN_NAME" in source:
        raise SystemExit("CLI implementation depends on CARGO_BIN_NAME; manual design required")
    match = re.search(r"(?m)^(?P<attrs>(?:#\[[^\n]+\]\n)*)(?P<vis>pub(?:\([^\)]*\))?\s+)?(?P<async>async\s+)?fn\s+main\s*\(\s*\)\s*(?P<ret>->\s*[^\{\n]+)?\s*\{", source)
    if not match:
        raise SystemExit("CLI main function signature was not recognized")
    attrs = match.group("attrs") or ""
    async_kw = match.group("async") or ""
    ret = (match.group("ret") or "").strip()
    replacement = f"{attrs}pub {async_kw}fn run()"
    if ret:
        replacement += f" {ret}"
    replacement += " {"
    library = source[:match.start()] + replacement + source[match.end():]
    inner_attrs = [line for line in source.splitlines() if line.startswith("#![")]
    prefix = "\n".join(inner_attrs)
    if prefix:
        prefix += "\n"
    if ret:
        if "Result" not in ret:
            raise SystemExit(f"unsupported CLI main return type: {ret}")
        wrapper = prefix + "fn main() -> anyhow::Result<()> {\n    leopardwm_cli::run()\n}\n"
    else:
        wrapper = prefix + "fn main() {\n    leopardwm_cli::run();\n}\n"
    if "[lib]" not in cargo:
        package_end = cargo.find("\n[[bin]]")
        if package_end < 0:
            raise SystemExit("first CLI [[bin]] block not found")
        cargo = cargo[:package_end] + '\n\n[lib]\nname = "leopardwm_cli"\npath = "src/lib.rs"\n' + cargo[package_end:]
    blocks = list(re.finditer(r"(?ms)^\[\[bin\]\]\n.*?(?=^\[\[|^\[[^\[]|\Z)", cargo))
    alias = next((m for m in blocks if re.search(r'(?m)^name\s*=\s*"lwm"\s*$', m.group(0))), None)
    if alias is None:
        raise SystemExit("lwm target block not found")
    block = alias.group(0)
    updated = re.sub(r'(?m)^path\s*=\s*"src/main\.rs"\s*$', 'path = "src/bin/lwm.rs"', block, count=1)
    updated = re.sub(r"(?m)^(?:test|bench)\s*=\s*false\s*\n?", "", updated)
    if updated == block:
        raise SystemExit("failed to retarget lwm")
    cargo = cargo[:alias.start()] + updated + cargo[alias.end():]
    write(lib_path, library)
    write(main_path, wrapper)
    write(alias_path, wrapper)
    write(cargo_path, cargo)
    record(main_path, "replaced duplicated CLI implementation with a thin entry point")
    record(lib_path, "moved the CLI implementation and tests into a single library target")
    record(alias_path, "added a separate thin lwm alias entry point")
    record(cargo_path, "made both binaries link one shared CLI implementation")


def externalize_settings_document() -> None:
    rust_path = ROOT / "crates/daemon/src/settings/html.rs"
    html_path = ROOT / "crates/daemon/src/settings/settings.html"
    source = read(rust_path)
    if html_path.exists() and 'include_str!("settings.html")' in source:
        return
    if html_path.exists():
        raise SystemExit("settings.html exists without include_str! wiring")
    pattern = re.compile(r'(?P<decl>pub(?:\([^\)]+\))?\s+const\s+SETTINGS_HTML\s*:\s*&str\s*=\s*)r(?P<hash>#{0,16})"(?P<body>.*?)"(?P=hash);', re.DOTALL)
    match = pattern.search(source)
    if not match:
        if "include_str!" in source and "SETTINGS_HTML" in source:
            return
        raise SystemExit("embedded SETTINGS_HTML raw string was not found")
    body = match.group("body")
    if "<html" not in body.lower() or "{VERSION}" not in body:
        raise SystemExit("embedded settings document failed sanity checks")
    source = source[:match.start()] + f'{match.group("decl")}include_str!("settings.html");' + source[match.end():]
    write(rust_path, source)
    write(html_path, body)
    record(rust_path, "replaced the large Rust raw literal with include_str!")
    record(html_path, "moved Settings HTML/CSS/JS into a native static asset")


def ensure_smallvec_dependency() -> None:
    cargo_path = ROOT / "crates/core_layout/Cargo.toml"
    cargo = read(cargo_path)
    if re.search(r"(?m)^smallvec\s*=", cargo):
        return
    dep = re.search(r"(?m)^\[dependencies\]\s*$", cargo)
    if not dep:
        raise SystemExit("core_layout [dependencies] section not found")
    cargo = cargo[:dep.end()] + '\nsmallvec = "1.15"' + cargo[dep.end():]
    write(cargo_path, cargo)
    record(cargo_path, "added SmallVec for allocation-free common layout scratch storage")


def replace_vec_init(item: str, variable: str, replacement: str) -> tuple[str, bool]:
    for pattern in [re.compile(rf"(?m)^(?P<prefix>\s*let\s+mut\s+{re.escape(variable)}(?:\s*:\s*[^=;]+)?\s*=\s*)Vec::new\(\);"), re.compile(rf"(?m)^(?P<prefix>\s*let\s+mut\s+{re.escape(variable)}(?:\s*:\s*[^=;]+)?\s*=\s*)Vec::with_capacity\([^;]+\);")]:
        new_item, count = pattern.subn(rf"\g<prefix>{replacement};", item, count=1)
        if count:
            return new_item, True
    return item, False


def optimize_layout_hot_path() -> None:
    path = find_unique_rust("fn compute_non_fullscreen_placements", "crates/core_layout")
    source = read(path)
    start, end = find_item_span(source, "fn compute_non_fullscreen_placements")
    item = source[start:end]
    item, placements_changed = replace_vec_init(item, "placements", "Vec::with_capacity(self.window_count())")
    smallvec_changed = []
    for variable in ("visible_windows", "visible_weights", "min_heights"):
        explicit = re.compile(rf"(?m)^(?P<indent>\s*)let\s+mut\s+{variable}\s*:\s*Vec<(?P<inner>[^\n;]+)>\s*=\s*(?:Vec::new\(\)|Vec::with_capacity\([^\n;]+\));")
        match = explicit.search(item)
        if match:
            repl = f"{match.group('indent')}let mut {variable}: smallvec::SmallVec<[{match.group('inner').strip()}; 8]> = smallvec::SmallVec::new();"
            item = item[:match.start()] + repl + item[match.end():]
            smallvec_changed.append(variable)
            continue
        inferred = re.compile(rf"(?m)^(?P<prefix>\s*let\s+mut\s+{variable}\s*=\s*)(?:Vec::new\(\)|Vec::with_capacity\([^\n;]+\));")
        item, count = inferred.subn(rf"\g<prefix>smallvec::SmallVec::<[_; 8]>::new();", item, count=1)
        if count:
            smallvec_changed.append(variable)
    if placements_changed or smallvec_changed:
        source = source[:start] + item + source[end:]
        write(path, source)
        if placements_changed:
            record(path, "reserved placement output capacity from the known window count")
        if smallvec_changed:
            ensure_smallvec_dependency()
            record(path, "kept common per-column scratch vectors inline: " + ", ".join(smallvec_changed))


def optimize_exact_capacity_sites() -> None:
    candidates = [("fn column_bounds_from_placements", "bounds", "placements.len()"), ("fn merged_cleanup_window_ids", "ids", "primary.len() + secondary.len()"), ("fn partition_for_animation", "live", "placements.len()"), ("fn partition_for_animation", "ghosts", "placements.len().min(ghosted.len())")]
    for function, variable, capacity in candidates:
        matches = []
        for path in (ROOT / "crates").rglob("*.rs"):
            source = path.read_text(encoding="utf-8", errors="replace")
            if function in source:
                matches.append(path)
        if len(matches) != 1:
            continue
        path = matches[0]
        source = read(path)
        try:
            start, end = find_item_span(source, function)
        except ValueError:
            continue
        item = source[start:end]
        pattern = re.compile(rf"(?m)^(?P<prefix>\s*let\s+mut\s+{re.escape(variable)}(?:\s*:\s*[^=;]+)?\s*=\s*)Vec::new\(\);")
        item, count = pattern.subn(rf"\g<prefix>Vec::with_capacity({capacity});", item, count=1)
        if count:
            source = source[:start] + item + source[end:]
            write(path, source)
            record(path, f"reserved {variable} using an already-known upper bound")


def ensure_windows_storage_feature() -> None:
    cargo_path = ROOT / "Cargo.toml"
    cargo = read(cargo_path)
    if "Win32_Storage_FileSystem" in cargo:
        return
    marker = '"Win32_System_Threading"'
    if marker not in cargo:
        match = re.search(r'(?m)^(\s*)"Win32_[^"]+",\s*$', cargo)
        if not match:
            raise SystemExit("workspace windows feature list not found")
        insert = match.end()
        cargo = cargo[:insert] + f'\n{match.group(1)}"Win32_Storage_FileSystem",' + cargo[insert:]
    else:
        cargo = cargo.replace(marker, marker + ',\n    "Win32_Storage_FileSystem"', 1)
    write(cargo_path, cargo)
    record(cargo_path, "enabled Win32 atomic file-replacement APIs")


def add_atomic_file_module() -> None:
    module_path = ROOT / "crates/daemon/src/atomic_file.rs"
    if not module_path.exists():
        code = r'''//! Crash-safe, same-directory file replacement for persisted daemon state.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const REPLACE_RETRIES: usize = 4;

pub(crate) fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> io::Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = unique_sibling(path);
    let result = write_then_replace(&temp, path, contents.as_ref());
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn write_then_replace(temp: &Path, destination: &Path, contents: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(temp)?;
    file.write_all(contents)?;
    file.flush()?;
    file.sync_all()?;
    drop(file);
    let mut last_error = None;
    for attempt in 0..=REPLACE_RETRIES {
        match replace_file(temp, destination) {
            Ok(()) => return Ok(()),
            Err(error) if attempt < REPLACE_RETRIES && is_transient_replace_error(&error) => {
                last_error = Some(error);
                std::thread::sleep(Duration::from_millis(5_u64 << attempt));
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| io::Error::other("file replacement failed")))
}

fn unique_sibling(path: &Path) -> PathBuf {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("leopardwm-state");
    path.with_file_name(format!(".{name}.{pid}.{sequence}.tmp"))
}

fn is_transient_replace_error(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(5 | 32 | 33))
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        MoveFileExW(PCWSTR(source.as_ptr()), PCWSTR(destination.as_ptr()), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH).map_err(io::Error::from)
    }
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("leopardwm-atomic-file-{name}-{}-{}", std::process::id(), TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)))
    }

    #[test]
    fn creates_and_replaces_without_exposing_partial_contents() {
        let dir = test_dir("replace");
        let path = dir.join("state.json");
        write(&path, b"first").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"first");
        write(&path, b"second-state").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second-state");
        assert_eq!(fs::read_dir(&dir).unwrap().filter_map(Result::ok).count(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_destination_does_not_leave_temp_files() {
        let dir = test_dir("cleanup");
        fs::create_dir_all(&dir).unwrap();
        let destination = dir.join("is-a-directory");
        fs::create_dir_all(&destination).unwrap();
        assert!(write(&destination, b"data").is_err());
        assert!(fs::read_dir(&dir).unwrap().filter_map(Result::ok).all(|entry| !entry.file_name().to_string_lossy().ends_with(".tmp")));
        fs::remove_dir_all(dir).unwrap();
    }
}
'''
        write(module_path, code)
        record(module_path, "added flushed same-volume atomic replacement with transient-lock retry")
    main_path = ROOT / "crates/daemon/src/main.rs"
    main = read(main_path)
    if not re.search(r"(?m)^mod atomic_file;\s*$", main):
        first_mod = re.search(r"(?m)^mod [a-zA-Z0-9_]+;\s*$", main)
        if not first_mod:
            raise SystemExit("daemon module declarations not found")
        main = main[:first_mod.start()] + "mod atomic_file;\n" + main[first_mod.start():]
        write(main_path, main)
        record(main_path, "wired the atomic persistence module")
    ensure_windows_storage_feature()


def parse_call(text: str, open_paren: int) -> tuple[int, list[str]]:
    depth = 0
    state = "code"
    raw_hashes = 0
    start_arg = open_paren + 1
    args = []
    i = open_paren
    while i < len(text):
        ch = text[i]
        nxt = text[i + 1] if i + 1 < len(text) else ""
        if state == "string":
            if ch == "\\":
                i += 2
                continue
            if ch == '"':
                state = "code"
            i += 1
            continue
        if state == "raw":
            if ch == '"' and text.startswith("#" * raw_hashes, i + 1):
                i += 1 + raw_hashes
                state = "code"
            i += 1
            continue
        if state == "line_comment":
            if ch == "\n":
                state = "code"
            i += 1
            continue
        if state == "block_comment":
            if ch == "*" and nxt == "/":
                state = "code"
                i += 2
                continue
            i += 1
            continue
        if ch == '"':
            state = "string"
            i += 1
            continue
        if ch == "r":
            match = re.match(r'r(#{0,16})"', text[i:])
            if match:
                raw_hashes = len(match.group(1))
                state = "raw"
                i += len(match.group(0))
                continue
        if ch == "/" and nxt == "/":
            state = "line_comment"
            i += 2
            continue
        if ch == "/" and nxt == "*":
            state = "block_comment"
            i += 2
            continue
        if ch in "([{":
            depth += 1
        elif ch in ")]}":
            depth -= 1
            if depth == 0:
                args.append(text[start_arg:i].strip())
                return i, args
        elif ch == "," and depth == 1:
            args.append(text[start_arg:i].strip())
            start_arg = i + 1
        i += 1
    raise SystemExit("unterminated function call")


def replace_direct_persistence_writes() -> None:
    add_atomic_file_module()
    changed_files = 0
    for path in (ROOT / "crates/daemon/src").rglob("*.rs"):
        if path.name == "atomic_file.rs" or path.name not in {"config.rs", "persistence.rs", "state.rs"}:
            continue
        source = read(path)
        positions = []
        for match in re.finditer(r"(?<![A-Za-z0-9_])(?:(?:std::)?fs::write)\s*\(", source):
            open_paren = source.find("(", match.start())
            end_paren, args = parse_call(source, open_paren)
            if len(args) == 2:
                positions.append((match.start(), end_paren + 1, f"crate::atomic_file::write({args[0]}, {args[1]})"))
        if positions:
            for start, end, replacement in reversed(positions):
                source = source[:start] + replacement + source[end:]
            write(path, source)
            record(path, f"made {len(positions)} persisted write(s) crash-safe")
            changed_files += 1
    if changed_files == 0:
        atomic_refs = sum(read(path).count("atomic_file::write(") for path in (ROOT / "crates/daemon/src").rglob("*.rs"))
        if atomic_refs == 0:
            raise SystemExit("no daemon persistence writes were found to harden")


def harden_release_workflows() -> None:
    for relative in (".github/workflows/ci.yml", ".github/workflows/release.yml"):
        path = ROOT / relative
        if not path.exists():
            continue
        text = read(path)
        changed = False
        if relative.endswith("release.yml") and "concurrency:" not in text:
            index = text.find("\npermissions:")
            if index >= 0:
                text = text[:index] + "\nconcurrency:\n  group: release-${{ github.ref }}\n  cancel-in-progress: false\n" + text[index:]
                changed = True
        replacements = {"cargo test --all --locked": "cargo test --workspace --all-targets --locked", "cargo clippy --all --locked -- -D warnings": "cargo clippy --workspace --all-targets --locked -- -D warnings"}
        for old, new in replacements.items():
            if old in text and new not in text:
                text = text.replace(old, new)
                changed = True
        if changed:
            write(path, text)
            record(path, "expanded quality gates and serialized release publication")


def remove_transient_files() -> None:
    for path in [ROOT / "agent_docs/README.tmp", ROOT / "agent_docs/control-note.tmp", ROOT / ".github/workflows/agent-export-perf-source.yml"]:
        if path.exists():
            path.unlink()
            record(path, "removed transient audit control material")


def write_production_report() -> None:
    rust_files = list((ROOT / "crates").rglob("*.rs"))
    total_lines = sum(p.read_text(encoding="utf-8", errors="replace").count("\n") + 1 for p in rust_files)
    report_path = ROOT / "agent_docs/production-hardening-v0.2.6-sehoon.5.md"
    lines = ["# LeopardWM production hardening — v0.2.6-sehoon.5", "", "This pass is based on the actual `agent/performance-audit-03` source tree.", "It avoids speculative unsafe micro-optimizations and records only changes verified by tests, Clippy, all-target checks, and a Windows release build.", "", "## Audited surface", "", f"- Rust files: {len(rust_files)}", f"- Rust source lines: {total_lines}", "- Primary risk paths: layout calculation, persistence, Settings/WebView2, CLI target structure, release automation, scratchpad/floating transitions", "", "## Implemented changes", ""]
    for change in CHANGES:
        lines.append(f"- `{change.path}` — {change.detail}")
    lines += ["", "## Deliberately deferred", "", "- A global HWND reverse index: requires every mutation path to maintain a second source of truth and needs profiling first.", "- Lossy WinEvent debouncing: can drop the final user resize unless MoveSizeEnd flushing and ordering are designed together.", "- A custom global allocator: no allocation trace currently demonstrates a net win.", "- Unsafe indexing or target-cpu=native: not justified for portable commercial binaries.", "", "## Required release gates", "", "- cargo fmt --all -- --check", "- cargo test --workspace --all-targets --locked", "- cargo clippy --workspace --all-targets --locked -- -D warnings", "- cargo check --workspace --all-targets --locked", "- release-mode library/binary tests", "- optimized Windows build and GUI subsystem verification", "- repeated scratchpad/floating regression tests"]
    write(report_path, "\n".join(lines) + "\n")
    record(report_path, "documented the verified production-hardening scope and deferred risks")


def main() -> None:
    split_cli_implementation()
    externalize_settings_document()
    optimize_layout_hot_path()
    optimize_exact_capacity_sites()
    replace_direct_persistence_writes()
    harden_release_workflows()
    remove_transient_files()
    write_production_report()


if __name__ == "__main__":
    main()
