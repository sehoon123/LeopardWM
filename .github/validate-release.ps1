#Requires -Version 5.1
<#
.SYNOPSIS
Validates the provenance and package identity of a LeopardWM release candidate.

.DESCRIPTION
The release workflow calls this script before building and again after packaging.
A valid tag resolves to the checked-out commit, that commit is exactly origin/main,
the tag's core version equals [workspace.package].version, and CHANGELOG.md has a
matching section. A SemVer prerelease suffix is allowed so tags such as
v0.2.6-sehoon.24-rc2 remain valid while the MSI retains Cargo's 0.2.6 version.
When artifact paths are supplied, the script also validates ZIP inventory, MSI
properties/files, and checksums before publication.
#>
param(
    [string]$RepoRoot = (Split-Path -Parent $PSScriptRoot),
    [Parameter(Mandatory = $true)]
    [string]$Tag,
    [string]$ZipPath,
    [string]$MsiPath,
    [string]$ChecksumsPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$RepoRoot = (Resolve-Path -LiteralPath $RepoRoot).Path

function Invoke-RepositoryGit {
    param(
        [Parameter(Mandatory = $true)]
        [string[]]$Arguments
    )

    $previousPreference = $ErrorActionPreference
    try {
        # Git progress is written to stderr even on success. PowerShell 5 turns
        # that into non-terminating ErrorRecord objects; the native exit code is
        # the authoritative result.
        $ErrorActionPreference = 'Continue'
        $output = & git -C $RepoRoot @Arguments 2>&1
        $exitCode = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $previousPreference
    }
    if ($exitCode -ne 0) {
        $text = (@($output) | ForEach-Object { "$_" }) -join [Environment]::NewLine
        throw "git $($Arguments -join ' ') failed: $text"
    }

    $text = (@($output) | ForEach-Object { "$_" }) -join [Environment]::NewLine
    return $text.Trim()
}

function Get-WorkspacePackageVersion {
    param(
        [Parameter(Mandatory = $true)]
        [string]$CargoTomlPath
    )

    $contents = [IO.File]::ReadAllText($CargoTomlPath)
    $section = [regex]::Match($contents, '(?ms)^\[workspace\.package\]\s*$.*?(?=^\[|\z)')
    if (-not $section.Success) {
        throw "[workspace.package] was not found in $CargoTomlPath"
    }

    $version = [regex]::Match($section.Value, '(?m)^\s*version\s*=\s*"(?<version>[^"]+)"\s*(?:#.*)?$')
    if (-not $version.Success) {
        throw "[workspace.package].version was not found in $CargoTomlPath"
    }

    return $version.Groups['version'].Value
}

function Assert-ChangelogSection {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ChangelogPath,
        [Parameter(Mandatory = $true)]
        [string]$ReleaseVersion
    )

    $contents = [IO.File]::ReadAllText($ChangelogPath)
    $header = '^##\s+\[?' + [regex]::Escape($ReleaseVersion) + '\]?\s*$'
    if (-not [regex]::IsMatch($contents, $header, [Text.RegularExpressions.RegexOptions]::Multiline)) {
        throw "CHANGELOG.md has no section headed '## $ReleaseVersion'"
    }
}

function Resolve-ExpectedArtifact {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path,
        [Parameter(Mandatory = $true)]
        [string]$ExpectedName
    )

    $resolved = (Resolve-Path -LiteralPath $Path).Path
    if ([IO.Path]::GetFileName($resolved) -cne $ExpectedName) {
        throw "Expected artifact name $ExpectedName, got $([IO.Path]::GetFileName($resolved))"
    }
    return $resolved
}

function Assert-ZipInventory {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path,
        [Parameter(Mandatory = $true)]
        [string]$ArchiveDirectory,
        [Parameter(Mandatory = $true)]
        [string]$BinaryDirectory
    )

    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [IO.Compression.ZipFile]::OpenRead($Path)
    try {
        $expectedEntries = @(
            "$ArchiveDirectory/leopardwm.exe",
            "$ArchiveDirectory/leopardwm-cli.exe",
            "$ArchiveDirectory/lwm.exe",
            "$ArchiveDirectory/leopardwm-watchdog.exe",
            "$ArchiveDirectory/README.md",
            "$ArchiveDirectory/LICENSE",
            "$ArchiveDirectory/CHANGELOG.md"
        )
        $actualEntries = @($archive.Entries | Where-Object { -not $_.FullName.EndsWith('/') } | ForEach-Object { $_.FullName })

        foreach ($expectedEntry in $expectedEntries) {
            $entryMatches = @($archive.Entries | Where-Object { [String]::Equals($_.FullName, $expectedEntry, [StringComparison]::Ordinal) })
            if ($entryMatches.Count -ne 1) {
                throw "ZIP must contain exactly one $expectedEntry entry; found $($entryMatches.Count)"
            }
            if ($entryMatches[0].Length -eq 0) {
                throw "ZIP entry $expectedEntry is empty"
            }
            if ($expectedEntry.EndsWith('.exe')) {
                $binaryPath = Join-Path $BinaryDirectory ([IO.Path]::GetFileName($expectedEntry))
                if (-not (Test-Path -LiteralPath $binaryPath)) {
                    throw "Release binary is missing: $binaryPath"
                }
                $stream = $entryMatches[0].Open()
                $hasher = [Security.Cryptography.SHA256]::Create()
                try {
                    $zipHash = ([BitConverter]::ToString($hasher.ComputeHash($stream))).Replace('-', '')
                }
                finally {
                    $hasher.Dispose()
                    $stream.Dispose()
                }
                $binaryHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $binaryPath).Hash
                if ($zipHash -cne $binaryHash) {
                    throw "ZIP binary $expectedEntry does not match $binaryPath"
                }
            }
        }

        $unexpectedEntries = @($actualEntries | Where-Object { $_ -notin $expectedEntries })
        if ($unexpectedEntries.Count -gt 0) {
            throw "ZIP contains unexpected file entries: $($unexpectedEntries -join ', ')"
        }
    }
    finally {
        $archive.Dispose()
    }
}

function Get-MsiProperty {
    param(
        [Parameter(Mandatory = $true)]
        [object]$Database,
        [Parameter(Mandatory = $true)]
        [string]$Name
    )

    $view = $Database.OpenView("SELECT ``Value`` FROM ``Property`` WHERE ``Property`` = '$Name'")
    try {
        [void]$view.Execute()
        $record = $view.Fetch()
        if ($null -eq $record) {
            throw "MSI property $Name is missing"
        }
        return $record.StringData(1)
    }
    finally {
        [void]$view.Close()
    }
}

function Get-MsiFileNames {
    param(
        [Parameter(Mandatory = $true)]
        [object]$Database
    )

    $view = $Database.OpenView("SELECT ``FileName`` FROM ``File``")
    try {
        [void]$view.Execute()
        $names = @()
        while ($true) {
            $record = $view.Fetch()
            if ($null -eq $record) {
                break
            }
            $names += ($record.StringData(1) -split '\|')[-1]
        }
        return $names
    }
    finally {
        [void]$view.Close()
    }
}

function Assert-MsiIdentity {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path,
        [Parameter(Mandatory = $true)]
        [string]$CargoVersion
    )

    $installer = $null
    $database = $null
    try {
        $installer = New-Object -ComObject WindowsInstaller.Installer
        $database = $installer.OpenDatabase($Path, 0)
        $productName = Get-MsiProperty -Database $database -Name 'ProductName'
        if ($productName -cne 'LeopardWM') {
            throw "MSI ProductName must be LeopardWM, got $productName"
        }

        $productVersion = Get-MsiProperty -Database $database -Name 'ProductVersion'
        if ($productVersion -cne $CargoVersion) {
            throw "MSI ProductVersion must match Cargo version $CargoVersion, got $productVersion"
        }

        $expectedFiles = @('leopardwm.exe', 'leopardwm-cli.exe', 'lwm.exe', 'leopardwm-watchdog.exe', 'License.rtf')
        $actualFiles = @(Get-MsiFileNames -Database $database)
        foreach ($expectedFile in $expectedFiles) {
            if ($actualFiles -cnotcontains $expectedFile) {
                throw "MSI is missing expected file $expectedFile"
            }
        }
    }
    finally {
        if ($null -ne $database) {
            [void][Runtime.InteropServices.Marshal]::ReleaseComObject($database)
        }
        if ($null -ne $installer) {
            [void][Runtime.InteropServices.Marshal]::ReleaseComObject($installer)
        }
    }
}

function Assert-Checksums {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path,
        [Parameter(Mandatory = $true)]
        [string[]]$ArtifactPaths
    )

    $records = @{}
    foreach ($line in (Get-Content -LiteralPath $Path)) {
        $match = [regex]::Match($line, '^(?<hash>[A-Fa-f0-9]{64})\s+\*?(?<name>.+)$')
        if (-not $match.Success) {
            throw "Invalid checksum line: $line"
        }

        $name = [IO.Path]::GetFileName($match.Groups['name'].Value.Trim())
        if ([string]::IsNullOrWhiteSpace($name) -or $records.ContainsKey($name)) {
            throw "Invalid or duplicate checksum entry: $line"
        }
        $records[$name] = $match.Groups['hash'].Value
    }

    $expectedNames = @($ArtifactPaths | ForEach-Object { [IO.Path]::GetFileName($_) })
    if ($records.Count -ne $expectedNames.Count) {
        throw "checksums.txt must contain exactly $($expectedNames.Count) artifact entries"
    }

    foreach ($artifactPath in $ArtifactPaths) {
        $name = [IO.Path]::GetFileName($artifactPath)
        if (-not $records.ContainsKey($name)) {
            throw "checksums.txt is missing $name"
        }
        $actualHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $artifactPath).Hash
        if ($actualHash -ine $records[$name]) {
            throw "Checksum mismatch for $name"
        }
    }
}

if (-not $Tag.StartsWith('v')) {
    throw "Release tag must start with v: $Tag"
}

$cargoToml = Join-Path $RepoRoot 'Cargo.toml'
$changelog = Join-Path $RepoRoot 'CHANGELOG.md'
$cargoVersion = Get-WorkspacePackageVersion -CargoTomlPath $cargoToml
if ($cargoVersion -notmatch '^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$') {
    throw "[workspace.package].version must be a three-part SemVer version for WiX, got $cargoVersion"
}

$releaseVersion = $Tag.Substring(1)
$semverIdentifier = '(?:0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)'
$tagPattern = '^v' + [regex]::Escape($cargoVersion) + '(?:-' + $semverIdentifier + '(?:\.' + $semverIdentifier + ')*)?$'
if ($Tag -notmatch $tagPattern) {
    throw "Release tag $Tag must use Cargo version $cargoVersion with an optional SemVer prerelease suffix"
}

Assert-ChangelogSection -ChangelogPath $changelog -ReleaseVersion $releaseVersion

Invoke-RepositoryGit -Arguments @('fetch', '--no-tags', 'origin', '+refs/heads/main:refs/remotes/origin/main') | Out-Null
Invoke-RepositoryGit -Arguments @('show-ref', '--verify', '--quiet', "refs/tags/$Tag") | Out-Null
$headCommit = Invoke-RepositoryGit -Arguments @('rev-parse', 'HEAD^{commit}')
$tagCommit = Invoke-RepositoryGit -Arguments @('rev-parse', "$($Tag)^{commit}")
$mainCommit = Invoke-RepositoryGit -Arguments @('rev-parse', 'origin/main^{commit}')

if ($headCommit -cne $tagCommit) {
    throw "Checked-out commit $headCommit does not match tag $Tag ($tagCommit)"
}
if ($headCommit -cne $mainCommit) {
    throw "Tag $Tag resolves to $headCommit but origin/main resolves to $mainCommit"
}

$hasZip = -not [string]::IsNullOrWhiteSpace($ZipPath)
$hasMsi = -not [string]::IsNullOrWhiteSpace($MsiPath)
$hasChecksums = -not [string]::IsNullOrWhiteSpace($ChecksumsPath)
if ($hasZip -or $hasMsi -or $hasChecksums) {
    if (-not ($hasZip -and $hasMsi -and $hasChecksums)) {
        throw 'ZipPath, MsiPath, and ChecksumsPath must be supplied together'
    }

    $archiveDirectory = "LeopardWM-$releaseVersion-x86_64-windows"
    $zip = Resolve-ExpectedArtifact -Path $ZipPath -ExpectedName "$archiveDirectory.zip"
    $msi = Resolve-ExpectedArtifact -Path $MsiPath -ExpectedName "LeopardWM-$releaseVersion-x86_64.msi"
    $checksums = (Resolve-Path -LiteralPath $ChecksumsPath).Path
    $binaryDirectory = Join-Path $RepoRoot 'target/x86_64-pc-windows-msvc/release'
    if (-not (Test-Path -LiteralPath $binaryDirectory)) {
        throw "Release binary directory is missing: $binaryDirectory"
    }

    Assert-ZipInventory -Path $zip -ArchiveDirectory $archiveDirectory -BinaryDirectory $binaryDirectory
    Assert-MsiIdentity -Path $msi -CargoVersion $cargoVersion
    Assert-Checksums -Path $checksums -ArtifactPaths @($zip, $msi)
    Write-Host "Release artifacts validated: $([IO.Path]::GetFileName($zip)), $([IO.Path]::GetFileName($msi)), and checksums.txt"
}

Write-Host "Release candidate validated: $Tag at $headCommit (origin/main) with Cargo version $cargoVersion"
