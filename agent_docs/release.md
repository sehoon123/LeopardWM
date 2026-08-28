# Release Process

Guide for all agents and contributors releasing LeopardWM.

## Version and tag format

The workspace version in `Cargo.toml` is a three-part SemVer core version (`X.Y.Z`), which is also the MSI `ProductVersion`.

Release tags must be `vX.Y.Z` or that same core version followed by a SemVer prerelease suffix. For example, with Cargo version `0.2.6`, both `v0.2.6` and the established `v0.2.6-sehoon.24-rc2` form are valid. Build metadata and a different core version are rejected. The changelog header is the tag without its leading `v`, including any prerelease suffix.

## Release workflow

`.github/workflows/release.yml` starts for `v*` pushes, but it publishes only after `.github/validate-release.ps1` confirms all of the following:

1. The tag resolves to the checked-out commit.
2. That commit exactly equals the fetched `origin/main` candidate.
3. The tag core version equals `[workspace.package].version` and its optional suffix is valid SemVer prerelease syntax.
4. `CHANGELOG.md` contains the exact matching section.

The workflow then formats, runs the locked workspace suite, and runs both controlled Win32 probes (`floating_return_windows` and `preview_lifecycle_windows`) serially. Each probe has a fail-closed daemon-absence preflight; it never stops a process automatically. It builds locked release binaries, verifies the daemon and watchdog PE GUI subsystems, and packages:

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
2. Bump `[workspace.package].version` in `Cargo.toml` and update `Cargo.lock` only when preparing a new version.
3. Run the local gate and inspect every result:
   - `cargo build --release`
   - `pwsh ./.github/verify-gui-subsystems.ps1`
   - `cargo test --all`
   - `cargo test -p leopardwm-platform-win32 --features integration-probes --test floating_return_windows -- --test-threads=1`
   - `cargo test -p leopardwm-platform-win32 --features integration-probes --test preview_lifecycle_windows -- --test-threads=1`
   - `cargo clippy --all -- -D warnings`
   - `cargo fmt --all -- --check`
4. Commit the release preparation and independently review the exact diff from the prior release.
5. Push the candidate to `main` without force and wait for the required `check` workflow.
6. Confirm `origin/main` still equals the reviewed candidate and that the tag does not already exist.
7. Create and push the tag only after those checks. Do not retag a changed candidate.
8. Monitor the release workflow and verify the GitHub Release, ZIP/MSI contents, checksums, and release notes.
9. Update the repository Scoop manifest and submit any Winget update manually from the published MSI.

Any candidate change after verification or review requires the complete gate and review again before publication.

## Branch protection (`main`)

- Required status check: `check` (strict; the branch must be up to date).
- Required approving reviews: 1.
- Linear history is required.
- Admin enforcement is disabled, so repository administrators can bypass these requirements without changing protection settings.
- Force pushes are not allowed.
