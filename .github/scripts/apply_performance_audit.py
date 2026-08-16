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


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def write(path: str, text: str) -> None:
    target = ROOT / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(text, encoding="utf-8", newline="\n")


def record(path: str, detail: str) -> None:
    CHANGES.append(Change(path, detail))


def replace_once(text: str, old: str, new: str, *, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one match, found {count}")
    return text.replace(old, new, 1)


def find_item_span(text: str, needle: str) -> tuple[int, int]:
    """Return a Rust item's byte span while ignoring braces in strings/comments."""
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
            # Lifetimes are followed by an identifier and are not character literals.
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


def find_rust_source(needle: str, subtree: str = "crates") -> Path:
    matches: list[Path] = []
    for path in (ROOT / subtree).rglob("*.rs"):
        source = path.read_text(encoding="utf-8")
        if needle in source:
            matches.append(path)
    if len(matches) != 1:
        names = ", ".join(str(path.relative_to(ROOT)) for path in matches)
        raise SystemExit(f"{needle}: expected one source file, found {len(matches)}: {names}")
    return matches[0]


def replace_vec_constructor(
    item: str,
    variable: str,
    constructor: str,
    *,
    required: bool = False,
) -> tuple[str, bool]:
    pattern = re.compile(
        rf"(?m)^(?P<prefix>\s*let\s+mut\s+{re.escape(variable)}"
        rf"(?:\s*:\s*[^=;]+)?\s*=\s*)Vec::new\(\);"
    )
    item, count = pattern.subn(rf"\g<prefix>{constructor};", item, count=1)
    if required and count != 1:
        raise SystemExit(f"{variable}: expected one Vec::new constructor, found {count}")
    return item, count == 1


def replace_with_inline_smallvec(item: str, variable: str) -> tuple[str, bool]:
    explicit = re.compile(
        rf"(?m)^(?P<indent>\s*)let\s+mut\s+{re.escape(variable)}\s*:\s*"
        rf"Vec<(?P<inner>[^\n;]+)>\s*=\s*"
        rf"(?:Vec::new\(\)|Vec::with_capacity\([^\n;]+\));"
    )

    def explicit_repl(match: re.Match[str]) -> str:
        inner = match.group("inner").strip()
        return (
            f"{match.group('indent')}let mut {variable}: "
            f"smallvec::SmallVec<[{inner}; 8]> = smallvec::SmallVec::new();"
        )

    item, count = explicit.subn(explicit_repl, item, count=1)
    if count:
        return item, True

    inferred = re.compile(
        rf"(?m)^(?P<prefix>\s*let\s+mut\s+{re.escape(variable)}\s*=\s*)"
        rf"(?:Vec::new\(\)|Vec::with_capacity\([^\n;]+\));"
    )
    item, count = inferred.subn(
        rf"\g<prefix>smallvec::SmallVec::<[_; 8]>::new();", item, count=1
    )
    return item, count == 1


def split_cli_into_shared_library() -> None:
    main_path = "crates/cli/src/main.rs"
    lib_path = "crates/cli/src/lib.rs"
    alias_path = "crates/cli/src/bin/lwm.rs"
    cargo_path = "crates/cli/Cargo.toml"

    if (ROOT / lib_path).exists():
        raise SystemExit(f"{lib_path} already exists; refusing to overwrite an unknown library")

    source = read(main_path)
    if "CARGO_BIN_NAME" in source:
        raise SystemExit("CLI uses CARGO_BIN_NAME; shared-library conversion needs explicit review")

    main_match = re.search(
        r"(?m)^(?P<prefix>(?:#\[[^\n]+\]\n)*)fn\s+main\s*\(\s*\)\s*"
        r"(?P<ret>->\s*[^\{\n]+)?\s*\{",
        source,
    )
    if not main_match:
        raise SystemExit("CLI main function signature was not recognized")

    signature = main_match.group(0)
    run_signature = signature.replace("fn main", "pub fn run", 1)
    library = source[: main_match.start()] + run_signature + source[main_match.end() :]
    return_type = (main_match.group("ret") or "").strip()
    wrapper_return = f" {return_type}" if return_type else ""

    inner_attrs = "".join(
        line + "\n" for line in source.splitlines() if line.startswith("#![")
    )
    wrapper = (
        f"{inner_attrs}fn main(){wrapper_return} {{\n"
        "    leopardwm_cli::run()\n"
        "}\n"
    )

    write(lib_path, library)
    write(main_path, wrapper)
    write(alias_path, wrapper)

    cargo = read(cargo_path)
    if re.search(r"(?m)^\[lib\]$", cargo):
        raise SystemExit("CLI Cargo.toml already declares a library target")
    blocks = list(re.finditer(r"(?ms)^\[\[bin\]\]\n.*?(?=^\[\[|\Z)", cargo))
    alias_block = next(
        (match for match in blocks if re.search(r'(?m)^name\s*=\s*"lwm"\s*$', match.group(0))),
        None,
    )
    if alias_block is None:
        raise SystemExit("lwm binary target block not found")
    block = alias_block.group(0)
    updated = re.sub(
        r'(?m)^path\s*=\s*"src/main\.rs"\s*$', 'path = "src/bin/lwm.rs"', block, count=1
    )
    updated = re.sub(r"(?m)^(?:test|bench)\s*=\s*false\s*\n?", "", updated)
    if updated == block or 'path = "src/bin/lwm.rs"' not in updated:
        raise SystemExit("failed to retarget the lwm binary")
    cargo = cargo[: alias_block.start()] + updated + cargo[alias_block.end() :]
    write(cargo_path, cargo)

    record(main_path, "replaced the duplicate full binary crate with a thin wrapper")
    record(lib_path, "moved CLI implementation and unit tests into one shared library target")
    record(alias_path, "added a thin alias wrapper; the implementation is compiled once")
    record(cargo_path, "retargeted lwm to its own wrapper and removed duplicate-target warning")


def extract_settings_html() -> None:
    rust_path = "crates/daemon/src/settings/html.rs"
    html_path = "crates/daemon/src/settings/settings.html"
    if (ROOT / html_path).exists():
        raise SystemExit(f"{html_path} already exists; refusing to overwrite it")

    source = read(rust_path)
    pattern = re.compile(
        r'(?P<decl>pub(?:\([^\)]+\))?\s+const\s+SETTINGS_HTML\s*:\s*&str\s*=\s*)'
        r'r(?P<hash>#{0,16})"(?P<body>.*?)"(?P=hash);',
        re.DOTALL,
    )
    match = pattern.search(source)
    if not match:
        raise SystemExit("embedded SETTINGS_HTML raw string was not found")
    body = match.group("body")
    if "<html" not in body.lower() or "{VERSION}" not in body:
        raise SystemExit("embedded Settings document did not pass sanity checks")

    replacement = f'{match.group("decl")}include_str!("settings.html");'
    source = source[: match.start()] + replacement + source[match.end() :]
    write(rust_path, source)
    write(html_path, body)

    record(rust_path, "replaced a 100+ KB Rust raw literal with include_str!")
    record(html_path, "moved the Settings document into a native HTML asset")


def add_smallvec_dependency() -> None:
    cargo_path = "crates/core_layout/Cargo.toml"
    cargo = read(cargo_path)
    if re.search(r"(?m)^smallvec\s*=", cargo):
        return
    dep = re.search(r"(?m)^\[dependencies\]\s*$", cargo)
    if not dep:
        raise SystemExit("core_layout [dependencies] section not found")
    insert = dep.end()
    cargo = cargo[:insert] + '\nsmallvec = "1.15"' + cargo[insert:]
    write(cargo_path, cargo)
    record(cargo_path, "added SmallVec for allocation-free common column layouts")


def optimize_core_layout_allocations() -> None:
    path = find_rust_source("fn compute_non_fullscreen_placements", "crates/core_layout")
    source = path.read_text(encoding="utf-8")
    start, end = find_item_span(source, "fn compute_non_fullscreen_placements")
    item = source[start:end]

    item, reserved = replace_vec_constructor(
        item, "placements", "Vec::with_capacity(self.window_count())", required=True
    )
    assert reserved

    inline_changed: list[str] = []
    for variable in ("visible_windows", "visible_weights", "min_heights"):
        item, changed = replace_with_inline_smallvec(item, variable)
        if changed:
            inline_changed.append(variable)

    if not inline_changed:
        raise SystemExit("no per-column scratch vectors were found for SmallVec conversion")

    source = source[:start] + item + source[end:]
    path.write_text(source, encoding="utf-8", newline="\n")
    add_smallvec_dependency()
    rel = str(path.relative_to(ROOT)).replace("\\", "/")
    record(rel, "reserved final placement capacity from window_count")
    record(
        rel,
        "kept common per-column scratch arrays inline (up to eight visible windows): "
        + ", ".join(inline_changed),
    )


def optimize_named_vec(path: Path, function: str, variable: str, capacity: str) -> bool:
    source = path.read_text(encoding="utf-8")
    try:
        start, end = find_item_span(source, f"fn {function}")
    except ValueError:
        return False
    item = source[start:end]
    item, changed = replace_vec_constructor(item, variable, f"Vec::with_capacity({capacity})")
    if not changed:
        return False
    source = source[:start] + item + source[end:]
    path.write_text(source, encoding="utf-8", newline="\n")
    record(
        str(path.relative_to(ROOT)).replace("\\", "/"),
        f"preallocated {variable} in {function} from {capacity}",
    )
    return True


def optimize_daemon_scratch_vectors() -> None:
    changes = 0
    for path in (ROOT / "crates/daemon/src").rglob("*.rs"):
        if optimize_named_vec(path, "column_bounds_from_placements", "bounds", "placements.len()"):
            changes += 1
        if optimize_named_vec(
            path, "column_bounds_from_placements", "column_bounds", "placements.len()"
        ):
            changes += 1
        for variable in ("live", "live_placements"):
            if optimize_named_vec(path, "partition_for_animation", variable, "placements.len()"):
                changes += 1
    if changes == 0:
        # These helpers may already be preallocated in a future base. The core-layout
        # changes remain mandatory, so this is informational rather than fatal.
        record("crates/daemon/src", "no additional named daemon scratch Vecs required changes")


def tighten_release_profile() -> None:
    path = "Cargo.toml"
    cargo = read(path)
    profile_match = re.search(r"(?ms)^\[profile\.release\]\n(?P<body>.*?)(?=^\[|\Z)", cargo)
    if not profile_match:
        raise SystemExit("root [profile.release] section not found")
    block = profile_match.group(0)
    updated = block
    if not re.search(r"(?m)^incremental\s*=", updated):
        updated = updated.rstrip() + "\nincremental = false\n"
    if updated != block:
        cargo = cargo[: profile_match.start()] + updated + cargo[profile_match.end() :]
        write(path, cargo)
        record(path, "made non-incremental optimized release builds explicit and reproducible")


def write_report() -> None:
    source_files = [
        path
        for path in ROOT.rglob("*")
        if path.is_file()
        and path.suffix.lower() in {".rs", ".toml", ".html", ".yml", ".yaml", ".ps1"}
        and ".git" not in path.parts
        and "target" not in path.parts
    ]
    line_count = 0
    byte_count = 0
    for path in source_files:
        data = path.read_bytes()
        byte_count += len(data)
        line_count += data.count(b"\n") + (0 if not data or data.endswith(b"\n") else 1)

    details = "\n".join(f"- `{change.path}` — {change.detail}" for change in CHANGES)
    report = f"""# Sehoon fork performance audit — pass 3

This pass starts from `v0.2.6-sehoon.3` and is scoped exclusively to
`sehoon123/LeopardWM`. It favors low-risk structural and allocation improvements;
it deliberately does not add timing-based WinEvent suppression without ETW evidence.

## Audited inventory

- Source/build assets scanned: **{len(source_files)}**
- Source/build lines scanned: **{line_count}**
- Source/build bytes scanned: **{byte_count}**

## Changes

{details}

## Runtime impact model

- The layout result vector now performs one capacity reservation sized from the
  workspace window count instead of growing geometrically.
- For columns containing up to eight visible windows, the temporary window,
  weight, and minimum-height arrays remain inline and perform **zero heap
  allocations**. Larger stacks spill safely to the heap.
- Daemon placement/drag scratch outputs reserve from their input cardinality where
  the implementation exposes an exact upper bound.
- No event is dropped, reordered, or delayed by this pass.

## Build and maintainability impact

- `leopardwm-cli` and `lwm` are thin binaries over one library implementation, so
  Cargo no longer compiles and analyzes the complete CLI source twice.
- The Settings page is a real HTML asset rather than a 100+ KB Rust raw literal,
  reducing Rust parser/formatter work and making browser-level review practical.

## Verification gate

The release job must pass formatting, the complete locked test suite, all-target
Clippy with warnings denied, all-target/all-feature checking, optimized
core-layout tests, optimized Windows builds, and PE GUI-subsystem validation
before it may commit or publish binaries.

## Deliberately deferred

`EVENT_OBJECT_LOCATIONCHANGE` coalescing and a persistent cross-frame layout
scratch arena remain measurement-driven follow-ups. Both can improve throughput,
but either can also introduce input latency or borrow/lifetime complexity when
implemented without ETW traces and hardware-backed stress tests.
"""
    write("agent_docs/sehoon-fork-performance-audit.md", report)
    record("agent_docs/sehoon-fork-performance-audit.md", "recorded scope and performance invariants")


def main() -> None:
    split_cli_into_shared_library()
    extract_settings_html()
    optimize_core_layout_allocations()
    optimize_daemon_scratch_vectors()
    tighten_release_profile()
    write_report()

    if len(CHANGES) < 8:
        raise SystemExit(f"performance pass changed too little ({len(CHANGES)} recorded changes)")


if __name__ == "__main__":
    main()
