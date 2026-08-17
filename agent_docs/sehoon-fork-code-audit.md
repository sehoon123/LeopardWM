# Sehoon fork code-audit log

This document records the repository-wide audit pass that produced `v0.2.6-sehoon.3`.
It is intentionally scoped to the personal fork and is not an upstream contribution.

## Inventory scanned

- Source/build files scanned: **100**
- Lines scanned: **67684**
- `unsafe` tokens reviewed by inventory: **322**
- direct `.unwrap()` calls inventoried: **1269**
- `.expect(...)` calls inventoried: **148**
- TODO/FIXME/unimplemented markers inventoried: **0**

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
