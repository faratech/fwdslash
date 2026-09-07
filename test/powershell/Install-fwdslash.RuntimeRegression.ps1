# Runtime-architecture regression fixture for #79, refreshed for the hardened
# installer (#125). Run from native Windows PowerShell without a profile:
#   C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe -NoProfile -File .\test\powershell\Install-fwdslash.RuntimeRegression.ps1
#
# Nothing here reaches the network or the package manager: Invoke-RestMethod,
# Invoke-WebRequest, Start-Process, Add-AppxPackage, Get-AppxPackage and
# Get-AuthenticodeSignature are all replaced in the caller scope, which the
# installer's own child script scope inherits. The two files the installer
# verifies are synthesized on disk instead of mocked away, because the
# installer opens, hashes and parses them for real:
#   * the bundle is a genuine zip holding an AppxBundleManifest.xml, so
#     Get-FileHash, Get-BundleIdentity and Assert-BundlePayload all run;
#   * the runtime installer is a stub whose PE header carries the machine word
#     Get-PeArchitecture reads.
#
# The stub PE has no version resource, so [Diagnostics.FileVersionInfo] -- a
# static call no scope can intercept -- makes the installer reject it after the
# architecture check. That is deliberate: the fixture asserts which runtime URL
# was chosen and that the rejection is the friendly guidance path, and covers
# the bundle install separately with the runtime already registered.

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$expectedVersion = '0.0.0'
$expectedTag = "v$expectedVersion"
$expectedAsset = "fwdslash-$expectedVersion.0.msixbundle"
$githubPublisher = 'CN=Mike Fara, O=Mike Fara, L=White Plains, S=ny, C=US'

function Assert-True {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw $Message }
}

function New-FixtureBundle {
    # A real zip: the installer reads AppxMetadata/AppxBundleManifest.xml out
    # of it with System.IO.Compression.
    param([string]$Path, [string]$PackageVersion, [switch]$WithResourcePackage)

    $manifest = @"
<?xml version="1.0" encoding="utf-8"?>
<Bundle xmlns="http://schemas.microsoft.com/appx/2013/bundle">
  <Identity Name="32827MikeFara.fwdslash" Publisher="$githubPublisher" Version="$PackageVersion" />
  <Packages>
    <Package Type="application" Version="$PackageVersion" Architecture="x64" FileName="x64.msix" />
    <Package Type="application" Version="$PackageVersion" Architecture="arm64" FileName="arm64.msix" />
"@
    if ($WithResourcePackage) {
        # Resource packages carry no architecture and a version of their own;
        # the payload assert must ignore them (#125).
        $manifest += @"
    <Package Type="resource" Version="0.0.0.0" ResourceId="language-en" FileName="en.msix" />
"@
    }
    $manifest += @"
  </Packages>
</Bundle>
"@

    Add-Type -AssemblyName System.IO.Compression.FileSystem | Out-Null
    Add-Type -AssemblyName System.IO.Compression | Out-Null
    $stream = [IO.File]::Open($Path, [IO.FileMode]::Create, [IO.FileAccess]::Write, [IO.FileShare]::None)
    try {
        $archive = [IO.Compression.ZipArchive]::new($stream, [IO.Compression.ZipArchiveMode]::Create)
        try {
            $entry = $archive.CreateEntry('AppxMetadata/AppxBundleManifest.xml')
            $writer = [IO.StreamWriter]::new($entry.Open())
            try { $writer.Write($manifest) } finally { $writer.Dispose() }
        } finally { $archive.Dispose() }
    } finally { $stream.Dispose() }
}

function New-FixturePeStub {
    # Just enough of a PE for Get-PeArchitecture: e_lfanew at 0x3c, then the
    # signature and the machine word.
    param([string]$Path, [string]$Architecture)

    $machine = switch ($Architecture) {
        'x64' { 0x8664 }
        'arm64' { 0xAA64 }
        default { throw "unsupported fixture architecture $Architecture" }
    }
    $bytes = New-Object byte[] 512
    $bytes[0] = 0x4D; $bytes[1] = 0x5A
    [BitConverter]::GetBytes([int]0x80).CopyTo($bytes, 0x3c)
    [Text.Encoding]::ASCII.GetBytes('PE').CopyTo($bytes, 0x80)
    [BitConverter]::GetBytes([uint16]$machine).CopyTo($bytes, 0x84)
    [IO.File]::WriteAllBytes($Path, $bytes)
}

function Invoke-InstallerFixture {
    param(
        [string]$ProcessArchitecture,
        [AllowEmptyString()][string]$Wow64Architecture,
        [object[]]$RuntimePackages = @(),
        [switch]$OmitDigest,
        [switch]$WithResourcePackage,
        [string]$RequestedVersion = $expectedVersion
    )

    $installer = Join-Path $PSScriptRoot '../../tools/Install-fwdslash.ps1'
    $temporaryRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("fsw-runtime-{0}" -f [guid]::NewGuid())
    $previousTemp = $env:TEMP
    $previousTmp = $env:TMP
    $previousArchitecture = $env:PROCESSOR_ARCHITECTURE
    $previousWow64 = $env:PROCESSOR_ARCHITEW6432
    $calls = New-Object System.Collections.Generic.List[string]
    try {
        New-Item -ItemType Directory -Path $temporaryRoot | Out-Null
        # The bundle is built once, outside the staging directory the installer
        # creates and deletes, so its hash can be published in the mock release.
        $sourceBundle = Join-Path $temporaryRoot 'source.msixbundle'
        New-FixtureBundle -Path $sourceBundle -PackageVersion "$expectedVersion.0" -WithResourcePackage:$WithResourcePackage
        $bundleHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $sourceBundle).Hash

        $env:TEMP = $temporaryRoot
        $env:TMP = $temporaryRoot
        $env:PROCESSOR_ARCHITECTURE = $ProcessArchitecture
        $env:PROCESSOR_ARCHITEW6432 = $Wow64Architecture

        function Get-AppxPackage {
            [CmdletBinding()]
            param([string]$Name)
            if ($Name -eq '32827MikeFara.fwdslash') { return }
            if ($Name -eq 'Microsoft.WindowsAppRuntime.2') { return $RuntimePackages }
            return
        }
        function Invoke-RestMethod {
            [CmdletBinding()]
            param([string]$Uri)
            $calls.Add("release:$Uri")
            $asset = [ordered]@{
                name = $expectedAsset
                browser_download_url = "https://github.com/faratech/fwdslash/releases/download/$expectedTag/$expectedAsset"
            }
            if (-not $OmitDigest) { $asset['digest'] = "sha256:$bundleHash" }
            return [pscustomobject]@{
                tag_name = $expectedTag
                assets = @([pscustomobject]$asset)
            }
        }
        function Invoke-WebRequest {
            [CmdletBinding()]
            param([string]$Uri, [string]$OutFile)
            $calls.Add("download:$Uri")
            if ($Uri -like '*windowsappruntimeinstall-*') {
                $architecture = if ($Uri -like '*-arm64.exe') { 'arm64' } else { 'x64' }
                New-FixturePeStub -Path $OutFile -Architecture $architecture
            } else {
                Copy-Item -LiteralPath $sourceBundle -Destination $OutFile -Force
            }
        }
        function Get-AuthenticodeSignature {
            [CmdletBinding()]
            param([string]$LiteralPath)
            $subject = if ($LiteralPath -like '*msixbundle') {
                $githubPublisher
            } else {
                'CN=Microsoft Corporation, O=Microsoft Corporation, L=Redmond, S=Washington, C=US'
            }
            return [pscustomobject]@{
                Status = 'Valid'
                SignerCertificate = [pscustomobject]@{ Subject = $subject }
            }
        }
        function Start-Process {
            [CmdletBinding()]
            param([string]$FilePath, [string]$ArgumentList, [switch]$Wait, [switch]$PassThru)
            $calls.Add("start:$FilePath")
            return [pscustomobject]@{ ExitCode = 0 }
        }
        function Add-AppxPackage {
            [CmdletBinding()]
            param([string]$Path)
            $calls.Add("install:$Path")
        }

        # *>&1 and not 2>&1: the guidance path reports through Write-Host, which
        # is the information stream in PowerShell 5.0 and later.
        $output = & $installer -Version $RequestedVersion *>&1
        foreach ($record in @($output)) { $calls.Add("output:$record") }
        return @($calls)
    } finally {
        $env:TEMP = $previousTemp
        $env:TMP = $previousTmp
        $env:PROCESSOR_ARCHITECTURE = $previousArchitecture
        $env:PROCESSOR_ARCHITEW6432 = $previousWow64
        Remove-Item -LiteralPath $temporaryRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}

$satisfiedRuntime = @([pscustomobject]@{
    Name = 'Microsoft.WindowsAppRuntime.2'; IsFramework = $true; Architecture = 'ARM64'; Version = [version]'2.5.0.0'
})
$satisfiedRuntimeX64 = @([pscustomobject]@{
    Name = 'Microsoft.WindowsAppRuntime.2'; IsFramework = $true; Architecture = 'X64'; Version = [version]'2.5.0.0'
})

function Assert-RuntimeDownload {
    # The stub runtime installer has no version resource, so the installer
    # rejects it after the architecture check and takes the guidance path.
    param([object[]]$Calls, [string]$Architecture, [string]$Message)
    Assert-True -Condition (@($Calls | Where-Object { $_ -like "download:*windowsappruntimeinstall-$Architecture.exe" }).Count -eq 1) -Message $Message
    Assert-True -Condition (@($Calls | Where-Object { $_ -like '*The Windows App Runtime 2.x could not be installed automatically*' }).Count -eq 1) `
        -Message "$Message must fall back to the manual-install guidance for the stub installer"
}

function Assert-BundleInstalled {
    param([object[]]$Calls, [string]$Message)
    Assert-True -Condition (@($Calls | Where-Object { $_ -like "install:*$expectedAsset" }).Count -eq 1) -Message $Message
    Assert-True -Condition (@($Calls | Where-Object { $_ -like 'download:*windowsappruntimeinstall-*' }).Count -eq 0) `
        -Message "$Message must not download a runtime it already has"
}

# --- runtime architecture selection -----------------------------------------

Assert-RuntimeDownload -Calls (Invoke-InstallerFixture -ProcessArchitecture 'ARM64' -Wow64Architecture '') `
    -Architecture 'arm64' -Message 'native ARM64 must select the ARM64 runtime'
Assert-RuntimeDownload -Calls (Invoke-InstallerFixture -ProcessArchitecture 'AMD64' -Wow64Architecture 'ARM64') `
    -Architecture 'arm64' -Message 'emulated x64 PowerShell on ARM64 must select the ARM64 runtime'
Assert-RuntimeDownload -Calls (Invoke-InstallerFixture -ProcessArchitecture 'AMD64' -Wow64Architecture '') `
    -Architecture 'x64' -Message 'native x64 must select the x64 runtime'

$wrongArchitecture = [pscustomobject]@{
    Name = 'Microsoft.WindowsAppRuntime.2'; IsFramework = $true; Architecture = 'X64'; Version = [version]'2.5.0.0'
}
Assert-RuntimeDownload -Calls (Invoke-InstallerFixture -ProcessArchitecture 'ARM64' -Wow64Architecture '' -RuntimePackages @($wrongArchitecture)) `
    -Architecture 'arm64' -Message 'an x64 framework registration cannot satisfy ARM64'

$wrongName = [pscustomobject]@{
    Name = 'Microsoft.WindowsAppRuntime.2Preview'; IsFramework = $true; Architecture = 'ARM64'; Version = [version]'2.5.0.0'
}
Assert-RuntimeDownload -Calls (Invoke-InstallerFixture -ProcessArchitecture 'ARM64' -Wow64Architecture '' -RuntimePackages @($wrongName)) `
    -Architecture 'arm64' -Message 'a similarly named runtime cannot satisfy the exact framework dependency'

$unsupportedRejected = $false
try {
    Invoke-InstallerFixture -ProcessArchitecture 'x86' -Wow64Architecture '' | Out-Null
} catch {
    $unsupportedRejected = $_.Exception.Message -like '*supports only x64 and ARM64*'
}
Assert-True -Condition $unsupportedRejected -Message 'unsupported architectures must be rejected before installation work starts'

# --- bundle verification and install (#125) ---------------------------------

Assert-BundleInstalled -Calls (Invoke-InstallerFixture -ProcessArchitecture 'ARM64' -Wow64Architecture '' -RuntimePackages $satisfiedRuntime) `
    -Message 'a bare three-part -Version must install the release bundle'
Assert-BundleInstalled -Calls (Invoke-InstallerFixture -ProcessArchitecture 'ARM64' -Wow64Architecture '' -RuntimePackages $satisfiedRuntime -RequestedVersion $expectedTag) `
    -Message 'a v-prefixed -Version must install the same release bundle'
Assert-BundleInstalled -Calls (Invoke-InstallerFixture -ProcessArchitecture 'AMD64' -Wow64Architecture '' -RuntimePackages $satisfiedRuntimeX64 -OmitDigest) `
    -Message 'a release without an asset digest must still install'
Assert-BundleInstalled -Calls (Invoke-InstallerFixture -ProcessArchitecture 'AMD64' -Wow64Architecture '' -RuntimePackages $satisfiedRuntimeX64 -WithResourcePackage) `
    -Message 'a resource package in the bundle must not fail the payload assert'

# The v-prefixed spelling must reach exactly the same release endpoint as the
# bare one -- never https://.../tags/vv0.0.0.
$bareCalls = Invoke-InstallerFixture -ProcessArchitecture 'ARM64' -Wow64Architecture '' -RuntimePackages $satisfiedRuntime
$prefixedCalls = Invoke-InstallerFixture -ProcessArchitecture 'ARM64' -Wow64Architecture '' -RuntimePackages $satisfiedRuntime -RequestedVersion $expectedTag
$bareRelease = @($bareCalls | Where-Object { $_ -like 'release:*' })
$prefixedRelease = @($prefixedCalls | Where-Object { $_ -like 'release:*' })
Assert-True -Condition ($bareRelease.Count -eq 1 -and $prefixedRelease.Count -eq 1 -and $bareRelease[0] -eq $prefixedRelease[0]) `
    -Message 'both -Version spellings must resolve to the same release URL'
Assert-True -Condition ($bareRelease[0] -like "*/releases/tags/$expectedTag") `
    -Message 'the normalized release URL must carry exactly one v prefix'

$malformedVersionRejected = $false
try {
    Invoke-InstallerFixture -ProcessArchitecture 'ARM64' -Wow64Architecture '' -RequestedVersion 'not-a-version' | Out-Null
} catch {
    $malformedVersionRejected = $_.Exception.Message -like '*Version must be latest or a release tag*'
}
Assert-True -Condition $malformedVersionRejected -Message 'a non-tag -Version must still be rejected'
