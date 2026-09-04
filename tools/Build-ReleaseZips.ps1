# Builds the per-architecture release ZIPs: signs the three executables with
# Azure Trusted Signing first (see signing\README.md), then archives the
# runtime payload — exes, shell adapters, LICENSE, and the bootstrap installer
# is published separately on the release page.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Version
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$signing = Join-Path $repo 'signing'

foreach ($triple in 'aarch64-pc-windows-msvc', 'x86_64-pc-windows-msvc') {
    $arch = if ($triple -eq 'aarch64-pc-windows-msvc') { 'arm64' } else { 'x64' }
    $binaries = Join-Path $repo "target\$triple\release"
    $stage = Join-Path $repo "out\package\forward-slash-windows-$Version-$arch"
    if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
    New-Item -ItemType Directory -Force -Path (Join-Path $stage 'shell\cmd') | Out-Null
    New-Item -ItemType Directory -Force -Path (Join-Path $stage 'shell\powershell') | Out-Null

    foreach ($exe in 'fwdslash.exe', 'fswbroker.exe', 'fswsettings.exe') {
        # Sign the source binary before it enters the archive.
        & (Join-Path $signing 'sign.ps1') (Join-Path $binaries $exe) `
            -Description 'Forward Slash Windows'
        if ($LASTEXITCODE -ne 0) { throw "Signing failed for $exe." }
        Copy-Item (Join-Path $binaries $exe) $stage
    }
    Copy-Item (Join-Path $repo 'shell\cmd\*') (Join-Path $stage 'shell\cmd') -Force
    Copy-Item (Join-Path $repo 'shell\powershell\*') (Join-Path $stage 'shell\powershell') -Force
    Copy-Item (Join-Path $repo 'LICENSE') $stage

    $zip = Join-Path $repo "out\package\forward-slash-windows-$Version-$arch.zip"
    Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip -Force
    Write-Host "Built $zip"
}
