[CmdletBinding()]
param(
    [ValidateSet('x64', 'ARM64', 'All')]
    [string]$Architecture = 'All',
    [ValidatePattern('^\d+\.\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$')]
    [string]$PackageVersion = '10.0.28000.2526'
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$packageRoot = Join-Path $repo 'packages'
$downloadRoot = Join-Path $repo 'out\driver-dependency-downloads'

$packageIds = [System.Collections.Generic.List[string]]::new()
$packageIds.Add('Microsoft.Windows.SDK.CPP')
if ($Architecture -in 'x64', 'All') {
    $packageIds.Add('Microsoft.Windows.SDK.CPP.x64')
    $packageIds.Add('Microsoft.Windows.WDK.x64')
}
if ($Architecture -in 'ARM64', 'All') {
    $packageIds.Add('Microsoft.Windows.SDK.CPP.arm64')
    $packageIds.Add('Microsoft.Windows.WDK.arm64')
}

New-Item -ItemType Directory -Force -Path $packageRoot, $downloadRoot | Out-Null
Add-Type -AssemblyName System.IO.Compression.FileSystem

foreach ($packageId in $packageIds) {
    $destination = Join-Path $packageRoot "$packageId.$PackageVersion"
    $nuspec = Join-Path $destination "$packageId.nuspec"
    if (Test-Path -LiteralPath $nuspec) {
        Write-Verbose "$packageId $PackageVersion is already restored."
        continue
    }

    if (Test-Path -LiteralPath $destination) {
        Remove-Item -LiteralPath $destination -Recurse -Force
    }

    $lowerId = $packageId.ToLowerInvariant()
    $baseUri = "https://api.nuget.org/v3-flatcontainer/$lowerId/$PackageVersion"
    $fileName = "$lowerId.$PackageVersion.nupkg"
    $download = Join-Path $downloadRoot $fileName

    Write-Host "Downloading $packageId $PackageVersion..."
    Invoke-WebRequest -UseBasicParsing -Uri "$baseUri/$fileName" -OutFile $download
    $registrationUri = "https://api.nuget.org/v3/registration5-semver1/$lowerId/$PackageVersion.json"
    $registration = Invoke-RestMethod -UseBasicParsing -Uri $registrationUri
    $catalogEntry = Invoke-RestMethod -UseBasicParsing -Uri $registration.catalogEntry
    if ($catalogEntry.packageHashAlgorithm -cne 'SHA512') {
        throw "NuGet reported an unsupported hash algorithm for $packageId."
    }
    $expectedHash = $catalogEntry.packageHash.Trim()

    $stream = [IO.File]::OpenRead($download)
    try {
        $sha512 = [Security.Cryptography.SHA512]::Create()
        try {
            $actualHash = [Convert]::ToBase64String($sha512.ComputeHash($stream))
        } finally {
            $sha512.Dispose()
        }
    } finally {
        $stream.Dispose()
    }
    if ($actualHash -cne $expectedHash) {
        throw "SHA-512 validation failed for $packageId $PackageVersion."
    }

    New-Item -ItemType Directory -Path $destination | Out-Null
    try {
        [IO.Compression.ZipFile]::ExtractToDirectory($download, $destination)
    } catch {
        Remove-Item -LiteralPath $destination -Recurse -Force
        throw
    }
    if (-not (Test-Path -LiteralPath $nuspec)) {
        throw "The restored $packageId package is incomplete: $nuspec is missing."
    }
}

Write-Host "Driver dependencies are restored under $packageRoot."
