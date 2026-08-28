# Distribution Setup

The release workflow publishes GitHub Release ZIP/MSI assets only. It does not publish Winget or Scoop updates and does not currently sign binaries.

## Winget — manual submission

There is no `publish-winget` job and no `WINGET_TOKEN` secret in this repository. After a GitHub Release succeeds:

1. Download the published MSI and read its SHA-256 from `checksums.txt`.
2. Update or submit the `sehoon123.LeopardWM` manifest in `microsoft/winget-pkgs` following that repository's current contribution requirements.
3. Verify the accepted upstream manifest with `winget show sehoon123.LeopardWM` and, when available, `winget upgrade sehoon123.LeopardWM` on a clean machine.

Do not claim that a successful GitHub Release updated Winget; upstream review and merge are separate manual work.

## Scoop — repository manifest

`dist/scoop/leopardwm.json` is the checked-in Scoop manifest. As part of release preparation, update all of these together from the published GitHub Release ZIP:

- `version` to `[workspace.package].version`;
- the URL and `extract_dir` to the matching stable release asset; and
- the ZIP SHA-256 to the value in that release's `checksums.txt`.

The manifest ships daemon and both CLI shims. The watchdog remains adjacent to the CLI binaries and is intentionally not shimmed for direct invocation.

## Code signing — future manual setup

No signing step exists in `.github/workflows/release.yml`, so current releases remain unsigned. If signing is introduced, document the selected provider, required identities/secrets, OIDC permissions, which ZIP/MSI binaries are signed, and a clean-machine verification before representing releases as signed.

## Status checklist

- [ ] GitHub Release ZIP/MSI inventories and checksums verified
- [ ] Scoop manifest updated from the matching published ZIP
- [ ] Winget manifest submitted manually (if a Winget update is desired)
- [ ] Winget upstream review/merge verified independently
- [ ] Code-signing plan approved and implemented before claiming signed releases
