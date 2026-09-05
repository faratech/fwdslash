<#
.SYNOPSIS
    Verifies an .msixbundle is a valid, uploadable Microsoft Store submission.

.DESCRIPTION
    Unbundles and unpacks the bundle with makeappx and asserts, per
    architecture package, that the manifest carries the Partner Center identity
    (Name / Publisher / Version), that both x64 and arm64 are present, that the
    runtime payload is complete, and that nothing which must never reach the
    Store made it in.

    Two of those assertions exist because getting them wrong wastes a
    submission rather than failing loudly:

      * The bundle must be UNSIGNED. Partner Center re-signs the package with
        the Store certificate, so a signature we applied is discarded at best
        and rejected at worst. An AppxSignature.p7x inside any package means
        the release workflow signed the wrong artifact.
      * The manifest must NOT declare `unvirtualizedResources`. Dropping that
        restricted capability is what removed the Microsoft-approval
        requirement from the submission (docs/store-submission.md section 2);
        it must not come back by accident.

    Run it after tools\Package-Msix.ps1; .github\workflows\release.yml runs the
    same script against the bundle it attaches to the GitHub release.

.EXAMPLE
    .\tools\Test-StoreBundle.ps1 -Bundle out\msix-store\fwdslash-0.0.3.0-store-unsigned.msixbundle -Version 0.0.3.0
#>
[CmdletBinding()]
param(
    # The .msixbundle to inspect.
    [Parameter(Mandatory)]
    [string]$Bundle,

    # The four-part MSIX version every package inside must carry.
    [Parameter(Mandatory)]
    [ValidatePattern('^\d+\.\d+\.\d+\.0$')]
    [string]$Version,

    # The Partner Center identity for Store ID 9P51CM0MTMK2 — the defaults of
    # tools\Package-Msix.ps1, repeated here so this script fails when the
    # packager was invoked with the GitHub flavor's publisher by mistake.
    [string]$IdentityName = '32827MikeFara.fwdslash',
    [string]$Publisher = 'CN=ABDB6B3F-DF9E-447D-BC0E-4DA7BAFD14C4',

    # Scratch directory for the unpacked copies. Removed and recreated.
    [string]$WorkDirectory
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repo = Split-Path -Parent $PSScriptRoot

if (-not (Test-Path -LiteralPath $Bundle -PathType Leaf)) {
    throw "Bundle not found: $Bundle"
}
$bundleItem = Get-Item -LiteralPath $Bundle

# Same resolution order as tools\Package-Msix.ps1: the SDK pinned under
# packages\ first, then whichever machine-wide kit is newest.
function Resolve-PackagingTool {
    param([string]$Name)

    $hostArchitecture = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'arm64' } else { 'x64' }
    $candidates = @()
    $nugetBin = Join-Path $repo 'packages\Microsoft.Windows.SDK.CPP.10.0.28000.2526\c\bin\10.0.28000.0'
    $candidates += Join-Path $nugetBin "$hostArchitecture\$Name"
    $kitBin = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
    if (Test-Path -LiteralPath $kitBin) {
        foreach ($version in (Get-ChildItem -LiteralPath $kitBin -Directory |
                Where-Object { $_.Name -match '^10\.' } |
                Sort-Object { [Version]$_.Name } -Descending)) {
            $candidates += Join-Path $version.FullName "$hostArchitecture\$Name"
        }
    }
    foreach ($candidate in $candidates) {
        if (Test-Path -LiteralPath $candidate -PathType Leaf) { return $candidate }
    }
    throw "Could not locate $Name in the pinned SDK or any installed Windows Kit."
}

$makeappx = Resolve-PackagingTool 'makeappx.exe'

if (-not $WorkDirectory) {
    $WorkDirectory = Join-Path (Split-Path -Parent $bundleItem.FullName) 'inspect'
}
if (Test-Path -LiteralPath $WorkDirectory) {
    Remove-Item -LiteralPath $WorkDirectory -Recurse -Force
}
$bundleOut = Join-Path $WorkDirectory 'bundle'
New-Item -ItemType Directory -Force -Path $bundleOut | Out-Null

& $makeappx unbundle /p $bundleItem.FullName /d $bundleOut /o | ForEach-Object { Write-Verbose $_ }
if ($LASTEXITCODE -ne 0) { throw "makeappx could not unbundle $($bundleItem.Name)." }

$packages = @(Get-ChildItem -LiteralPath $bundleOut -Filter '*.msix')
if ($packages.Count -ne 2) {
    throw "Expected two architecture packages in the bundle; found $($packages.Count)."
}

# Every file the product needs at runtime. The adapter list is the same one
# tools\Package-Msix.ps1 asserts into the stage — repeated here so a bundle
# assembled some other way is still checked.
$requiredFiles = @(
    'fwdslash.exe',
    'fswbroker.exe',
    'fswsettings.exe',
    'AppxManifest.xml',
    'resources.pri',
    'LICENSE',
    'Assets\Square44x44Logo.png',
    'Assets\Square150x150Logo.png',
    'Assets\StoreLogo.png',
    'shell\cmd\fsw-autorun.cmd',
    'shell\cmd\fsw-dir.cmd',
    'shell\cmd\fsw-cd.cmd',
    'shell\cmd\fsw-pushd.cmd',
    'shell\powershell\ForwardSlashWindows.psm1'
)

$foundArchitectures = @()

foreach ($package in $packages) {
    $unpacked = Join-Path $WorkDirectory $package.BaseName
    New-Item -ItemType Directory -Force -Path $unpacked | Out-Null
    & $makeappx unpack /p $package.FullName /d $unpacked /o | ForEach-Object { Write-Verbose $_ }
    if ($LASTEXITCODE -ne 0) { throw "makeappx could not unpack $($package.Name)." }

    [xml]$manifest = Get-Content -Raw -LiteralPath (Join-Path $unpacked 'AppxManifest.xml')
    $identity = $manifest.SelectSingleNode("/*[local-name()='Package']/*[local-name()='Identity']")
    if (-not $identity) { throw "$($package.Name): the manifest has no Identity element." }

    $identityName = $identity.GetAttribute('Name')
    $identityPublisher = $identity.GetAttribute('Publisher')
    $identityVersion = $identity.GetAttribute('Version')
    $architecture = $identity.GetAttribute('ProcessorArchitecture')

    if ($identityName -ne $IdentityName) {
        throw "$($package.Name): Identity/Name is '$identityName', expected '$IdentityName'."
    }
    if ($identityPublisher -ne $Publisher) {
        throw ("$($package.Name): Identity/Publisher is '$identityPublisher', expected '$Publisher'. " +
               'A Store upload must carry the Partner Center publisher, not the Trusted Signing subject.')
    }
    if ($identityVersion -ne $Version) {
        throw "$($package.Name): Identity/Version is '$identityVersion', expected '$Version'."
    }
    if ($architecture -notin @('x64', 'arm64')) {
        throw "$($package.Name): unexpected ProcessorArchitecture '$architecture'."
    }
    $foundArchitectures += $architecture

    $publisherDisplayName = $manifest.SelectSingleNode(
        "/*[local-name()='Package']/*[local-name()='Properties']/*[local-name()='PublisherDisplayName']")
    if (-not $publisherDisplayName -or $publisherDisplayName.InnerText -ne 'WindowsForum.com') {
        throw "$($package.Name): PublisherDisplayName must be 'WindowsForum.com' for the Store listing."
    }

    $capabilities = @($manifest.SelectNodes("//*[local-name()='Capability']") |
        ForEach-Object { $_.GetAttribute('Name') })
    if ('runFullTrust' -notin $capabilities) {
        throw "$($package.Name): missing the runFullTrust capability."
    }
    if ('unvirtualizedResources' -in $capabilities) {
        throw ("$($package.Name): declares unvirtualizedResources. That restricted capability " +
               'requires Microsoft approval and was deliberately removed (docs/store-submission.md section 2).')
    }

    $framework = $manifest.SelectSingleNode(
        "//*[local-name()='PackageDependency' and @Name='Microsoft.WindowsAppRuntime.2']")
    if (-not $framework) {
        throw "$($package.Name): missing the Microsoft.WindowsAppRuntime.2 dependency."
    }

    foreach ($file in $requiredFiles) {
        if (-not (Test-Path -LiteralPath (Join-Path $unpacked $file) -PathType Leaf)) {
            throw "$($package.Name): missing required payload file '$file'."
        }
    }

    # Partner Center re-signs; anything we signed is dead weight at best.
    if (Test-Path -LiteralPath (Join-Path $unpacked 'AppxSignature.p7x')) {
        throw "$($package.Name): the package is signed. The Store submission artifact must be unsigned."
    }

    # driver\ is production-gated and must never enter a package, and .pdb
    # files would leak symbols into the Store payload.
    $forbidden = @(Get-ChildItem -LiteralPath $unpacked -Recurse -File |
        Where-Object { $_.Extension -in @('.sys', '.pdb', '.inf') })
    if ($forbidden.Count -gt 0) {
        throw "$($package.Name): payload contains files that must never ship: $(($forbidden.Name | Sort-Object -Unique) -join ', ')."
    }

    Write-Host "  $($package.Name): $architecture, $identityName $identityVersion, unsigned, payload complete"
}

$architectureSet = @($foundArchitectures | Sort-Object -Unique) -join ','
if ($architectureSet -ne 'arm64,x64') {
    throw "Bundle architectures are '$($foundArchitectures -join ',')', expected x64 and arm64."
}

Remove-Item -LiteralPath $WorkDirectory -Recurse -Force

Write-Host ''
Write-Host "Store bundle validated: $($bundleItem.Name) ($([Math]::Round($bundleItem.Length / 1MB, 2)) MB)"
Write-Host "  identity  $IdentityName / $Publisher"
Write-Host "  version   $Version"
Write-Host '  packages  x64 + arm64, unsigned, runFullTrust only'
