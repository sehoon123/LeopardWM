#Requires -Version 5.1
# Fixture tests for .github/validate-release.ps1. These use a local bare Git
# remote so provenance checks exercise the same origin/main path as release CI.

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$validator = Join-Path (Split-Path -Parent $PSScriptRoot) 'validate-release.ps1'
if (-not (Test-Path -LiteralPath $validator)) {
    throw "Release validator not found: $validator"
}
$validatorSource = Get-Content -LiteralPath $validator -Raw
if ($validatorSource -notmatch '\$actualHash\s+-ine\s+\$records\[\$name\]') {
    throw 'Release checksum comparison must accept lowercase sha256sum output'
}

function Invoke-TestGit {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Repository,
        [Parameter(Mandatory = $true)]
        [string[]]$Arguments
    )

    # Windows PowerShell 5 wraps native stderr as ErrorRecord objects. Git
    # writes benign progress (for example checkout branch notices) to stderr,
    # so collect it under Continue and decide solely from the native exit code.
    $previousPreference = $ErrorActionPreference
    try {
        $ErrorActionPreference = 'Continue'
        $output = & git -C $Repository @Arguments 2>&1
        $exitCode = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $previousPreference
    }
    if ($exitCode -ne 0) {
        throw "git $($Arguments -join ' ') failed: $((@($output) -join [Environment]::NewLine))"
    }
}

function Assert-ValidatorRejects {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Repository,
        [Parameter(Mandatory = $true)]
        [string]$Tag
    )

    try {
        & $validator -RepoRoot $Repository -Tag $Tag
    }
    catch {
        return
    }
    throw "Expected $Tag to be rejected"
}

$testRoot = Join-Path ([IO.Path]::GetTempPath()) "leopardwm-release-validation-$PID-$([Guid]::NewGuid().ToString('N'))"
$remote = Join-Path $testRoot 'origin.git'
$repository = Join-Path $testRoot 'worktree'

try {
    New-Item -ItemType Directory -Path $testRoot | Out-Null

    # Execute the validator's real checksum function against GNU-style
    # lowercase output, not merely a source-text policy assertion.
    $functionStart = $validatorSource.IndexOf('function Assert-Checksums')
    $functionEnd = $validatorSource.IndexOf("`nif (-not `$Tag", $functionStart)
    if ($functionStart -lt 0 -or $functionEnd -le $functionStart) {
        throw 'Could not extract Assert-Checksums from validator'
    }
    Invoke-Expression $validatorSource.Substring($functionStart, $functionEnd - $functionStart)
    $artifact = Join-Path $testRoot 'artifact.zip'
    $checksums = Join-Path $testRoot 'checksums.txt'
    [IO.File]::WriteAllBytes($artifact, [Text.Encoding]::UTF8.GetBytes('checksum fixture'))
    $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $artifact).Hash
    [IO.File]::WriteAllText($checksums, "$($hash.ToLowerInvariant())  artifact.zip`n")
    Assert-Checksums -Path $checksums -ArtifactPaths @($artifact)
    $wrongPrefix = if ($hash[0] -eq '0') { '1' } else { '0' }
    [IO.File]::WriteAllText($checksums, "$($wrongPrefix + $hash.Substring(1).ToLowerInvariant())  artifact.zip`n")
    try {
        Assert-Checksums -Path $checksums -ArtifactPaths @($artifact)
        throw 'Changed checksum nibble was accepted'
    }
    catch {
        if ($_.Exception.Message -eq 'Changed checksum nibble was accepted') { throw }
    }

    & git init --bare $remote | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw 'Could not initialize the local test remote'
    }
    & git init $repository | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw 'Could not initialize the local test repository'
    }

    Invoke-TestGit -Repository $repository -Arguments @('config', 'user.email', 'release-validator@example.invalid')
    Invoke-TestGit -Repository $repository -Arguments @('config', 'user.name', 'Release Validator Test')
    Invoke-TestGit -Repository $repository -Arguments @('checkout', '-b', 'main')

    [IO.File]::WriteAllText((Join-Path $repository 'Cargo.toml'), @'
[workspace]
resolver = "2"

[workspace.package]
version = "0.2.6"
'@)
    [IO.File]::WriteAllText((Join-Path $repository 'CHANGELOG.md'), @'
# Changelog

## 0.2.6-sehoon.24-rc2

- Candidate coverage.

## 0.2.6-rc1

- Off-main coverage.
'@)

    Invoke-TestGit -Repository $repository -Arguments @('add', 'Cargo.toml', 'CHANGELOG.md')
    Invoke-TestGit -Repository $repository -Arguments @('commit', '-m', 'test fixture')
    Invoke-TestGit -Repository $repository -Arguments @('remote', 'add', 'origin', $remote)
    Invoke-TestGit -Repository $repository -Arguments @('push', '-u', 'origin', 'main')

    # Established prerelease convention: core Cargo version plus SemVer suffix.
    Invoke-TestGit -Repository $repository -Arguments @('tag', 'v0.2.6-sehoon.24-rc2')
    Invoke-TestGit -Repository $repository -Arguments @('push', 'origin', '--tags')
    Invoke-TestGit -Repository $repository -Arguments @('checkout', '--detach', 'v0.2.6-sehoon.24-rc2')
    & $validator -RepoRoot $repository -Tag 'v0.2.6-sehoon.24-rc2'

    # Tags with a different core version or invalid syntax must not publish.
    Invoke-TestGit -Repository $repository -Arguments @('tag', 'v0.2.7')
    Invoke-TestGit -Repository $repository -Arguments @('tag', 'vnot-a-version')
    Assert-ValidatorRejects -Repository $repository -Tag 'v0.2.7'
    Assert-ValidatorRejects -Repository $repository -Tag 'vnot-a-version'

    # A syntactically valid tag must still resolve to the exact origin/main SHA.
    Invoke-TestGit -Repository $repository -Arguments @('checkout', '-b', 'off-main')
    [IO.File]::AppendAllText((Join-Path $repository 'CHANGELOG.md'), "`n- Off-main commit.`n")
    Invoke-TestGit -Repository $repository -Arguments @('add', 'CHANGELOG.md')
    Invoke-TestGit -Repository $repository -Arguments @('commit', '-m', 'off-main fixture')
    Invoke-TestGit -Repository $repository -Arguments @('tag', 'v0.2.6-rc1')
    Assert-ValidatorRejects -Repository $repository -Tag 'v0.2.6-rc1'

    # A version-coherent, main-anchored tag still needs its own changelog section.
    Invoke-TestGit -Repository $repository -Arguments @('checkout', 'main')
    [IO.File]::AppendAllText((Join-Path $repository 'CHANGELOG.md'), "`n- Main fixture update.`n")
    Invoke-TestGit -Repository $repository -Arguments @('add', 'CHANGELOG.md')
    Invoke-TestGit -Repository $repository -Arguments @('commit', '-m', 'missing changelog fixture')
    Invoke-TestGit -Repository $repository -Arguments @('push', 'origin', 'main')
    Invoke-TestGit -Repository $repository -Arguments @('tag', 'v0.2.6-missing')
    Invoke-TestGit -Repository $repository -Arguments @('checkout', '--detach', 'v0.2.6-missing')
    Assert-ValidatorRejects -Repository $repository -Tag 'v0.2.6-missing'

    Write-Host 'Release validation policy fixtures passed.'
}
finally {
    if (Test-Path -LiteralPath $testRoot) {
        Remove-Item -LiteralPath $testRoot -Recurse -Force
    }
}
