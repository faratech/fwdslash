[CmdletBinding()]
param(
    [string]$ControllerPath
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repo = Split-Path -Parent $PSScriptRoot
$packagedShell = Join-Path $PSScriptRoot 'shell\cmd'
$sourceShell = if (Test-Path -LiteralPath $packagedShell -PathType Container) {
    $packagedShell
} else {
    Join-Path $repo 'shell\cmd'
}
if (-not $ControllerPath) {
    $packagedController = Join-Path $PSScriptRoot 'fswctl.exe'
    $developmentController = Join-Path $repo 'out\user\arm64\Release\fswctl.exe'
    $ControllerPath = if (Test-Path -LiteralPath $packagedController) {
        $packagedController
    } else {
        $developmentController
    }
}
$ControllerPath = [IO.Path]::GetFullPath($ControllerPath)
if (-not (Test-Path -LiteralPath $ControllerPath -PathType Leaf)) {
    throw "fswctl.exe was not found: $ControllerPath"
}

$installRoot = Join-Path $env:LOCALAPPDATA 'ForwardSlashWindows\cmd'
$installParent = Split-Path -Parent $installRoot
$transactionId = [Guid]::NewGuid().ToString('N')
$staging = Join-Path $installParent ".cmd-staging-$transactionId"
$rollbackDirectory = Join-Path $installParent ".cmd-rollback-$transactionId"
$commandProcessorPath = 'Software\Microsoft\Command Processor'
$statePath = 'Software\ForwardSlashWindows\CmdAdapter'
$marker = "call `"$installRoot\fsw-autorun.cmd`""

$currentUser = [Microsoft.Win32.Registry]::CurrentUser
$commandKey = $null
$stateKey = $null
$deployed = $false
$renamedOld = $false
$autorunChanged = $false
$createdState = $false

try {
    New-Item -ItemType Directory -Force -Path $installParent | Out-Null
    New-Item -ItemType Directory -Path $staging | Out-Null
    Copy-Item -LiteralPath (Join-Path $sourceShell 'fsw-autorun.cmd') -Destination $staging
    Copy-Item -LiteralPath (Join-Path $sourceShell 'fsw-dir.cmd') -Destination $staging
    Copy-Item -LiteralPath $ControllerPath -Destination (Join-Path $staging 'fswctl.exe')

    $stateKey = $currentUser.OpenSubKey($statePath, $true)
    if ($stateKey) {
        $state = [string]$stateKey.GetValue('State', '',
            [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        if ($state -eq 'installed') {
            throw 'The cmd adapter is already installed. Uninstall it before reinstalling.'
        }
        throw "An incomplete cmd adapter transaction exists (state: $state). Run Uninstall-CmdAdapter.ps1 to recover it."
    }

    $commandKey = $currentUser.CreateSubKey($commandProcessorPath, $true)
    $originalNames = @($commandKey.GetValueNames())
    $originalPresent = $originalNames -contains 'AutoRun'
    $originalValue = if ($originalPresent) {
        [string]$commandKey.GetValue('AutoRun', '',
            [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    } else { '' }
    $originalKind = if ($originalPresent) {
        [int]$commandKey.GetValueKind('AutoRun')
    } else { [int][Microsoft.Win32.RegistryValueKind]::String }
    if ($originalKind -notin [int][Microsoft.Win32.RegistryValueKind]::String,
        [int][Microsoft.Win32.RegistryValueKind]::ExpandString) {
        throw 'The existing Command Processor AutoRun value is not a string. No changes were made.'
    }
    $installedValue = if ([string]::IsNullOrWhiteSpace($originalValue)) {
        $marker
    } else {
        "$originalValue & $marker"
    }

    $stateKey = $currentUser.CreateSubKey($statePath, $true)
    $createdState = $true
    $stateKey.SetValue('State', 'prepared', [Microsoft.Win32.RegistryValueKind]::String)
    $stateKey.SetValue('Version', '0.0.1', [Microsoft.Win32.RegistryValueKind]::String)
    $stateKey.SetValue('TransactionId', $transactionId, [Microsoft.Win32.RegistryValueKind]::String)
    $stateKey.SetValue('InstallDirectory', $installRoot, [Microsoft.Win32.RegistryValueKind]::String)
    $stateKey.SetValue('OriginalPresent', [int]$originalPresent, [Microsoft.Win32.RegistryValueKind]::DWord)
    $stateKey.SetValue('OriginalKind', $originalKind, [Microsoft.Win32.RegistryValueKind]::DWord)
    $stateKey.SetValue('OriginalAutoRun', $originalValue, [Microsoft.Win32.RegistryValueKind]::String)
    $stateKey.SetValue('InstalledAutoRun', $installedValue, [Microsoft.Win32.RegistryValueKind]::String)
    $stateKey.Flush()

    if (Test-Path -LiteralPath $installRoot) {
        Move-Item -LiteralPath $installRoot -Destination $rollbackDirectory
        $renamedOld = $true
    }
    Move-Item -LiteralPath $staging -Destination $installRoot
    $deployed = $true

    $commandKey.SetValue('AutoRun', $installedValue,
        [Microsoft.Win32.RegistryValueKind]$originalKind)
    $commandKey.Flush()
    $autorunChanged = $true

    $stateKey.SetValue('State', 'installed', [Microsoft.Win32.RegistryValueKind]::String)
    $stateKey.Flush()
    if ($renamedOld) {
        Remove-Item -LiteralPath $rollbackDirectory -Recurse -Force
        $renamedOld = $false
    }
    Write-Host 'Forward Slash Windows cmd adapter installed for new Command Prompt sessions.'
    Write-Host 'Run Uninstall-CmdAdapter.ps1 for an exact, guarded rollback.'
} catch {
    if ($autorunChanged -and $commandKey) {
        if ($originalPresent) {
            $commandKey.SetValue('AutoRun', $originalValue,
                [Microsoft.Win32.RegistryValueKind]$originalKind)
        } else {
            $commandKey.DeleteValue('AutoRun', $false)
        }
        $commandKey.Flush()
    }
    if ($deployed -and (Test-Path -LiteralPath $installRoot)) {
        Remove-Item -LiteralPath $installRoot -Recurse -Force
    }
    if ($renamedOld -and (Test-Path -LiteralPath $rollbackDirectory)) {
        Move-Item -LiteralPath $rollbackDirectory -Destination $installRoot
    }
    if (Test-Path -LiteralPath $staging) {
        Remove-Item -LiteralPath $staging -Recurse -Force
    }
    if ($stateKey) {
        $stateKey.Close()
        $stateKey = $null
    }
    if ($createdState) {
        $currentUser.DeleteSubKeyTree($statePath, $false)
    }
    throw
} finally {
    if ($stateKey) { $stateKey.Close() }
    if ($commandKey) { $commandKey.Close() }
    $currentUser.Close()
}
