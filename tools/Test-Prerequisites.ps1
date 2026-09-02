[CmdletBinding()]
param(
    [switch]$RequireWdk
)

$ErrorActionPreference = 'Stop'
$missing = [System.Collections.Generic.List[string]]::new()

$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
if (-not (Test-Path -LiteralPath $vswhere)) {
    $missing.Add('Visual Studio 2022 or 2026 with Desktop development with C++')
} else {
    $installation = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if (-not $installation) {
        $missing.Add('MSVC x64/x86 build tools')
    }
}

if ($RequireWdk) {
    $msbuild = if (Test-Path -LiteralPath $vswhere) {
        & $vswhere -latest -products '*' -requires Microsoft.Component.MSBuild -find 'MSBuild\**\Bin\MSBuild.exe' | Select-Object -First 1
    }
    if (-not $msbuild) {
        $missing.Add('MSBuild for the repository-local WDK NuGet build')
    }
}

if ($missing.Count -gt 0) {
    Write-Error ("Missing prerequisites:`n - " + ($missing -join "`n - "))
}

Write-Host 'Forward Slash Windows build prerequisites are available.'
