# LeopardWM production hardening — v0.2.6-sehoon.5

This pass is based on the actual `agent/performance-audit-03` source tree.
It avoids speculative unsafe micro-optimizations and records only changes verified by tests, Clippy, all-target checks, and a Windows release build.

## Audited surface

- Rust files: 85
- Rust source lines: 64973
- Primary risk paths: layout calculation, persistence, Settings/WebView2, CLI target structure, release automation, scratchpad/floating transitions

## Implemented changes

- `crates/cli/src/main.rs` — replaced duplicated CLI implementation with a thin entry point
- `crates/cli/src/lib.rs` — moved the CLI implementation and tests into a single library target
- `crates/cli/src/bin/lwm.rs` — added a separate thin lwm alias entry point
- `crates/cli/Cargo.toml` — made both binaries link one shared CLI implementation
- `crates/daemon/src/settings/html.rs` — replaced the large Rust raw literal with include_str!
- `crates/daemon/src/settings/settings.html` — moved Settings HTML/CSS/JS into a native static asset
- `crates/core_layout/src/workspace/layout.rs` — reserved placement output capacity from the known window count
- `crates/core_layout/Cargo.toml` — added SmallVec for allocation-free common layout scratch storage
- `crates/core_layout/src/workspace/layout.rs` — kept common per-column scratch vectors inline: visible_windows, visible_weights, min_heights
- `crates/daemon/src/transitions.rs` — reserved ghosts using an already-known upper bound
- `crates/daemon/src/atomic_file.rs` — added flushed same-volume atomic replacement with transient-lock retry
- `crates/daemon/src/main.rs` — wired the atomic persistence module
- `Cargo.toml` — enabled Win32 atomic file-replacement APIs
- `crates/daemon/src/config.rs` — made 3 persisted write(s) crash-safe
- `crates/daemon/src/persistence.rs` — made 1 persisted write(s) crash-safe

## Deliberately deferred

- A global HWND reverse index: requires every mutation path to maintain a second source of truth and needs profiling first.
- Lossy WinEvent debouncing: can drop the final user resize unless MoveSizeEnd flushing and ordering are designed together.
- A custom global allocator: no allocation trace currently demonstrates a net win.
- Unsafe indexing or target-cpu=native: not justified for portable commercial binaries.

## Required release gates

- cargo fmt --all -- --check
- cargo test --workspace --all-targets --locked
- cargo clippy --workspace --all-targets --locked -- -D warnings
- cargo check --workspace --all-targets --locked
- release-mode library/binary tests
- optimized Windows build and GUI subsystem verification
- repeated scratchpad/floating regression tests
