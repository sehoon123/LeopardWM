# LeopardWM fork performance audit — v0.2.6-sehoon.4

Baseline: v0.2.6-sehoon.3

## Changes

- Consolidated the two CLI executables onto one library implementation with thin binary wrappers.
- Moved the embedded Settings page to an include_str! asset, preserving zero runtime I/O while simplifying compilation.
- Added exact capacity reservations to placement output and known-cardinality collection paths.
- Used inline-backed SmallVec scratch storage for common per-column layout cases.
- Retained the floating, scratchpad, Settings-resize, DPI, and lifecycle fixes from prior fork releases.

## Release gate

- Debug all-target tests
- Release-mode library and binary tests
- Clippy for all targets with warnings denied
- Workspace all-target check
- Optimized Windows build and GUI subsystem verification
- Repeated release-mode layout, scratchpad, and floating stress tests

The allocation reductions are source-level structural improvements. No synthetic wall-clock percentage is claimed without controlled ETW and allocator traces on representative hardware.
