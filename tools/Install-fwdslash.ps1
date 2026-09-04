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
    [string]$Version = 'latest'
)

$ErrorActionPreference = 'Stop'
$repo = 'faratech/fwdslash'

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
