[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateSet('x86', 'x64', 'ARM64')]
    [string]$Architecture,
    [ValidateSet('Debug', 'Release')]
    [string]$Configuration = 'Release'
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$source = Join-Path $repo ("out\user\{0}\{1}" -f $Architecture.ToLowerInvariant(), $Configuration)
$packageRoot = Join-Path $repo 'out\package'
$stage = Join-Path $packageRoot ("forward-slash-windows-0.0.1-{0}" -f $Architecture.ToLowerInvariant())
$archive = "$stage.zip"

if (-not (Test-Path -LiteralPath $source)) {
    throw "Build output does not exist: $source"
}

$resolvedStage = [IO.Path]::GetFullPath($stage)
$resolvedPackageRoot = [IO.Path]::GetFullPath($packageRoot) + [IO.Path]::DirectorySeparatorChar
if (-not $resolvedStage.StartsWith($resolvedPackageRoot, [StringComparison]::OrdinalIgnoreCase)) {
    throw "Refusing to stage outside $resolvedPackageRoot"
}
if (Test-Path -LiteralPath $resolvedStage) {
    Remove-Item -LiteralPath $resolvedStage -Recurse -Force
}
New-Item -ItemType Directory -Force -Path $resolvedStage | Out-Null
$files = 'fwdslash.exe', 'fswbroker.exe', 'fswsettings.exe'
foreach ($file in $files) {
    Copy-Item -LiteralPath (Join-Path $source $file) -Destination $stage
}
foreach ($dependency in 'Microsoft.WindowsAppRuntime.Bootstrap.dll', 'resources.pri', 'fswsettings.pri') {
    $dependencyPath = Join-Path $source $dependency
    if (Test-Path -LiteralPath $dependencyPath -PathType Leaf) {
        Copy-Item -LiteralPath $dependencyPath -Destination $stage
    }
}
$appAssets = Join-Path $source 'Assets'
if (Test-Path -LiteralPath $appAssets -PathType Container) {
    Copy-Item -LiteralPath $appAssets -Destination $stage -Recurse
}

Copy-Item -LiteralPath (Join-Path $repo 'README.md') -Destination $stage
Copy-Item -LiteralPath (Join-Path $repo 'LICENSE') -Destination $stage
New-Item -ItemType Directory -Force -Path (Join-Path $stage 'shell\cmd') | Out-Null
Copy-Item -LiteralPath (Join-Path $repo 'shell\cmd\fsw-autorun.cmd') -Destination (Join-Path $stage 'shell\cmd')
Copy-Item -LiteralPath (Join-Path $repo 'shell\cmd\fsw-dir.cmd') -Destination (Join-Path $stage 'shell\cmd')
New-Item -ItemType Directory -Force -Path (Join-Path $stage 'shell\powershell') | Out-Null
Copy-Item -LiteralPath (Join-Path $repo 'shell\powershell\ForwardSlashWindows.psm1') -Destination (Join-Path $stage 'shell\powershell')
foreach ($script in 'Install-CmdAdapter.ps1', 'Uninstall-CmdAdapter.ps1',
                    'Install-PowerShellAdapter.ps1', 'Uninstall-PowerShellAdapter.ps1') {
    Copy-Item -LiteralPath (Join-Path $repo "tools\$script") -Destination $stage
}

if (Test-Path -LiteralPath $archive) {
    Remove-Item -LiteralPath $archive
}
Compress-Archive -LiteralPath $stage -DestinationPath $archive
Get-FileHash -Algorithm SHA256 -LiteralPath $archive | Format-List
