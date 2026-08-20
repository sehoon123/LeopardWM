$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$sourcePath = 'crates/platform_win32/src/window_region.rs'
$releaseTag = 'v0.2.6-sehoon.14-rc1'
$releaseVersion = '0.2.6-sehoon.14-rc1'
$sourceBranch = 'fix/final-region-redraw-v14'

$text = [System.IO.File]::ReadAllText($sourcePath).Replace("`r`n", "`n")
$testName = 'fn exact_landing_recommits_an_unchanged_region_for_redraw()'
$patchedMarker = 'the exact landing must still re-commit the HRGN with bRedraw=TRUE'
$hasCodePatch = $text.Contains($patchedMarker)
$hasRegression = $text.Contains($testName)

if ($hasCodePatch -and $hasRegression) {
    Write-Host 'Exact-landing redraw patch is already present; this is the post-push verification run.'
    exit 0
}
if ($hasCodePatch -xor $hasRegression) {
    throw 'Partial exact-landing patch detected; refusing to continue.'
}

$old = @'
    if current_owned == Some(target_region) && actual_region_matches(hwnd, target_region) {
        return RegionClipResult::Unchanged;
    }

    // Replace the bridge directly. Clearing first creates an unbounded
'@
$old = $old.Replace("`r`n", "`n")
$new = @'
    if current_owned == Some(target_region) && actual_region_matches(hwnd, target_region) {
        if !redraw {
            return RegionClipResult::Unchanged;
        }
        // prepare_window_region_clip installs its bridge without repainting.
        // When that bridge already equals the final shape, the exact landing
        // must still re-commit the HRGN with bRedraw=TRUE. Otherwise modern
        // compositor-backed windows can retain a stale gray backing surface.
    }

    // Replace the bridge directly. Clearing first creates an unbounded
'@
$new = $new.Replace("`r`n", "`n")
if (-not $text.Contains($old)) {
    throw 'Exact apply_window_region_clip block was not found.'
}
$text = $text.Replace($old, $new)

$marker = @'
    #[test]
    fn centered_preview_regions_are_symmetric() {
'@
$marker = $marker.Replace("`r`n", "`n")
$regression = @'
    #[test]
    fn exact_landing_recommits_an_unchanged_region_for_redraw() {
        let window = TestWindow::new();
        let id = window_id(window.0);
        let outer = Rect::new(0, 0, 1000, 800);
        let clip = Rect::new(0, 0, 250, 800);

        assert_eq!(
            apply_window_region_clip(id, outer, outer, clip, false),
            super::RegionClipResult::Applied
        );
        assert_eq!(
            apply_window_region_clip(id, outer, outer, clip, true),
            super::RegionClipResult::Applied
        );
        assert!(actual_region_matches(window.0, Rect::new(0, 0, 250, 800)));
    }

'@
$regression = $regression.Replace("`r`n", "`n")
if (-not $text.Contains($marker)) {
    throw 'Regression-test insertion marker was not found.'
}
$text = $text.Replace($marker, $regression + $marker)
[System.IO.File]::WriteAllText(
    $sourcePath,
    $text,
    [System.Text.UTF8Encoding]::new($false)
)
Write-Host 'Exact-landing redraw patch applied.'

cargo fmt --all
if ($LASTEXITCODE -ne 0) { throw 'cargo fmt failed' }
cargo fmt --all -- --check
if ($LASTEXITCODE -ne 0) { throw 'cargo fmt check failed' }
git diff --check
if ($LASTEXITCODE -ne 0) { throw 'git diff check failed' }
$changed = @(git status --short | ForEach-Object { $_.Substring(3) })
if ($changed.Count -ne 1 -or $changed[0] -ne $sourcePath) {
    throw "Unexpected changed files: $($changed -join ', ')"
}

cargo test -p leopardwm-platform-win32 exact_landing_recommits_an_unchanged_region_for_redraw --locked -- --test-threads=1
if ($LASTEXITCODE -ne 0) { throw 'exact-landing redraw regression failed' }
cargo test -p leopardwm-platform-win32 window_region --locked -- --test-threads=1
if ($LASTEXITCODE -ne 0) { throw 'focused window-region tests failed' }
cargo test --workspace --all-targets --locked
if ($LASTEXITCODE -ne 0) { throw 'full test suite failed' }
cargo clippy --workspace --all-targets --locked -- -D warnings
if ($LASTEXITCODE -ne 0) { throw 'Clippy failed' }
cargo check --workspace --all-targets --locked
if ($LASTEXITCODE -ne 0) { throw 'cargo check failed' }
cargo build --workspace --release --locked
if ($LASTEXITCODE -ne 0) { throw 'release build failed' }
./.github/verify-gui-subsystems.ps1 -RepoRoot (Get-Location).Path

cargo install cargo-wix --version '^0.3' --locked
if ($LASTEXITCODE -ne 0) { throw 'cargo-wix installation failed' }
cargo wix -p leopardwm-daemon -I wix/main.wxs --target x86_64-pc-windows-msvc --no-build --nocapture --bin-path 'C:\Program Files (x86)\WiX Toolset v3.14\bin'
if ($LASTEXITCODE -ne 0) { throw 'cargo wix failed' }
$builtMsi = Get-ChildItem target/wix/*.msi | Sort-Object LastWriteTime -Descending | Select-Object -First 1
if (-not $builtMsi) { throw 'cargo wix produced no MSI' }
$msiName = "LeopardWM-$releaseVersion-x86_64.msi"
Copy-Item $builtMsi.FullName $msiName

$binDir = @('target/x86_64-pc-windows-msvc/release', 'target/release') |
    Where-Object { Test-Path (Join-Path $_ 'leopardwm.exe') } |
    Select-Object -First 1
if (-not $binDir) { throw 'Release binary directory not found' }

$localAdmin = Join-Path (Get-Location) 'msi-admin-image'
New-Item -ItemType Directory -Path $localAdmin -Force | Out-Null
$resolvedMsi = (Resolve-Path $msiName).Path
$adminArguments = "/a `"$resolvedMsi`" /qn TARGETDIR=`"$localAdmin`""
$adminProcess = Start-Process "$env:SystemRoot\System32\msiexec.exe" -ArgumentList $adminArguments -Wait -PassThru
if ($adminProcess.ExitCode -ne 0) {
    throw "MSI administrative install failed: $($adminProcess.ExitCode)"
}

$binaries = @('leopardwm.exe', 'leopardwm-cli.exe', 'lwm.exe', 'leopardwm-watchdog.exe')
foreach ($binary in $binaries) {
    $installed = Get-ChildItem $localAdmin -Recurse -Filter $binary | Select-Object -First 1
    if (-not $installed) { throw "MSI is missing $binary" }
    $builtHash = (Get-FileHash (Join-Path $binDir $binary) -Algorithm SHA256).Hash
    $installedHash = (Get-FileHash $installed.FullName -Algorithm SHA256).Hash
    if ($builtHash -ne $installedHash) { throw "MSI binary mismatch: $binary" }
}

$packageDir = "LeopardWM-$releaseVersion-x86_64-windows"
New-Item -ItemType Directory -Path $packageDir -Force | Out-Null
foreach ($binary in $binaries) {
    $source = Join-Path $binDir $binary
    Copy-Item $source (Join-Path $packageDir $binary)
    Copy-Item $source $binary
}
Copy-Item README.md, LICENSE, CHANGELOG.md $packageDir
$zipName = "$packageDir.zip"
7z a $zipName $packageDir | Out-Host
if ($LASTEXITCODE -ne 0) { throw 'ZIP packaging failed' }

$assets = @($zipName, $msiName) + $binaries
$checksumLines = foreach ($asset in $assets) {
    "{0} *{1}" -f (Get-FileHash $asset -Algorithm SHA256).Hash.ToLowerInvariant(), $asset
}
$checksumLines | Set-Content checksums.txt -Encoding ascii
@'
Diagnostic prerelease for real-hardware validation. It is intentionally not merged to main.

Exact-landing redraw change:
- intermediate region commits still use bRedraw=FALSE
- when the bridge already equals the final HRGN, the synchronous landing now reapplies that same HRGN with bRedraw=TRUE
- this closes the previous early-return path that could leave a stale gray compositor backing surface
- includes a regression test proving the identical region is recommitted on the redraw landing

Validation focus: Windows File Explorer, Windows Notepad, WinUI/DirectComposition windows, and classic Win32 windows at both left and right monitor edges.
'@ | Set-Content release-notes-v14-rc1.md -Encoding utf8

git config user.name 'github-actions[bot]'
git config user.email '41898282+github-actions[bot]@users.noreply.github.com'
git add $sourcePath
git diff --cached --check
if ($LASTEXITCODE -ne 0) { throw 'staged diff check failed' }
git commit -m 'fix: redraw unchanged clip region on exact landing'
if ($LASTEXITCODE -ne 0) { throw 'source commit failed' }
$sourceSha = (git rev-parse HEAD).Trim()
git push origin "HEAD:refs/heads/$sourceBranch"
if ($LASTEXITCODE -ne 0) { throw 'RC branch push failed' }
Write-Host "Verified RC source commit: $sourceSha"

if (gh release view $releaseTag --repo $env:GITHUB_REPOSITORY *> $null) {
    throw "Release already exists: $releaseTag"
}
$tagLookup = git ls-remote --tags origin "refs/tags/$releaseTag"
if ($tagLookup) { throw "Tag already exists: $releaseTag" }

gh release create $releaseTag `
    $zipName `
    $msiName `
    @binaries `
    checksums.txt `
    --repo $env:GITHUB_REPOSITORY `
    --target $sourceSha `
    --title "LeopardWM $releaseTag - exact landing redraw RC" `
    --notes-file release-notes-v14-rc1.md `
    --prerelease
if ($LASTEXITCODE -ne 0) { throw 'GitHub prerelease publication failed' }

$verify = Join-Path (Get-Location) 'published-verification'
New-Item -ItemType Directory -Path $verify -Force | Out-Null
gh release download $releaseTag --repo $env:GITHUB_REPOSITORY --dir $verify
if ($LASTEXITCODE -ne 0) { throw 'Published release download failed' }
foreach ($line in Get-Content (Join-Path $verify 'checksums.txt')) {
    if ($line -notmatch '^([0-9a-f]{64}) \*(.+)$') { throw "Invalid checksum line: $line" }
    $assetPath = Join-Path $verify $Matches[2]
    if (-not (Test-Path $assetPath)) { throw "Missing published asset: $($Matches[2])" }
    $actualHash = (Get-FileHash $assetPath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualHash -ne $Matches[1]) { throw "Published checksum mismatch: $($Matches[2])" }
}

$publishedZip = Join-Path $verify $zipName
7z t $publishedZip | Out-Host
if ($LASTEXITCODE -ne 0) { throw 'Published ZIP CRC check failed' }
$extract = Join-Path $verify 'zip-extracted'
7z x $publishedZip "-o$extract" -y | Out-Host
if ($LASTEXITCODE -ne 0) { throw 'Published ZIP extraction failed' }

$publishedMsi = Join-Path $verify $msiName
$publishedAdmin = Join-Path $verify 'msi-admin-image'
New-Item -ItemType Directory -Path $publishedAdmin -Force | Out-Null
$publishedArguments = "/a `"$publishedMsi`" /qn TARGETDIR=`"$publishedAdmin`""
$publishedProcess = Start-Process "$env:SystemRoot\System32\msiexec.exe" -ArgumentList $publishedArguments -Wait -PassThru
if ($publishedProcess.ExitCode -ne 0) {
    throw "Published MSI administrative install failed: $($publishedProcess.ExitCode)"
}
foreach ($binary in $binaries) {
    $standalone = Join-Path $verify $binary
    $fromZip = Get-ChildItem $extract -Recurse -Filter $binary | Select-Object -First 1
    $fromMsi = Get-ChildItem $publishedAdmin -Recurse -Filter $binary | Select-Object -First 1
    if (-not $fromZip -or -not $fromMsi) { throw "Packaged binary missing: $binary" }
    $expectedHash = (Get-FileHash $standalone -Algorithm SHA256).Hash
    if ((Get-FileHash $fromZip.FullName -Algorithm SHA256).Hash -ne $expectedHash) {
        throw "ZIP binary mismatch: $binary"
    }
    if ((Get-FileHash $fromMsi.FullName -Algorithm SHA256).Hash -ne $expectedHash) {
        throw "MSI binary mismatch: $binary"
    }
}

git fetch origin --tags --force
if ($LASTEXITCODE -ne 0) { throw 'tag fetch failed' }
$tagSha = (git rev-list -n 1 $releaseTag).Trim()
$branchSha = ((git ls-remote origin "refs/heads/$sourceBranch") -split '\s+')[0]
if ($tagSha -ne $sourceSha -or $branchSha -ne $sourceSha) {
    throw "Source identity mismatch: tag=$tagSha branch=$branchSha expected=$sourceSha"
}
Write-Host "Published and independently verified $releaseTag at $sourceSha"
