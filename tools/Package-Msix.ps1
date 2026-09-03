[CmdletBinding()]
param(
    [ValidateSet('x64', 'ARM64')]
    [string[]]$Architecture = @('x64', 'ARM64'),

    [ValidateSet('Debug', 'Release')]
    [string]$Configuration = 'Release',

    # Four-part MSIX version. Defaults to the product version compiled into the
    # binaries so the package can never disagree with what it contains. The Store
    # requires the revision (fourth) field to be 0.
    [string]$Version,

    # From Partner Center: Product > Product management > Product identity, for
    # Store ID 9P51CM0MTMK2. These must match exactly or the upload is rejected.
    # PackageFamilyName is 32827MikeFara.fwdslash_t6j5qexy2jpp2.
    [string]$IdentityName = '32827MikeFara.fwdslash',
    [string]$Publisher = 'CN=ABDB6B3F-DF9E-447D-BC0E-4DA7BAFD14C4',
    [string]$PublisherDisplayName = 'WindowsForum.com',

    # Local install testing only. Store submissions are uploaded unsigned and
    # Partner Center re-signs them.
    [string]$CertificatePath,
    [string]$CertificatePassword,

    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repo = Split-Path -Parent $PSScriptRoot
$packagingRoot = Join-Path $repo 'packaging'
$manifestTemplate = Join-Path $packagingRoot 'AppxManifest.xml'
$priConfig = Join-Path $packagingRoot 'priconfig.xml'
$assetSource = Join-Path $packagingRoot 'Assets'
$outputRoot = Join-Path $repo 'out\msix'

foreach ($required in $manifestTemplate, $priConfig) {
    if (-not (Test-Path -LiteralPath $required -PathType Leaf)) {
        throw "Missing packaging input: $required"
    }
}
if (-not (Test-Path -LiteralPath $assetSource -PathType Container)) {
    throw "MSIX assets are missing. Run tools\Build-AppIcon.ps1 first."
}

if (-not $Version) {
    # assets\fwdslash.rc is the single place the product version is authored.
    $resourceScript = Get-Content -LiteralPath (Join-Path $repo 'assets\fwdslash.rc') -Raw
    if ($resourceScript -notmatch 'VALUE\s+"FileVersion",\s*"([0-9]+)\.([0-9]+)\.([0-9]+)') {
        throw 'Could not read FileVersion from assets\fwdslash.rc; pass -Version explicitly.'
    }
    $Version = '{0}.{1}.{2}.0' -f $Matches[1], $Matches[2], $Matches[3]
}
if ($Version -notmatch '^\d+\.\d+\.\d+\.0$') {
    throw "MSIX version must be Major.Minor.Build.0 (the Store reserves the revision field): $Version"
}

# Prefer the SDK pinned in packages\ so packaging does not depend on whichever
# kit happens to be installed; fall back to the machine-wide kit.
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
$makepri = Resolve-PackagingTool 'makepri.exe'

function Invoke-Tool {
    param([string]$FilePath, [string[]]$Arguments)

    & $FilePath @Arguments | ForEach-Object { Write-Verbose $_ }
    if ($LASTEXITCODE -ne 0) {
        throw "$(Split-Path -Leaf $FilePath) exited with $LASTEXITCODE."
    }
}

# Everything the product needs at runtime. Deliberately excludes the test
# executables, every build artifact (*.pdb, *.winmd, *.lib, *.exp) and anything
# from driver\, which is production-gated and must never reach the Store.
$payloadFiles = @(
    'fwdslash.exe',
    'fswbroker.exe',
    'fswsettings.exe',
    'Install-CmdAdapter.ps1',
    'Uninstall-CmdAdapter.ps1',
    'Install-PowerShellAdapter.ps1',
    'Uninstall-PowerShellAdapter.ps1'
)
# Inside a package the Windows App SDK is reached through the manifest
# PackageDependency, and -Packaged stops the bootstrap initializer being
# compiled in, so the bootstrap DLL would be dead weight.
$optionalPayloadFiles = @(
    'App.xbf',
    'fswsettings.pri'
)

New-Item -ItemType Directory -Force -Path $outputRoot | Out-Null
$produced = @()

foreach ($target in $Architecture) {
    Write-Host "== $target =="
    if (-not $SkipBuild) {
        & (Join-Path $PSScriptRoot 'Build-UserMode.ps1') -Architecture $target -Configuration $Configuration `
            -Packaged -PackageIdentityName $IdentityName
        if ($LASTEXITCODE -ne 0) { throw "Build-UserMode.ps1 failed for $target." }
    }

    $binaries = Join-Path $repo ('out\user\{0}\{1}' -f $target.ToLowerInvariant(), $Configuration)
    if (-not (Test-Path -LiteralPath $binaries -PathType Container)) {
        throw "Build output does not exist: $binaries"
    }

    $stage = Join-Path $outputRoot ('stage-{0}' -f $target.ToLowerInvariant())
    if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $stage | Out-Null

    foreach ($file in $payloadFiles) {
        $source = Join-Path $binaries $file
        if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
            throw "Required payload file is missing from the build: $source"
        }
        Copy-Item -LiteralPath $source -Destination $stage
    }
    foreach ($file in $optionalPayloadFiles) {
        $source = Join-Path $binaries $file
        if (Test-Path -LiteralPath $source -PathType Leaf) {
            Copy-Item -LiteralPath $source -Destination $stage
        }
    }
    foreach ($directory in 'shell\cmd', 'shell\powershell') {
        $source = Join-Path $binaries $directory
        if (Test-Path -LiteralPath $source -PathType Container) {
            $targetDirectory = Join-Path $stage $directory
            New-Item -ItemType Directory -Force -Path $targetDirectory | Out-Null
            Copy-Item -Path (Join-Path $source '*') -Destination $targetDirectory -Recurse -Force
        }
    }
    Copy-Item -LiteralPath (Join-Path $repo 'LICENSE') -Destination $stage

    # The MSIX logo set, plus the title-bar image the settings app loads through
    # ms-appx:///Assets/.
    $stageAssets = Join-Path $stage 'Assets'
    New-Item -ItemType Directory -Force -Path $stageAssets | Out-Null
    Copy-Item -Path (Join-Path $assetSource '*') -Destination $stageAssets -Recurse -Force
    $titleBar = Join-Path $binaries 'Assets\fwdslash-titlebar.png'
    if (Test-Path -LiteralPath $titleBar -PathType Leaf) {
        Copy-Item -LiteralPath $titleBar -Destination $stageAssets -Force
    }

    $stagedAssets = @(Get-ChildItem -LiteralPath $stageAssets -File).Count
    if ($stagedAssets -lt 40) {
        throw "Only $stagedAssets files staged into Assets; expected the full MSIX logo set."
    }
    foreach ($adapter in 'shell\cmd\fsw-autorun.cmd', 'shell\cmd\fsw-dir.cmd',
                         'shell\powershell\ForwardSlashWindows.psm1') {
        if (-not (Test-Path -LiteralPath (Join-Path $stage $adapter) -PathType Leaf)) {
            throw "Adapter payload missing from the stage: $adapter"
        }
    }

    $manifest = (Get-Content -LiteralPath $manifestTemplate -Raw).
        Replace('{{IDENTITY_NAME}}', $IdentityName).
        Replace('{{PUBLISHER}}', $Publisher).
        Replace('{{PUBLISHER_DISPLAY_NAME}}', $PublisherDisplayName).
        Replace('{{VERSION}}', $Version).
        Replace('{{ARCHITECTURE}}', $target.ToLowerInvariant())
    if ($manifest -match '\{\{') {
        throw 'The manifest template still contains unsubstituted tokens.'
    }
    # UTF-8 without a BOM; makeappx rejects a BOM ahead of the XML declaration.
    [IO.File]::WriteAllText((Join-Path $stage 'AppxManifest.xml'), $manifest,
        (New-Object System.Text.UTF8Encoding($false)))

    Invoke-Tool $makepri @(
        'new',
        '/pr', $stage,
        '/cf', $priConfig,
        '/of', (Join-Path $stage 'resources.pri'),
        '/in', $IdentityName,
        '/o'
    )

    # The settings app resolves ms-appx:///App.xaml and its title-bar image
    # through the package resource map, which must be named after Identity/Name.
    # A silent mismatch here surfaces only as a blank or crashing window.
    $priDump = Join-Path $outputRoot ('resources-{0}.xml' -f $target.ToLowerInvariant())
    Invoke-Tool $makepri @('dump', '/if', (Join-Path $stage 'resources.pri'), '/of', $priDump, '/o')
    $dump = Get-Content -LiteralPath $priDump -Raw
    if ($dump -notmatch [regex]::Escape('name=' + [char]34 + $IdentityName + [char]34)) {
        throw "resources.pri primary map is not named '$IdentityName'; ms-appx lookups would fail."
    }
    foreach ($resource in 'Square44x44Logo.png', 'fwdslash-titlebar.png') {
        if ($dump -notmatch [regex]::Escape($resource)) {
            throw "resources.pri is missing an expected resource: $resource"
        }
    }

    $package = Join-Path $outputRoot ('fwdslash-{0}-{1}.msix' -f $Version, $target.ToLowerInvariant())
    Invoke-Tool $makeappx @('pack', '/d', $stage, '/p', $package, '/o')
    Write-Host "  packed $package"
    $produced += $package
}

$bundle = $null
if ($produced.Count -gt 1) {
    # makeappx bundle takes a directory, so give it one holding only the .msix
    # files just built for this version.
    $bundleInput = Join-Path $outputRoot 'bundle-input'
    if (Test-Path -LiteralPath $bundleInput) { Remove-Item -LiteralPath $bundleInput -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $bundleInput | Out-Null
    foreach ($package in $produced) { Copy-Item -LiteralPath $package -Destination $bundleInput }

    $bundle = Join-Path $outputRoot ('fwdslash-{0}.msixbundle' -f $Version)
    Invoke-Tool $makeappx @('bundle', '/d', $bundleInput, '/p', $bundle, '/o')
    Remove-Item -LiteralPath $bundleInput -Recurse -Force
    Write-Host "  bundled $bundle"
}

$signTarget = if ($bundle) { $bundle } else { $produced[0] }
if ($CertificatePath) {
    $signtool = Resolve-PackagingTool 'signtool.exe'
    $signArguments = @('sign', '/fd', 'SHA256', '/f', $CertificatePath)
    if ($CertificatePassword) { $signArguments += @('/p', $CertificatePassword) }
    $signArguments += $signTarget
    Invoke-Tool $signtool $signArguments
    Write-Host "  signed $signTarget"
} else {
    Write-Host '  unsigned (correct for a Partner Center upload; pass -CertificatePath to install locally)'
}

Write-Host ''
Write-Host "Version:  $Version"
Write-Host "Identity: $IdentityName / $Publisher"
Get-Item -LiteralPath $signTarget | Select-Object Name, @{ Name = 'MB'; Expression = { [Math]::Round($_.Length / 1MB, 2) } } |
    Format-Table -AutoSize | Out-String | Write-Host
