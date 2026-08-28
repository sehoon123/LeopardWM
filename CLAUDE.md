# CLAUDE.md

## What
LeopardWM — a scroll-first tiling window manager for Windows 10/11.
Rust workspace, MSVC toolchain (`stable-x86_64-pc-windows-msvc`).

## Crates
| Crate | What it does |
|---|---|
| `core_layout` | Platform-agnostic scrolling layout engine |
| `platform_win32` | Win32 APIs and DWM integration |
| `ipc` | Named-pipe command/response protocol |
| `daemon` | Event loop, state, message-pump threads, tray, and settings WebView |
| `cli` | User-facing CLI (`leopardwm-cli` and `lwm`) |
| `watchdog` | Daemon supervisor and crash-recovery launcher |

All crates live under `crates/`. Internal names still use `leopardwm` (rename is future work).

## Commands
```
cargo build --release
cargo test --all
```

## Workflow
- Plan before editing if the change touches 3+ files or involves architectural decisions.
- Prefer reuse — search for existing functions/patterns before adding new code.
- Verify with tests or logs before declaring done.
- Keep changes minimal and scoped; avoid unrelated refactors.

## Large files
Use offset/limit when reading large sources, especially `crates/daemon/src/main.rs`,
`crates/daemon/src/config.rs`, `crates/daemon/src/helpers.rs`, and their test modules.

## When summarizing this conversation
Preserve: (1) which files were modified and why, (2) current git branch and uncommitted state, (3) in-progress work or unfinished steps, (4) user corrections/preferences from this session, (5) specific error messages being debugged.

## Reference docs
- Read `AGENTS.md` for repository-wide policies.
- Read `agent_docs/release.md` before changing release metadata, tags, or `CHANGELOG.md`.
- Read `agent_docs/ipc-events.md` before changing the public event stream.
