[CmdletBinding()]
param(
    [ValidateSet('Debug', 'Release')]
    [string]$Configuration = 'Debug',
    [string]$BuildDirectory,
    [switch]$KeepOpen
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$architecture = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'arm64' } else { 'x64' }
$build = if ($BuildDirectory) {
    [IO.Path]::GetFullPath($BuildDirectory)
} else {
    Join-Path $repo ("out\user\{0}\{1}" -f $architecture, $Configuration)
}
$artifacts = Join-Path $repo 'out\sandbox-artifacts'
$results = Join-Path $repo 'out\sandbox-results'
$generated = Join-Path $repo 'out\forward-slash-windows.wsb'

if (-not (Test-Path -LiteralPath (Join-Path $build 'fswcore_tests.exe'))) {
    throw 'Build the user-mode targets before starting Sandbox.'
}

New-Item -ItemType Directory -Force -Path $artifacts, $results | Out-Null
$staleArtifacts = @(
    'fswhost.exe', 'fswhook.dll', 'fsw_hook_integration.exe',
    'fsw_address_bar_integration.exe'
)
foreach ($staleArtifact in $staleArtifacts) {
    $stalePath = Join-Path $artifacts $staleArtifact
    if (Test-Path -LiteralPath $stalePath) {
        Remove-Item -LiteralPath $stalePath -Force
    }
}
$resultFile = Join-Path $results 'sandbox-results.json'
if (Test-Path -LiteralPath $resultFile) {
    Remove-Item -LiteralPath $resultFile
}
Copy-Item -LiteralPath (Join-Path $build 'fswcore_tests.exe') -Destination $artifacts -Force
Copy-Item -LiteralPath (Join-Path $build 'fswctl.exe') -Destination $artifacts -Force
Copy-Item -LiteralPath (Join-Path $build 'fswbroker.exe') -Destination $artifacts -Force
Copy-Item -LiteralPath (Join-Path $repo 'test\sandbox\bootstrap.ps1') -Destination (Join-Path $artifacts 'sandbox-bootstrap.ps1') -Force

$xml = Get-Content -LiteralPath (Join-Path $repo 'test\sandbox\user-mode.wsb.template') -Raw
$xml = $xml.Replace('@@ARTIFACTS@@', [Security.SecurityElement]::Escape($artifacts))
$xml = $xml.Replace('@@RESULTS@@', [Security.SecurityElement]::Escape($results))
Set-Content -LiteralPath $generated -Value $xml -Encoding utf8

$started = wsb.exe start --config $xml --raw | ConvertFrom-Json
if (-not $started.Id) {
    throw 'Windows Sandbox did not return an environment ID.'
}
try {
    wsb.exe connect --id $started.Id --raw
    $deadline = (Get-Date).AddSeconds(90)
    while ((Get-Date) -lt $deadline -and -not (Test-Path -LiteralPath $resultFile)) {
        Start-Sleep -Seconds 2
    }
    if (-not (Test-Path -LiteralPath $resultFile)) {
        throw "Sandbox did not produce $resultFile within 90 seconds."
    }
    $report = Get-Content -LiteralPath $resultFile -Raw | ConvertFrom-Json
    if (-not $report.passed) {
        throw "One or more Sandbox tests failed. See $resultFile"
    }
    Write-Host "Windows Sandbox tests passed. Results: $resultFile"
    if ($KeepOpen) {
        Write-Host "Windows Sandbox left running. Environment ID: $($started.Id)"
    }
} finally {
    if (-not $KeepOpen) {
        wsb.exe stop --id $started.Id --raw | Out-Null
    }
}
