$ErrorActionPreference = 'Stop'
$verify = Join-Path (Get-Location) 'published-verification'
New-Item -ItemType Directory -Path $verify -Force | Out-Null
gh release download $env:RELEASE_TAG --repo $env:GITHUB_REPOSITORY --dir $verify
if ($LASTEXITCODE -ne 0) { throw 'Release download failed' }

foreach ($line in Get-Content (Join-Path $verify 'checksums.txt')) {
    if ($line -notmatch '^([0-9a-f]{64}) \*(.+)$') { throw "Invalid checksum line: $line" }
    $name = Split-Path $Matches[2] -Leaf
    $path = Join-Path $verify $name
    if (-not (Test-Path $path)) { throw "Missing published asset: $name" }
    $actual = (Get-FileHash $path -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $Matches[1]) { throw "Checksum mismatch: $name" }
}

$zip = Join-Path $verify "LeopardWM-$env:RELEASE_VERSION-x86_64-windows.zip"
7z t $zip | Out-Host
if ($LASTEXITCODE -ne 0) { throw 'Published ZIP CRC check failed' }
$extract = Join-Path $verify 'zip-extracted'
7z x $zip "-o$extract" -y | Out-Host
if ($LASTEXITCODE -ne 0) { throw 'Published ZIP extraction failed' }

$msi = Join-Path $verify "LeopardWM-$env:RELEASE_VERSION-x86_64.msi"
$admin = Join-Path $verify 'msi-admin-image'
New-Item -ItemType Directory -Path $admin -Force | Out-Null
$arguments = "/a `"$msi`" /qn TARGETDIR=`"$admin`""
$process = Start-Process "$env:SystemRoot\System32\msiexec.exe" -ArgumentList $arguments -Wait -PassThru
if ($process.ExitCode -ne 0) { throw "Published MSI install failed: $($process.ExitCode)" }

foreach ($binary in @('leopardwm.exe', 'leopardwm-cli.exe', 'lwm.exe', 'leopardwm-watchdog.exe')) {
    $standalone = Join-Path $verify $binary
    $fromZip = Get-ChildItem $extract -Recurse -Filter $binary | Select-Object -First 1
    $fromMsi = Get-ChildItem $admin -Recurse -Filter $binary | Select-Object -First 1
    if (-not $fromZip -or -not $fromMsi) { throw "Packaged binary missing: $binary" }
    $expected = (Get-FileHash $standalone -Algorithm SHA256).Hash
    if ((Get-FileHash $fromZip.FullName -Algorithm SHA256).Hash -ne $expected) {
        throw "ZIP binary mismatch: $binary"
    }
    if ((Get-FileHash $fromMsi.FullName -Algorithm SHA256).Hash -ne $expected) {
        throw "MSI binary mismatch: $binary"
    }
}

$manifest = Get-Content (Join-Path $verify 'leopardwm.json') | ConvertFrom-Json
$zipHash = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLowerInvariant()
if ($manifest.version -ne $env:RELEASE_VERSION) { throw 'Scoop version mismatch' }
if ($manifest.architecture.'64bit'.hash -ne $zipHash) { throw 'Scoop ZIP hash mismatch' }

git fetch origin --tags --force
$tagSha = (git rev-list -n 1 $env:RELEASE_TAG).Trim()
$mainSha = ((git ls-remote origin refs/heads/main) -split '\s+')[0]
if ($tagSha -ne $env:SOURCE_SHA -or $mainSha -ne $env:SOURCE_SHA) {
    throw "Source identity mismatch: tag=$tagSha main=$mainSha expected=$env:SOURCE_SHA"
}

$release = gh release view $env:RELEASE_TAG --repo $env:GITHUB_REPOSITORY --json tagName,isDraft,isPrerelease,targetCommitish,assets | ConvertFrom-Json
if ($release.tagName -ne $env:RELEASE_TAG -or $release.isDraft -or -not $release.isPrerelease) {
    throw 'Published release metadata mismatch'
}
$expectedAssets = @(
    "LeopardWM-$env:RELEASE_VERSION-x86_64-windows.zip",
    "LeopardWM-$env:RELEASE_VERSION-x86_64.msi",
    'leopardwm.exe','leopardwm-cli.exe','lwm.exe','leopardwm-watchdog.exe',
    'checksums.txt','full-audit-v0.2.6-sehoon.10.md','leopardwm.json'
)
$actualAssets = @($release.assets | ForEach-Object { $_.name })
foreach ($asset in $expectedAssets) {
    if ($asset -notin $actualAssets) { throw "Missing release asset: $asset" }
}
