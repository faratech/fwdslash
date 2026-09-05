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

# The Windows App Runtime 2.x redistributable the manifest depends on
# (Microsoft.WindowsAppRuntime.2, MinVersion 2.0.0.0). Microsoft rewrites the
# aka.ms redirector target with each servicing release, so RE-VERIFY THIS URL
# whenever the Windows App SDK dependency in packaging\AppxManifest.xml moves:
# it is a redirector, and a wrong channel installs a runtime the package still
# refuses to run against.
$runtimeInstallerUrl = 'https://aka.ms/windowsappsdk/2.0/latest/windowsappruntimeinstall-{0}.exe'
$runtimeFrameworkName = 'Microsoft.WindowsAppRuntime.2'
$runtimeMinimumVersion = [version]'2.0.0.0'

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

$storeInstall = Get-AppxPackage -Name '32827MikeFara.fwdslash' -ErrorAction SilentlyContinue |
    Where-Object { $_.Publisher -eq $storePublisher }
if ($storeInstall -and -not $Force) {
    Write-Host 'You already have the Microsoft Store version of Forward Slash Windows.'
    Write-Host 'Update it from the Store (Library > Get updates); it updates itself there.'
    Write-Host 'To install the GitHub build side-by-side anyway, rerun with -Force.'
    exit 1
}

# A tag is 'v0.0.3'; accepting both that and '0.0.3' avoids building
# releases/tags/vv0.0.3, which is a 404.
$Version = $Version.TrimStart('v', 'V')

if ($Version -eq 'latest') {
    $release = Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest"
} else {
    $release = Invoke-RestMethod "https://api.github.com/repos/$repo/releases/tags/v$Version"
}

# Every release carries two bundles: this signed GitHub-flavor one, and the
# unsigned Store submission artifact, which carries the Partner Center identity
# and no signature at all — Add-AppxPackage would reject it.
$asset = $release.assets |
    Where-Object { $_.name -like '*.msixbundle' -and $_.name -notlike '*-store-unsigned.msixbundle' } |
    Select-Object -First 1
if (-not $asset) {
    throw 'The release does not contain a signed MSIX bundle.'
}

$outFile = Join-Path $env:TEMP $asset.name
Write-Host "Downloading $($asset.name)..."
Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $outFile

# The settings app is a Windows App SDK application: without the framework
# package present, Add-AppxPackage fails with 0x80073CF3 (dependency missing)
# and, if it were installed anyway, the app would refuse to start. The Store
# flavor gets this resolved for it; a bare Add-AppxPackage does not.
if (-not (Test-ForwardSlashWindowsRuntime -Architecture $runtimeArchitecture)) {
    $runtimeUrl = $runtimeInstallerUrl -f $runtimeArchitecture
    $runtimeFile = Join-Path $env:TEMP "WindowsAppRuntimeInstall-$runtimeArchitecture.exe"
    Write-Host "Installing the Windows App Runtime 2.x ($runtimeArchitecture); this is a one-time prerequisite..."
    try {
        Invoke-WebRequest -Uri $runtimeUrl -OutFile $runtimeFile
        $runtimeProcess = Start-Process -FilePath $runtimeFile -ArgumentList '--quiet' -Wait -PassThru
        if ($runtimeProcess.ExitCode -ne 0) {
            throw "the installer exited with $($runtimeProcess.ExitCode)"
        }
    } catch {
        Remove-Item $runtimeFile -Force -ErrorAction SilentlyContinue
        Remove-Item $outFile -Force -ErrorAction SilentlyContinue
        Write-Host ''
        Write-Host 'The Windows App Runtime 2.x could not be installed automatically:'
        Write-Host "  $($_.Exception.Message)"
        Write-Host ''
        Write-Host 'Install it by hand and rerun this script:'
        Write-Host "  $runtimeUrl"
        Write-Host 'Without it, Forward Slash Windows cannot be installed (0x80073CF3).'
        exit 1
    }
    Remove-Item $runtimeFile -Force -ErrorAction SilentlyContinue
}

Write-Host 'Installing...'
Add-AppxPackage -Path $outFile
Remove-Item $outFile -Force -ErrorAction SilentlyContinue

Write-Host ''
Write-Host 'Installed. Start it from the Start menu ("fwdslash"), then pick your'
Write-Host 'integrations in the app. Typing / in the Explorer address bar opens your'
Write-Host 'WSL distributions.'
