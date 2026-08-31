# Release Process

Guide for all agents and contributors releasing LeopardWM.

## Version and tag format

The workspace version in `Cargo.toml` is a three-part SemVer core version (`X.Y.Z`), which is also the MSI `ProductVersion`.

Release tags must be `vX.Y.Z` or that same core version followed by a SemVer prerelease suffix. For example, with Cargo version `0.2.6`, `v0.2.6-sehoon.24-rc2` is valid. Build metadata and a different core version are rejected. **Only one tag may use each core version:** bump `[workspace.package].version` for every published artifact, even when changing only the suffix. The changelog header is the tag without its leading `v`, including any prerelease suffix.

## Release workflow

`.github/workflows/release.yml` starts for `v*` pushes, but it publishes only after `.github/validate-release.ps1` confirms all of the following:

1. The tag resolves to the checked-out commit.
2. That commit exactly equals the fetched `origin/main` candidate.
3. The tag core version equals `[workspace.package].version`, its optional suffix is valid SemVer prerelease syntax, and no other tag uses that core.
4. `CHANGELOG.md` contains the exact matching section.

The tag workflow first waits for its `physical-gate` job on the protected `release-hardware` environment. That job must run interactively on a Windows self-hosted runner labeled `leopardwm-release-hardware`; it verifies exact tag/main provenance and runs `preview_lifecycle_windows` with dual-monitor, physical-click, and strict noninjected-input requirements enabled. It uploads an exact-SHA/run-attempt attestation. The hosted `build` job downloads and validates that same-run attestation before it can format, test, package, or publish. If a later job fails, use **Re-run all jobs**: re-running failed jobs alone deliberately lacks a new-attempt physical attestation and fails closed.

The hosted job then runs the locked workspace suite and both controlled Win32 probes (`floating_return_windows` and `preview_lifecycle_windows`) serially. Each probe has a fail-closed daemon-absence preflight; it never stops a process automatically. It builds locked release binaries, verifies the daemon and watchdog PE GUI subsystems, and packages:

- `LeopardWM-{tag-without-v}-x86_64-windows.zip` containing the daemon, both CLI names, watchdog, README, LICENSE, and CHANGELOG;
- `LeopardWM-{tag-without-v}-x86_64.msi` from `wix/main.wxs`; and
- `checksums.txt` with SHA-256 values for exactly those ZIP and MSI files.

Before the GitHub Release is created, the validator checks the ZIP inventory and executable hashes against the release build, MSI `ProductName`/`ProductVersion` and installed file table, artifact filenames, and checksums. The MSI uses Cargo's core version even when the external release tag has a prerelease suffix.

The workflow has **no Winget publishing job**. Submit Winget updates manually as described in `agent_docs/distribution_setup.md`.

Binary path: `target/x86_64-pc-windows-msvc/release/` (configured in `.cargo/config.toml`).

## Changelog format

Use Conventional Commits-style sections in `CHANGELOG.md`:

```markdown
## 0.2.0

### Features
- Add workspace switching via Ctrl+Alt+1-9

### Improvements
- Improve border rendering performance on multi-monitor setups

### Fixes
- Fix transient window suppression for Beeper desktop app
```

The header must be exactly `## X.Y.Z` (or `## X.Y.Z-prerelease` for a prerelease tag); bracketed headers are also accepted by the validator and release-note extractor.

## Pre-release checklist

1. Update `CHANGELOG.md` with the exact planned tag section and all notable user-facing changes.
2. Bump `[workspace.package].version` in `Cargo.toml` and update `Cargo.lock`; every artifact requires a new core version.
3. Run the local gate and inspect every result:
   - `cargo build --release`
   - `pwsh ./.github/verify-gui-subsystems.ps1`
   - `cargo test --all`
   - `cargo test -p leopardwm-platform-win32 --features integration-probes --test floating_return_windows -- --test-threads=1`
   - `cargo test -p leopardwm-platform-win32 --features integration-probes --test preview_lifecycle_windows -- --test-threads=1`
   - `cargo clippy --all -- -D warnings`
   - `cargo fmt --all -- --check`
4. On the release hardware, connect two physically adjacent displays, stop LeopardWM, and run the interactive pixel/click gate from PowerShell. Physically click the printed preview coordinate; the test must not inject this click:
   ```powershell
   .\.github\verify-no-leopardwm-daemon.ps1
   $env:LEOPARDWM_REQUIRE_DUAL_MONITOR = '1'
   $env:LEOPARDWM_REQUIRE_PHYSICAL_CLICK = '1'
   $env:LEOPARDWM_REQUIRE_NONINJECTED_CLICK = '1'
   $env:LEOPARDWM_PHYSICAL_CLICK_TIMEOUT_SECS = '300'   # optional, default 60, max 1800
   cargo test -p leopardwm-platform-win32 --features integration-probes --test preview_lifecycle_windows --locked -- --nocapture --test-threads=1
   Remove-Item Env:LEOPARDWM_REQUIRE_DUAL_MONITOR, Env:LEOPARDWM_REQUIRE_PHYSICAL_CLICK, Env:LEOPARDWM_REQUIRE_NONINJECTED_CLICK, Env:LEOPARDWM_PHYSICAL_CLICK_TIMEOUT_SECS -ErrorAction SilentlyContinue
   ```
5. Commit the release preparation and independently review the exact diff from the prior release.
6. Push the candidate to `main` without force and wait for the required `check` workflow.
7. Confirm `origin/main` still equals the reviewed candidate and that the tag does not already exist.
8. Launch the self-hosted runner **interactively in the release user's desktop session** with the `leopardwm-release-hardware` label, then create and push the tag. Do not run this runner as a service: a service cannot supply or observe the physical desktop click.
9. Approve the protected `release-hardware` environment, physically click the coordinate printed by the strict noninjected probe, and verify that the hosted build accepts the exact-SHA attestation before publishing.
10. Verify the GitHub Release, ZIP/MSI contents, checksums, and release notes. Update the repository Scoop manifest and submit any Winget update manually from the published MSI.

Any candidate change after verification or review requires the complete gate and review again before publication.

## Branch protection (`main`)

- Required status check: `check` (strict; the branch must be up to date).
- Required approving reviews: 1.
- Linear history is required.
- Admin enforcement is disabled, so repository administrators can bypass these requirements without changing protection settings.
- Force pushes are not allowed.
