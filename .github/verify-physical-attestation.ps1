#Requires -Version 5.1
param(
    [Parameter(Mandatory = $true)]
    [string]$Path,
    [Parameter(Mandatory = $true)]
    [string]$CandidateSha,
    [Parameter(Mandatory = $true)]
    [string]$Repository,
    [Parameter(Mandatory = $true)]
    [string]$RunId,
    [Parameter(Mandatory = $true)]
    [string]$RunAttempt
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if ($CandidateSha -cnotmatch '^[0-9a-f]{40}$') {
    throw 'Physical attestation candidate must be one full lowercase commit SHA'
}
if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
    throw "Physical attestation is missing: $Path"
}

$attestation = Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json
$required = @(
    'schema_version', 'commit_sha', 'repository', 'workflow_run_id',
    'run_attempt', 'dual_monitor', 'physical_click', 'noninjected_click', 'conclusion'
)
foreach ($name in $required) {
    if ($attestation.PSObject.Properties.Name -cnotcontains $name) {
        throw "Physical attestation is missing $name"
    }
}
if ([int]$attestation.schema_version -ne 1) {
    throw 'Unsupported physical attestation schema'
}
if ([string]$attestation.commit_sha -cne $CandidateSha) {
    throw 'Physical attestation commit does not match the release candidate'
}
if ([string]$attestation.repository -cne $Repository) {
    throw 'Physical attestation repository does not match this release'
}
if ([string]$attestation.workflow_run_id -cne $RunId -or
    [string]$attestation.run_attempt -cne $RunAttempt) {
    throw 'Physical attestation does not belong to this workflow run and attempt'
}
if ($attestation.dual_monitor -ne $true -or
    $attestation.physical_click -ne $true -or
    $attestation.noninjected_click -ne $true) {
    throw 'Physical attestation must require dual monitors and a noninjected physical click'
}
if ([string]$attestation.conclusion -cne 'success') {
    throw 'Physical attestation did not complete successfully'
}

Write-Host "Physical release attestation verified for $CandidateSha"
