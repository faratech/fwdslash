# Installs Forward Slash Windows from the latest GitHub release: downloads the
# signed MSIX bundle and registers it for the current user. No administrator
# rights needed — the package is signed with a Public Trust certificate, so
# Windows verifies it without importing anything.
#
#   powershell -ExecutionPolicy Bypass -File Install-fwdslash.ps1
#
[CmdletBinding()]
param(
    # Release tag to install; 'latest' picks the newest non-prerelease.
    [string]$Version = 'latest',

    # Install the GitHub build even when the Microsoft Store version is
    # already present.
    #
    # The two packages install side by side, but they are NOT independent:
    # both register the same windows.startupTask and the same 'fwdslash'
    # appExecutionAlias, and both brokers take the single-instance mutex
    # Local\ForwardSlashWindows.Broker. At logon Windows starts both startup
    # tasks and only one broker survives the race -- which one is not
    # predictable, and the alias and the fwdslash:// protocol likewise route to
    # whichever package registered last. Keep one flavor unless you are
    # deliberately testing both.
    [switch]$Force
)

$ErrorActionPreference = 'Stop'
$repo = 'faratech/fwdslash'
$storePublisher = 'CN=ABDB6B3F-DF9E-447D-BC0E-4DA7BAFD14C4'
$githubPublisher = 'CN=Mike Fara, O=Mike Fara, L=White Plains, S=ny, C=US'
$expectedIdentity = '32827MikeFara.fwdslash'

# The Windows App Runtime 2.x redistributable the manifest depends on
# (Microsoft.WindowsAppRuntime.2, MinVersion 2.0.0.0). Microsoft rewrites the
# aka.ms redirector target with each servicing release, so RE-VERIFY THIS URL
# whenever the Windows App SDK dependency in packaging\AppxManifest.xml moves:
# it is a redirector, and a wrong channel installs a runtime the package still
# refuses to run against.
$runtimeInstallerUrl = 'https://aka.ms/windowsappsdk/2.0/latest/windowsappruntimeinstall-{0}.exe'
$runtimeFrameworkName = 'Microsoft.WindowsAppRuntime.2'
$runtimeMinimumVersion = [version]'2.0.0.0'

# Paths are a security boundary here: this script downloads executable content and
# then launches it.  PowerShell's normal file APIs follow junctions and symlinks,
# so use Win32 handles opened with FILE_FLAG_OPEN_REPARSE_POINT to reject them and
# deny DELETE sharing while the verified object is used.
if (-not ('ForwardSlashWindows.NativeFile' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;

namespace ForwardSlashWindows {
    public static class NativeFile {
        private const uint GENERIC_READ = 0x80000000;
        private const uint OPEN_EXISTING = 3;
        private const uint FILE_ATTRIBUTE_REPARSE_POINT = 0x400;
        private const uint FILE_FLAG_BACKUP_SEMANTICS = 0x02000000;
        private const uint FILE_FLAG_OPEN_REPARSE_POINT = 0x00200000;
        private const uint FILE_SHARE_READ = 1;
        private const uint FILE_SHARE_WRITE = 2;

        [StructLayout(LayoutKind.Sequential)]
        private struct BY_HANDLE_FILE_INFORMATION {
            public uint FileAttributes;
            public System.Runtime.InteropServices.ComTypes.FILETIME CreationTime;
            public System.Runtime.InteropServices.ComTypes.FILETIME LastAccessTime;
            public System.Runtime.InteropServices.ComTypes.FILETIME LastWriteTime;
            public uint VolumeSerialNumber;
            public uint FileSizeHigh;
            public uint FileSizeLow;
            public uint NumberOfLinks;
            public uint FileIndexHigh;
            public uint FileIndexLow;
        }

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        private static extern SafeFileHandle CreateFile(
            string name, uint desiredAccess, uint shareMode, IntPtr securityAttributes,
            uint creationDisposition, uint flagsAndAttributes, IntPtr templateFile);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool GetFileInformationByHandle(
            SafeFileHandle handle, out BY_HANDLE_FILE_INFORMATION information);

        // Directories permit ordinary child writes, but deny deletion/rename of
        // the directory itself.  Files additionally deny writes after download.
        public static SafeFileHandle OpenLockedNoFollow(string path, bool directory) {
            uint shareMode = directory ? FILE_SHARE_READ | FILE_SHARE_WRITE : FILE_SHARE_READ;
            uint flags = FILE_FLAG_OPEN_REPARSE_POINT;
            if (directory) flags |= FILE_FLAG_BACKUP_SEMANTICS;
            SafeFileHandle handle = CreateFile(
                path, GENERIC_READ, shareMode, IntPtr.Zero, OPEN_EXISTING, flags, IntPtr.Zero);
            if (handle.IsInvalid) throw new Win32Exception(Marshal.GetLastWin32Error(), "Cannot open " + path);
            BY_HANDLE_FILE_INFORMATION info;
            if (!GetFileInformationByHandle(handle, out info)) {
                int error = Marshal.GetLastWin32Error();
                handle.Dispose();
                throw new Win32Exception(error, "Cannot inspect " + path);
            }
            if ((info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT) != 0) {
                handle.Dispose();
                throw new InvalidOperationException("Refusing reparse-point path: " + path);
            }
            return handle;
        }
    }
}
'@
}

function Get-ForwardSlashWindowsNativeArchitecture {
    # Under WOW64, PROCESSOR_ARCHITECTURE describes the emulated PowerShell
    # process while PROCESSOR_ARCHITEW6432 describes the native OS. This works
    # in Windows PowerShell 5.1 without Core-only RuntimeInformation APIs.
    $architecture = $env:PROCESSOR_ARCHITEW6432
    if ([string]::IsNullOrWhiteSpace($architecture)) {
        $architecture = $env:PROCESSOR_ARCHITECTURE
    }
    if ([string]::IsNullOrWhiteSpace($architecture)) {
        throw 'Forward Slash Windows supports only x64 and ARM64 Windows (no native architecture was reported).'
    }
    switch ($architecture.ToUpperInvariant()) {
        'AMD64' { return 'x64' }
        'X64' { return 'x64' }
        'ARM64' { return 'arm64' }
        default { throw "Forward Slash Windows supports only x64 and ARM64 Windows (detected '$architecture')." }
    }
}

function Test-ForwardSlashWindowsRuntime {
    param([Parameter(Mandatory = $true)][string]$Architecture)

    foreach ($package in @(Get-AppxPackage -Name $runtimeFrameworkName -ErrorAction SilentlyContinue)) {
        if ($null -eq $package -or
            $package.Name -ne $runtimeFrameworkName -or
            -not $package.IsFramework -or
            ([string]$package.Architecture) -ine $Architecture) {
            continue
        }
        try {
            if ([version]$package.Version -ge $runtimeMinimumVersion) {
                return $true
            }
        } catch {
            # A malformed registration cannot satisfy a package dependency.
        }
    }
    return $false
}

$runtimeArchitecture = Get-ForwardSlashWindowsNativeArchitecture

function Close-ForwardSlashWindowsHandles([object[]]$Handles) {
    foreach ($handle in @($Handles | Where-Object { $null -ne $_ })) {
        $handle.Dispose()
    }
}

function Open-ForwardSlashWindowsNoFollowPath([string]$Path, [switch]$Directory) {
    return [ForwardSlashWindows.NativeFile]::OpenLockedNoFollow(
        [IO.Path]::GetFullPath($Path), $Directory.IsPresent)
}

function Lock-ForwardSlashWindowsDirectoryChain([string]$Path) {
    $fullPath = [IO.Path]::GetFullPath($Path)
    $root = [IO.Path]::GetPathRoot($fullPath)
    if ([string]::IsNullOrWhiteSpace($root)) { throw "The staging path has no filesystem root: $Path" }

    $handles = @()
    try {
        # Holding a no-delete handle for every ancestor prevents a verified
        # installer pathname from being redirected by a junction replacement
        # between validation and CreateProcess.
        $handles += Open-ForwardSlashWindowsNoFollowPath -Path $root -Directory
        $current = $root
        $relative = $fullPath.Substring($root.Length)
        foreach ($part in @($relative -split '[\\/]' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })) {
            $current = Join-Path $current $part
            $handles += Open-ForwardSlashWindowsNoFollowPath -Path $current -Directory
        }
        return $handles
    } catch {
        Close-ForwardSlashWindowsHandles $handles
        throw
    }
}

function New-PrivateDownloadDirectory {
    # A fresh directory under the per-user temp root avoids touching the
    # product's normal LocalAppData state.  Its explicit, protected DACL keeps
    # lower-integrity processes from staging a replacement while the handle
    # chain below protects the object namespace itself.
    $directory = Join-Path ([IO.Path]::GetTempPath()) ('fsw-install-' + [guid]::NewGuid().ToString('N'))
    $item = New-Item -ItemType Directory -Path $directory -ErrorAction Stop
    $directoryHandle = Open-ForwardSlashWindowsNoFollowPath -Path $item.FullName -Directory
    try {
        $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User
        $acl = New-Object Security.AccessControl.DirectorySecurity
        $acl.SetAccessRuleProtection($true, $false)
        $inheritance = [Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit'
        $acl.AddAccessRule((New-Object Security.AccessControl.FileSystemAccessRule(
            $sid, 'FullControl', $inheritance,
            [Security.AccessControl.PropagationFlags]::None,
            [Security.AccessControl.AccessControlType]::Allow)))
        Set-Acl -LiteralPath $item.FullName -AclObject $acl
        # Acquire the complete chain before releasing the initial leaf handle,
        # leaving no replacement window between DACL setup and path locking.
        return [pscustomobject]@{
            Path = $item.FullName
            Locks = @(Lock-ForwardSlashWindowsDirectoryChain $item.FullName)
        }
    } finally {
        $directoryHandle.Dispose()
    }
}

function Assert-TrustedSignature([string]$Path, [string]$ExpectedSubject, [string]$Label) {
    $signature = Get-AuthenticodeSignature -LiteralPath $Path
    if ($signature.Status -ne 'Valid' -or -not $signature.SignerCertificate) {
        throw "$Label signature verification failed."
    }
    if ($signature.SignerCertificate.Subject -ne $ExpectedSubject) {
        throw "$Label publisher verification failed."
    }
}

function Get-BundleIdentity([string]$Path) {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [IO.Compression.ZipFile]::OpenRead($Path)
    try {
        $entry = $archive.GetEntry('AppxMetadata/AppxBundleManifest.xml')
        if (-not $entry) { throw 'The update bundle manifest is missing.' }
        $reader = [IO.StreamReader]::new($entry.Open())
        try { [xml]$manifest = $reader.ReadToEnd() } finally { $reader.Dispose() }
        return $manifest.Bundle
    } finally { $archive.Dispose() }
}

function Assert-BundlePayload([object]$Bundle, [string]$ExpectedVersion) {
    # The signed GitHub bundle always carries both application payloads. This
    # rejects a correctly named but incomplete cross-architecture bundle before
    # Add-AppxPackage chooses its payload for the local machine.
    #
    # Only <Package Type="application"> entries are payloads for a machine:
    # resource packages (Type="resource") carry a language/scale qualifier and
    # no meaningful architecture, so folding them into these asserts would fail
    # the moment the packager emits one.
    $applications = @($Bundle.Packages.Package | Where-Object { ([string]$_.Type) -eq 'application' })
    if ($applications.Count -eq 0) { throw 'The update bundle carries no application payload.' }
    $architectures = @($applications | ForEach-Object { [string]$_.Architecture } | Sort-Object -Unique)
    if (($architectures -join ',') -ne 'arm64,x64') { throw 'The update bundle payload architectures are invalid.' }
    foreach ($package in $applications) {
        if ($package.Version -ne $ExpectedVersion) { throw 'The update bundle payload version does not match the release tag.' }
    }
}

function Get-PeArchitecture([string]$Path) {
    $stream = [IO.File]::Open($Path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        $reader = [IO.BinaryReader]::new($stream)
        $stream.Position = 0x3c
        $peOffset = $reader.ReadInt32()
        $stream.Position = $peOffset + 4
        switch ($reader.ReadUInt16()) {
            0x8664 { return 'x64' }
            0xaa64 { return 'arm64' }
            default { throw 'The runtime installer architecture is unsupported.' }
        }
    } finally { $stream.Dispose() }
}

$storeInstall = Get-AppxPackage -Name '32827MikeFara.fwdslash' -ErrorAction SilentlyContinue |
    Where-Object { $_.Publisher -eq $storePublisher }
if ($storeInstall -and -not $Force) {
    Write-Host 'You already have the Microsoft Store version of Forward Slash Windows.'
    Write-Host 'Update it from the Store (Library > Get updates); it updates itself there.'
    Write-Host 'To install the GitHub build side-by-side anyway, rerun with -Force.'
    exit 1
}

# Release tags are the update trust boundary: nothing but a MAJOR.MINOR.PATCH
# tag ever reaches a URL, and the shape is asserted before the normalization
# below, never after. Both spellings of the tag are accepted from the user ('0.0.7' and 'v0.0.7'),
# but only one of them is ever turned into a URL: normalize to the canonical
# 'v'-prefixed tag exactly once, here, before anything downstream reads it.
if ($Version -ne 'latest') {
    if ($Version -notmatch '^v?[0-9]+\.[0-9]+\.[0-9]+$') {
        throw 'Version must be latest or a release tag such as v1.2.3.'
    }
    # -notmatch above is case-insensitive, so 'V0.0.7' reaches here too;
    # strip any leading v/V and re-add exactly one.
    $Version = 'v' + ($Version -replace '^[vV]', '')
}

if ($Version -eq 'latest') {
    $release = Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest"
} else {
    $release = Invoke-RestMethod "https://api.github.com/repos/$repo/releases/tags/$Version"
}
$releaseTag = [string]$release.tag_name
if ($releaseTag -notmatch '^v[0-9]+\.[0-9]+\.[0-9]+$' -or ($Version -ne 'latest' -and $releaseTag -ne $Version)) {
    throw 'The release tag is invalid or does not match the requested release.'
}

# Every release carries two bundles: this signed GitHub-flavor one, and the
# unsigned Store submission artifact, which carries the Partner Center identity
# and no signature at all — Add-AppxPackage would reject it. The pipeline names
# bundles after the four-part MSIX version (X.Y.Z.0), not the three-part tag.
if ($releaseTag -notmatch '^v([0-9]+)\.([0-9]+)\.([0-9]+)$') {
    throw 'The release tag is invalid or does not match the requested release.'
}
$expectedAssetName = "fwdslash-$($Matches[1]).$($Matches[2]).$($Matches[3]).0.msixbundle"
$expectedAssetUrl = "https://github.com/$repo/releases/download/$releaseTag/$expectedAssetName"
$asset = @($release.assets | Where-Object { $_.name -eq $expectedAssetName -and $_.browser_download_url -eq $expectedAssetUrl }) | Select-Object -First 1
if (-not $asset) {
    throw 'The release does not contain its expected signed MSIX bundle.'
}
# GitHub only began publishing asset digests recently, so every release made
# before that carries none. A missing digest is not a trust failure: the
# Authenticode signature, the package identity/publisher and the manifest
# version below are the actual boundary, and they all still run. A digest that
# is present but malformed is a different matter and still fails.
$digestProperty = $asset.PSObject.Properties['digest']
$expectedDigestHex = $null
if ($null -eq $digestProperty -or [string]::IsNullOrWhiteSpace([string]$digestProperty.Value)) {
    Write-Warning 'This release publishes no SHA-256 digest for its MSIX bundle; verifying the signature, package identity and version instead.'
} elseif ([string]$digestProperty.Value -notmatch '^sha256:([0-9a-fA-F]{64})$') {
    throw 'The release publishes a malformed SHA-256 digest for its MSIX bundle.'
} else {
    $expectedDigestHex = $matches[1].ToUpperInvariant()
}

$stagingScope = New-PrivateDownloadDirectory
$privateDirectory = $stagingScope.Path
$stagingLocks = @($stagingScope.Locks)
$outFile = Join-Path $privateDirectory $expectedAssetName
Write-Host "Downloading $($asset.name)..."
Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $outFile
$bundleLock = Open-ForwardSlashWindowsNoFollowPath $outFile
if ($expectedDigestHex) {
    $actualDigest = (Get-FileHash -Algorithm SHA256 -LiteralPath $outFile).Hash
    if ($actualDigest -ne $expectedDigestHex) { throw 'The downloaded MSIX bundle digest does not match the release.' }
}
Assert-TrustedSignature $outFile $githubPublisher 'Forward Slash Windows update'
$bundle = Get-BundleIdentity $outFile
if ($bundle.Identity.Name -ne $expectedIdentity) { throw 'The update package identity does not match Forward Slash Windows.' }
if ($bundle.Identity.Publisher -ne $githubPublisher) { throw 'The update package publisher does not match Forward Slash Windows.' }
$expectedPackageVersion = $releaseTag.TrimStart('v') + '.0'
if ($bundle.Identity.Version -ne $expectedPackageVersion) { throw 'The update package version does not match the release tag.' }
Assert-BundlePayload $bundle $expectedPackageVersion

# The settings app is a Windows App SDK application: without the framework
# package present, Add-AppxPackage fails with 0x80073CF3 (dependency missing)
# and, if it were installed anyway, the app would refuse to start. The Store
# flavor gets this resolved for it; a bare Add-AppxPackage does not.
if (-not (Test-ForwardSlashWindowsRuntime -Architecture $runtimeArchitecture)) {
    $runtimeUrl = $runtimeInstallerUrl -f $runtimeArchitecture
    $runtimeFile = Join-Path $privateDirectory "WindowsAppRuntimeInstall-$runtimeArchitecture.exe"
    Write-Host "Installing the Windows App Runtime 2.x ($runtimeArchitecture); this is a one-time prerequisite..."
    # Declared before the try: the catch disposes it, and the download itself
    # can fail before the assignment. Under StrictMode an unset variable there
    # would replace the guidance below with a bare "not defined" error.
    $runtimeLock = $null
    try {
        Invoke-WebRequest -Uri $runtimeUrl -OutFile $runtimeFile
        $runtimeLock = Open-ForwardSlashWindowsNoFollowPath $runtimeFile
        Assert-TrustedSignature $runtimeFile 'CN=Microsoft Corporation, O=Microsoft Corporation, L=Redmond, S=Washington, C=US' 'Windows App Runtime installer'
        if ((Get-PeArchitecture $runtimeFile) -ne $runtimeArchitecture) { throw 'The runtime installer architecture does not match this device.' }
        $runtimeMetadata = [Diagnostics.FileVersionInfo]::GetVersionInfo($runtimeFile)
        # Microsoft brands the same framework as "Windows App SDK" in some
        # signed binaries and "Microsoft Windows App Runtime" in others.
        # Accept only those two documented product identities, never a generic
        # executable that merely has a 2.x version string.
        if ([string]::IsNullOrWhiteSpace($runtimeMetadata.ProductName) -or
            $runtimeMetadata.ProductName -notin @('Windows App SDK', 'Microsoft Windows App Runtime')) {
            throw 'The runtime installer product identity is not Windows App SDK.'
        }
        $productVersion = $runtimeMetadata.ProductVersion
        if ([string]::IsNullOrWhiteSpace($productVersion) -or $productVersion -notmatch '^2\.[0-9]+(?:\.[0-9]+){0,2}$') {
            throw 'The runtime installer product version is not Windows App Runtime 2.x.'
        }
        # Keep $runtimeLock alive while Start-Process resolves and maps the
        # executable.  It denies replacement or deletion of the verified file.
        $runtimeProcess = Start-Process -FilePath $runtimeFile -ArgumentList '--quiet' -Wait -PassThru
        if ($runtimeProcess.ExitCode -ne 0) {
            throw "the installer exited with $($runtimeProcess.ExitCode)"
        }
    } catch {
        if ($runtimeLock) { $runtimeLock.Dispose(); $runtimeLock = $null }
        if ($bundleLock) { $bundleLock.Dispose(); $bundleLock = $null }
        Close-ForwardSlashWindowsHandles $stagingLocks
        $stagingLocks = @()
        Remove-Item $privateDirectory -Recurse -Force -ErrorAction SilentlyContinue
        Write-Host ''
        Write-Host 'The Windows App Runtime 2.x could not be installed automatically:'
        Write-Host "  $($_.Exception.Message)"
        Write-Host ''
        Write-Host 'Install it by hand and rerun this script:'
        Write-Host "  $runtimeUrl"
        Write-Host 'Without it, Forward Slash Windows cannot be installed (0x80073CF3).'
        exit 1
    }
    if ($runtimeLock) { $runtimeLock.Dispose(); $runtimeLock = $null }
    Remove-Item $runtimeFile -Force -ErrorAction SilentlyContinue
}

Write-Host 'Installing...'
Add-AppxPackage -Path $outFile
$bundleLock.Dispose()
$bundleLock = $null
Close-ForwardSlashWindowsHandles $stagingLocks
$stagingLocks = @()
Remove-Item $privateDirectory -Recurse -Force -ErrorAction SilentlyContinue

Write-Host ''
Write-Host 'Installed. Start it from the Start menu ("fwdslash"), then pick your'
Write-Host 'integrations in the app. Typing / in the Explorer address bar opens your'
Write-Host 'WSL distributions.'
