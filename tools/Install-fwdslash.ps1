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
    # already present (the two coexist side-by-side; the 'fwdslash' alias and
    # protocol route to whichever registered last).
    [switch]$Force
)

$ErrorActionPreference = 'Stop'
$repo = 'faratech/fwdslash'
$storePublisher = 'CN=ABDB6B3F-DF9E-447D-BC0E-4DA7BAFD14C4'

$storeInstall = Get-AppxPackage -Name '32827MikeFara.fwdslash' -ErrorAction SilentlyContinue |
    Where-Object { $_.Publisher -eq $storePublisher }
if ($storeInstall -and -not $Force) {
    Write-Host 'You already have the Microsoft Store version of Forward Slash Windows.'
    Write-Host 'Update it from the Store (Library > Get updates); it updates itself there.'
    Write-Host 'To install the GitHub build side-by-side anyway, rerun with -Force.'
    exit 1
}

if ($Version -eq 'latest') {
    $release = Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest"
} else {
    $release = Invoke-RestMethod "https://api.github.com/repos/$repo/releases/tags/v$Version"
}

$asset = $release.assets |
    Where-Object { $_.name -like '*.msixbundle' } |
    Select-Object -First 1
if (-not $asset) {
    throw 'The release does not contain an MSIX bundle.'
}

$outFile = Join-Path $env:TEMP $asset.name
Write-Host "Downloading $($asset.name)..."
Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $outFile

Write-Host 'Installing...'
Add-AppxPackage -Path $outFile
Remove-Item $outFile -Force -ErrorAction SilentlyContinue

Write-Host ''
Write-Host 'Installed. Start it from the Start menu ("fwdslash"), then pick your'
Write-Host 'integrations in the app. Typing / in the Explorer address bar opens your'
Write-Host 'WSL distributions.'
