<#
.SYNOPSIS
  Download the Microsoft.Trusted.Signing.Client package and install its dlib
  into .\lib\x64 and .\lib\x86.

.DESCRIPTION
  Run this once per machine (already done on this one). No nuget.exe or dotnet
  SDK required — it pulls the .nupkg straight from nuget.org and unzips it.

.EXAMPLE
  .\install-dlib.ps1
#>

[CmdletBinding()]
param(
    [string]$Version = "1.0.60",
    [switch]$Force
)

$ErrorActionPreference = "Stop"

# SHA-512 of each accepted .nupkg, as nuget.org's catalog publishes it
# (packageHash, base64). The dlib is loaded by signtool with the signing
# secret in the environment, so an unknown version or a mismatch is fatal.
# To add a version, copy packageHash from
# https://api.nuget.org/v3/registration5-semver1/microsoft.trusted.signing.client/<version>.json
# -> catalogEntry.
$PinnedSha512 = @{
    "1.0.60" = "72ko4GcVh3JUDTIiolc06PlEV2y+SCckldlM6BKbFKCyf/pzeMDaupTu8fXcINBNow7Brz+2P3PtX7chK+pXxA=="
}
if (-not $PinnedSha512.ContainsKey($Version)) {
    throw "No pinned SHA-512 for Microsoft.Trusted.Signing.Client $Version; add it to `$PinnedSha512 first."
}
$libRoot = Join-Path $PSScriptRoot "lib"

if ((Test-Path (Join-Path $libRoot "x64\Azure.CodeSigning.Dlib.dll")) -and -not $Force) {
    Write-Host "dlib already installed in $libRoot (use -Force to reinstall)." -ForegroundColor Yellow
    exit 0
}

$temp = Join-Path ([IO.Path]::GetTempPath()) ("tsc-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force $temp | Out-Null

try {
    $nupkg = Join-Path $temp "package.zip"
    $url = "https://www.nuget.org/api/v2/package/Microsoft.Trusted.Signing.Client/$Version"
    Write-Host "Downloading Microsoft.Trusted.Signing.Client $Version ..."
    Invoke-WebRequest -Uri $url -OutFile $nupkg

    $hex = (Get-FileHash -Algorithm SHA512 -LiteralPath $nupkg).Hash
    $bytes = [byte[]]::new($hex.Length / 2)
    for ($i = 0; $i -lt $bytes.Length; $i++) {
        $bytes[$i] = [Convert]::ToByte($hex.Substring($i * 2, 2), 16)
    }
    $actual = [Convert]::ToBase64String($bytes)
    if ($actual -cne $PinnedSha512[$Version]) {
        throw "Microsoft.Trusted.Signing.Client $Version SHA-512 mismatch: got $actual."
    }
    Write-Host "[OK] Package SHA-512 matches the pinned nuget.org hash." -ForegroundColor Green

    Expand-Archive $nupkg -DestinationPath (Join-Path $temp "pkg") -Force

    New-Item -ItemType Directory -Force $libRoot | Out-Null
    foreach ($arch in "x64", "x86") {
        $src = Join-Path $temp "pkg\bin\$arch"
        if (-not (Test-Path $src)) {
            Write-Host "[WARN] Package has no bin\$arch folder; skipping." -ForegroundColor Yellow
            continue
        }
        $dest = Join-Path $libRoot $arch
        if (Test-Path $dest) { Remove-Item $dest -Recurse -Force }
        Copy-Item $src $dest -Recurse -Force
        Write-Host "[OK] Installed $arch dlib to $dest" -ForegroundColor Green
    }

    Set-Content -Path (Join-Path $libRoot "VERSION.txt") -Value "Microsoft.Trusted.Signing.Client $Version"
    Write-Host ""
    Write-Host "Done. Next: .\test-signing.ps1" -ForegroundColor Cyan
}
finally {
    Remove-Item $temp -Recurse -Force -ErrorAction SilentlyContinue
}
