#Requires -Version 5.1
<#
.SYNOPSIS
    Zips a built fswfilter package for the driver lab, optionally test-signing
    it with a lab-only certificate.

.DESCRIPTION
    LAB PACKAGES ONLY. Everything this script produces is for a checkpointed
    Hyper-V guest (tools\New-DriverLabVm.ps1) and must never be loaded on a
    physical workstation. A lab package carries no Microsoft signature, so
    Windows will only load it on a machine with Secure Boot off and test signing
    on - which is exactly what makes such a machine unsafe to keep.

    Reads out\driver\<arch>\<Configuration>\fswfilter.{sys,inf,cat} (produced by
    tools\Build-Driver.ps1, which this script never invokes) and writes
    out\driver\<arch>\fwdslash-filter-<version>-<arch>.zip containing the three
    driver files plus a generated README.txt. With -Lab it first creates (once)
    a self-signed lab code-signing certificate, signs the .sys and .cat with it,
    and includes the .cer in the zip so Bootstrap-DriverLabGuest.ps1 can trust
    it inside the guest.

    Version comes from the root Cargo.toml [workspace.package] version plus a
    ".0" revision, the same source every other 0.0.x copy in the repo tracks.

.PARAMETER Architecture
    x64, ARM64 or All. Defaults to the host architecture.

.PARAMETER Configuration
    Build configuration directory to package. Default Release.

.PARAMETER Lab
    Create (once) and use the lab test certificate, then sign fswfilter.sys and
    fswfilter.cat with it and include the .cer in the zip.

.PARAMETER NoTimestamp
    Skip the RFC 3161 timestamp. A lab signature does not need to outlive the
    certificate, and the machine may have no network.

.PARAMETER Production
    Refused. See the Tier 3 note the script prints.

.EXAMPLE
    .\tools\Package-Driver.ps1 -Architecture ARM64
    .\tools\Package-Driver.ps1 -Architecture All -Lab
#>
[CmdletBinding()]
param(
    [ValidateSet('x64', 'ARM64', 'All')]
    [string]$Architecture,

    [ValidateSet('Debug', 'Release')]
    [string]$Configuration = 'Release',

    [switch]$Lab,

    [switch]$NoTimestamp,

    [switch]$Production
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# $PSScriptRoot is empty while parameter defaults bind under [CmdletBinding()].
$repo = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($Architecture)) {
    if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { $Architecture = 'ARM64' } else { $Architecture = 'x64' }
}

$script:LabCertificateSubject = 'CN=fwdslash lab test'
$script:LabCertificateName = 'fwdslash lab test'
$script:LabStore = 'PrivateCertStore'
$script:TimestampUrl = 'http://timestamp.digicert.com'

if ($Production) {
    Write-Host 'Refused: -Production is not implemented and must not be.'
    Write-Host ''
    Write-Host 'A loadable production driver needs a MICROSOFT signature, which this repo'
    Write-Host 'cannot produce locally. The Azure Trusted Signing kit under signing\ signs'
    Write-Host 'user-mode binaries and the MSIX; it cannot sign kernel code for load.'
    Write-Host ''
    Write-Host 'That path is Tier 3 of the driver plan and is deferred:'
    Write-Host '  1. Request a production filter altitude from Microsoft (the INF ships'
    Write-Host '     371120 as a placeholder).'
    Write-Host '  2. Register a Partner Center Hardware Program account against the'
    Write-Host '     Trusted Signing identity.'
    Write-Host '  3. Submit the driver .cab for ATTESTATION signing (Windows 10/11 client,'
    Write-Host '     x64 + ARM64) and ship the bytes Microsoft returns - never a rebuild.'
    Write-Host ''
    Write-Host 'See docs\driver-lab.md, "Deferred: Tier 3".'
    exit 2
}

function Get-WorkspaceVersion {
    $cargo = Join-Path $repo 'Cargo.toml'
    if (-not (Test-Path -LiteralPath $cargo)) {
        throw 'The root Cargo.toml was not found; the package version has no source.'
    }
    $inWorkspacePackage = $false
    foreach ($line in (Get-Content -LiteralPath $cargo)) {
        $trimmed = $line.Trim()
        if ($trimmed -match '^\[(.+)\]$') {
            $inWorkspacePackage = ($Matches[1] -eq 'workspace.package')
            continue
        }
        if ($inWorkspacePackage -and $trimmed -match '^version\s*=\s*"([^"]+)"') {
            return $Matches[1]
        }
    }
    throw 'No [workspace.package] version was found in the root Cargo.toml.'
}

function Get-KitTool {
    param([Parameter(Mandatory = $true)][string]$ToolName)

    $hostArchitecture = 'x86'
    if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { $hostArchitecture = 'arm64' }
    elseif ($env:PROCESSOR_ARCHITECTURE -eq 'AMD64') { $hostArchitecture = 'x64' }
    $preferences = @($hostArchitecture, 'x64', 'x86') | Select-Object -Unique

    $roots = @()
    $kitRoot = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
    if (Test-Path -LiteralPath $kitRoot) {
        $versioned = Get-ChildItem -LiteralPath $kitRoot -Directory -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -match '^\d+\.\d+\.\d+\.\d+$' } |
            Sort-Object { [Version]$_.Name } -Descending
        foreach ($directory in $versioned) { $roots += $directory.FullName }
        $roots += $kitRoot
    }
    # The repo-local SDK package is the fallback, so a machine without a full
    # SDK install can still produce a lab package.
    $packages = Join-Path $repo 'packages'
    if (Test-Path -LiteralPath $packages) {
        $localBins = Get-ChildItem -LiteralPath $packages -Directory -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -like 'Microsoft.Windows.SDK.CPP.*' } |
            ForEach-Object { Join-Path $_.FullName 'c\bin' } |
            Where-Object { Test-Path -LiteralPath $_ }
        foreach ($bin in $localBins) {
            $versioned = Get-ChildItem -LiteralPath $bin -Directory -ErrorAction SilentlyContinue |
                Sort-Object Name -Descending
            foreach ($directory in $versioned) { $roots += $directory.FullName }
        }
    }

    foreach ($root in $roots) {
        foreach ($preference in $preferences) {
            $candidate = Join-Path (Join-Path $root $preference) $ToolName
            if (Test-Path -LiteralPath $candidate) { return $candidate }
        }
    }
    return $null
}

function Invoke-Checked {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [Parameter(Mandatory = $true)][string[]]$Arguments
    )
    & $FilePath @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$(Split-Path -Leaf $FilePath) exited with $LASTEXITCODE."
    }
}

function New-LabCertificate {
    param([Parameter(Mandatory = $true)][string]$CertificatePath)

    if (Test-Path -LiteralPath $CertificatePath) {
        Write-Host "  lab certificate: reusing the existing $(Split-Path -Leaf $CertificatePath)"
        return
    }
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $CertificatePath) | Out-Null

    $makecert = Get-KitTool -ToolName 'makecert.exe'
    if ($null -ne $makecert) {
        # -eku 1.3.6.1.5.5.7.3.3 is Code Signing; a kernel catalog will not be
        # accepted from a certificate without it.
        Invoke-Checked -FilePath $makecert -Arguments @(
            '-r', '-pe',
            '-ss', $script:LabStore,
            '-n', $script:LabCertificateSubject,
            '-eku', '1.3.6.1.5.5.7.3.3',
            $CertificatePath
        )
        Write-Host '  lab certificate: created with makecert into the PrivateCertStore store'
        return
    }

    # makecert.exe was removed from newer SDK layouts; New-SelfSignedCertificate
    # produces an equivalent certificate. It lands in CurrentUser\My, so it is
    # copied into PrivateCertStore to keep the signtool arguments identical.
    Write-Warning 'makecert.exe was not found; falling back to New-SelfSignedCertificate.'
    $certificate = New-SelfSignedCertificate `
        -Subject $script:LabCertificateSubject `
        -Type CodeSigningCert `
        -KeyUsage DigitalSignature `
        -TextExtension @('2.5.29.37={text}1.3.6.1.5.5.7.3.3') `
        -CertStoreLocation 'Cert:\CurrentUser\My' `
        -NotAfter (Get-Date).AddYears(2)
    $storePath = "Cert:\CurrentUser\$($script:LabStore)"
    if (-not (Test-Path -LiteralPath $storePath)) {
        $store = New-Object Security.Cryptography.X509Certificates.X509Store($script:LabStore, 'CurrentUser')
        $store.Open('ReadWrite')
        $store.Close()
    }
    $store = New-Object Security.Cryptography.X509Certificates.X509Store($script:LabStore, 'CurrentUser')
    $store.Open('ReadWrite')
    $store.Add($certificate)
    $store.Close()
    Export-Certificate -Cert $certificate -FilePath $CertificatePath -Type CERT | Out-Null
    Write-Host '  lab certificate: created with New-SelfSignedCertificate'
}

function New-ReadmeText {
    param(
        [Parameter(Mandatory = $true)][string]$Version,
        [Parameter(Mandatory = $true)][string]$Target,
        [Parameter(Mandatory = $true)][bool]$Signed
    )

    $signatureLine = if ($Signed) {
        'These files are signed with a LAB-ONLY self-signed certificate (CN=fwdslash lab test).'
    } else {
        'These files are UNSIGNED. Windows will refuse to load them until they are signed.'
    }

    return @"
fwdslash filesystem filter (fswfilter) $Version $Target
=========================================================

WHAT THIS IS
    A Filter Manager minifilter that reparses drive-root paths shaped like
    C:\<Distribution>\path to \\wsl.localhost\<Distribution>\path, so every
    Win32 file API - not just the four shell surfaces the fwdslash broker
    rewrites - reaches a WSL distribution through a Windows-looking path.
    The mapping is published by the fwdslash broker over the Filter Manager
    port \FswFilterPort and is scoped to the publishing user's SID and session.
    With no broker connected the filter rewrites nothing.

LAB ONLY - READ THIS BEFORE INSTALLING
    $signatureLine
    There is no Microsoft signature, so this package loads only on a machine
    with Secure Boot OFF and test signing ON. A machine in that state is not
    safe to use for anything else.

    Install it ONLY in a checkpointed virtual machine you can throw away. Do not
    install it on a physical workstation. Kernel code that misbehaves takes the
    whole machine with it, and this driver has not passed the release gate in
    docs/compatibility.md.

    The fwdslash Store package and the fwdslash GitHub release contain no
    driver. Nothing in the shipping product installs this.

INSTALL (elevated, in the lab guest, after a reboot with test signing on)
    pnputil /add-driver fswfilter.inf /install
    fltmc load FswFilter
    fltmc filters                     (FswFilter, altitude 371120)
    fltmc instances -f FswFilter      (disk volumes only)

UNINSTALL
    fltmc unload FswFilter
    pnputil /enum-drivers              (find the oemNN.inf published name)
    pnputil /delete-driver oemNN.inf /uninstall /force

    Restoring the guest's clean checkpoint is faster and more complete.

ALTITUDE
    371120 is a placeholder in the FSFilter Activity Monitor range. A production
    altitude has to be allocated by Microsoft.

CONTENTS
    fswfilter.sys     the minifilter
    fswfilter.inf     DefaultInstall INF, service FswFilter, start type DEMAND
    fswfilter.cat     catalog
    fwdslash-lab.cer  lab certificate (present only in a -Lab package); import
                      into LocalMachine\Root and LocalMachine\TrustedPublisher
                      inside the guest, which
                      tools\Bootstrap-DriverLabGuest.ps1 does for you

MORE
    docs/driver-lab.md      operator runbook for the whole lab cycle
    docs/compatibility.md   the release gate this package has not passed yet
    SECURITY.md             reporting and driver status
"@
}

# ------------------------------------------------------------------ main loop
$version = "$(Get-WorkspaceVersion).0"
$targets = if ($Architecture -eq 'All') { @('x64', 'ARM64') } else { @($Architecture) }

$signtool = $null
$labCertificate = $null
if ($Lab) {
    $signtool = Get-KitTool -ToolName 'signtool.exe'
    if ($null -eq $signtool) {
        throw 'signtool.exe was not found in the Windows Kits or the repo-local SDK package; -Lab cannot sign.'
    }
    $labCertificate = Join-Path $repo 'out\driver\lab\fwdslash-lab.cer'
    Write-Host 'Lab signing requested.'
    New-LabCertificate -CertificatePath $labCertificate
}

$produced = @()
foreach ($target in $targets) {
    $outputName = if ($target -eq 'ARM64') { 'arm64' } else { 'x64' }
    Write-Host ''
    Write-Host "Packaging $target ($Configuration)"

    # Build-Driver.ps1 writes to out\driver\<arch>\<config> on the
    # command-line WDK path and to driver\fswfilter\<Arch>\<config> when the
    # Visual Studio WDK project component is registered. Both are accepted.
    $buildDirectory = $null
    foreach ($candidate in @(
            (Join-Path $repo "out\driver\$outputName\$Configuration"),
            (Join-Path $repo "driver\fswfilter\$target\$Configuration"))) {
        if (Test-Path -LiteralPath (Join-Path $candidate 'fswfilter.sys')) {
            $buildDirectory = $candidate
            break
        }
    }
    if ($null -eq $buildDirectory) {
        throw "No build output for $target. Run tools\Build-Driver.ps1 -Architecture $target -Configuration $Configuration first."
    }
    Write-Host "  source: $buildDirectory"
    $payload = @('fswfilter.sys', 'fswfilter.inf', 'fswfilter.cat')
    foreach ($file in $payload) {
        if (-not (Test-Path -LiteralPath (Join-Path $buildDirectory $file))) {
            throw "$file is missing from the $target build output. Run tools\Build-Driver.ps1 -Architecture $target -Configuration $Configuration first."
        }
    }

    $staging = Join-Path $repo "out\driver\$outputName\package"
    if (Test-Path -LiteralPath $staging) {
        Remove-Item -LiteralPath $staging -Recurse -Force
    }
    New-Item -ItemType Directory -Force -Path $staging | Out-Null
    foreach ($file in $payload) {
        Copy-Item -LiteralPath (Join-Path $buildDirectory $file) -Destination $staging -Force
    }

    if ($Lab) {
        $signArguments = @('sign', '/fd', 'sha256', '/s', $script:LabStore, '/n', $script:LabCertificateName)
        if (-not $NoTimestamp) {
            $signArguments += @('/t', $script:TimestampUrl)
        }
        # The .sys is signed first; the catalog covers the signed bytes.
        foreach ($file in @('fswfilter.sys', 'fswfilter.cat')) {
            Invoke-Checked -FilePath $signtool -Arguments ($signArguments + @((Join-Path $staging $file)))
        }
        Copy-Item -LiteralPath $labCertificate -Destination $staging -Force
        Write-Host '  signed fswfilter.sys and fswfilter.cat with the lab certificate'
    } else {
        Write-Host '  unsigned (pass -Lab to test-sign for the guest)'
    }

    $readme = New-ReadmeText -Version $version -Target $target -Signed ([bool]$Lab)
    Set-Content -LiteralPath (Join-Path $staging 'README.txt') -Value $readme -Encoding ascii

    $zip = Join-Path $repo "out\driver\$outputName\fwdslash-filter-$version-$outputName.zip"
    if (Test-Path -LiteralPath $zip) {
        Remove-Item -LiteralPath $zip -Force
    }
    # -LiteralPath does not expand wildcards, so the staging children are
    # enumerated instead of passing "<staging>\*".
    $items = Get-ChildItem -LiteralPath $staging -File | ForEach-Object { $_.FullName }
    Compress-Archive -LiteralPath $items -DestinationPath $zip -CompressionLevel Optimal
    Remove-Item -LiteralPath $staging -Recurse -Force

    $size = [math]::Round((Get-Item -LiteralPath $zip).Length / 1KB, 1)
    Write-Host "  wrote $(Split-Path -Leaf $zip) ($size KB)"
    $produced += $zip
}

Write-Host ''
Write-Host 'Lab packages produced:'
foreach ($zip in $produced) {
    Write-Host "  $zip"
}
Write-Host ''
Write-Host 'Copy one into the lab guest and install it there only:'
Write-Host '  Copy-VMFile -Name <vm> -SourcePath <zip> -DestinationPath C:\FswLab\fwdslash-filter.zip -CreateFullPath -FileSource Host'
Write-Host 'Never load it on a physical machine. See docs\driver-lab.md.'
exit 0
