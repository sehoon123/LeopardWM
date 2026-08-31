#Requires -Version 5.1
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$githubDir = Split-Path -Parent $PSScriptRoot
$verifier = Join-Path $githubDir 'verify-physical-attestation.ps1'
$workflow = Get-Content -LiteralPath (Join-Path $githubDir 'workflows/release.yml') -Raw
foreach ($marker in @(
    'environment: release-hardware',
    'needs: physical-gate',
    "LEOPARDWM_REQUIRE_DUAL_MONITOR: '1'",
    "LEOPARDWM_REQUIRE_PHYSICAL_CLICK: '1'",
    "LEOPARDWM_REQUIRE_NONINJECTED_CLICK: '1'",
    'physical-gate-${{ github.sha }}-${{ github.run_attempt }}'
)) {
    if (-not $workflow.Contains($marker)) {
        throw "Release workflow is missing physical gate marker: $marker"
    }
}

$candidate = 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
$repository = 'sehoon123/LeopardWM'
$runId = '12345'
$attempt = '2'
$temp = Join-Path ([IO.Path]::GetTempPath()) "leopardwm-physical-attestation-$PID-$([Guid]::NewGuid().ToString('N')).json"

function New-Attestation {
    [ordered]@{
        schema_version = 1
        commit_sha = $candidate
        repository = $repository
        workflow_run_id = $runId
        run_attempt = $attempt
        dual_monitor = $true
        physical_click = $true
        noninjected_click = $true
        conclusion = 'success'
    }
}

function Write-Attestation {
    param([Parameter(Mandatory = $true)][Collections.IDictionary]$Value)
    $Value | ConvertTo-Json | Set-Content -LiteralPath $temp -Encoding UTF8
}

function Assert-Rejected {
    param([Parameter(Mandatory = $true)][Collections.IDictionary]$Value)
    Write-Attestation $Value
    try {
        & $verifier -Path $temp -CandidateSha $candidate -Repository $repository -RunId $runId -RunAttempt $attempt
    }
    catch {
        return
    }
    throw 'Invalid physical attestation was accepted'
}

try {
    Write-Attestation (New-Attestation)
    & $verifier -Path $temp -CandidateSha $candidate -Repository $repository -RunId $runId -RunAttempt $attempt

    $bad = New-Attestation
    $bad.commit_sha = 'baaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
    Assert-Rejected $bad
    $bad = New-Attestation
    $bad.commit_sha = 'aaaaaaa'
    Assert-Rejected $bad
    $bad = New-Attestation
    $bad.repository = 'someone/else'
    Assert-Rejected $bad
    $bad = New-Attestation
    $bad.workflow_run_id = '999'
    Assert-Rejected $bad
    $bad = New-Attestation
    $bad.run_attempt = '3'
    Assert-Rejected $bad
    $bad = New-Attestation
    $bad.dual_monitor = $false
    Assert-Rejected $bad
    $bad = New-Attestation
    [void]$bad.Remove('physical_click')
    Assert-Rejected $bad
    $bad = New-Attestation
    $bad.noninjected_click = $false
    Assert-Rejected $bad
    $bad = New-Attestation
    $bad.conclusion = 'failure'
    Assert-Rejected $bad

    Write-Host 'Physical attestation policy fixtures passed.'
}
finally {
    Remove-Item -LiteralPath $temp -Force -ErrorAction SilentlyContinue
}
