[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$commandProcessorPath = 'Software\Microsoft\Command Processor'
$statePath = 'Software\ForwardSlashWindows\CmdAdapter'
$currentUser = [Microsoft.Win32.Registry]::CurrentUser
$stateKey = $currentUser.OpenSubKey($statePath, $true)
if (-not $stateKey) {
    $currentUser.Close()
    Write-Host 'Forward Slash Windows cmd adapter is not installed.'
    return
}

$commandKey = $null
$renamed = $false
$registryRestored = $false
$removalPath = $null
try {
    $state = [string]$stateKey.GetValue('State', '',
        [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    $installRoot = [string]$stateKey.GetValue('InstallDirectory', '',
        [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    $installedValue = [string]$stateKey.GetValue('InstalledAutoRun', '',
        [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    $originalValue = [string]$stateKey.GetValue('OriginalAutoRun', '',
        [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    $originalPresent = [int]$stateKey.GetValue('OriginalPresent', 0) -ne 0
    $originalKind = [Microsoft.Win32.RegistryValueKind][int]$stateKey.GetValue(
        'OriginalKind', [int][Microsoft.Win32.RegistryValueKind]::String)
    if ($state -notin 'installed', 'prepared', 'removing') {
        throw "Unknown adapter transaction state '$state'. No changes were made."
    }

    $commandKey = $currentUser.CreateSubKey($commandProcessorPath, $true)
    $currentNames = @($commandKey.GetValueNames())
    $currentPresent = $currentNames -contains 'AutoRun'
    $currentValue = if ($currentPresent) {
        [string]$commandKey.GetValue('AutoRun', '',
            [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    } else { '' }
    if ($currentPresent -and $currentValue -ne $installedValue -and
        $currentValue -ne $originalValue) {
        throw 'Command Processor AutoRun changed after installation. Refusing to overwrite it; reconcile that value and retry.'
    }

    $savedRemovalPath = [string]$stateKey.GetValue('RemovalPath', '',
        [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    if ($state -eq 'removing' -and $savedRemovalPath) {
        $removalPath = $savedRemovalPath
        $renamed = Test-Path -LiteralPath $removalPath
    } elseif ($installRoot -and (Test-Path -LiteralPath $installRoot)) {
        $removalPath = "$installRoot.removing-$([Guid]::NewGuid().ToString('N'))"
        $stateKey.SetValue('RemovalPath', $removalPath,
            [Microsoft.Win32.RegistryValueKind]::String)
        $stateKey.SetValue('State', 'removing',
            [Microsoft.Win32.RegistryValueKind]::String)
        $stateKey.Flush()
        Move-Item -LiteralPath $installRoot -Destination $removalPath
        $renamed = $true
    }

    if ($originalPresent) {
        $commandKey.SetValue('AutoRun', $originalValue, $originalKind)
    } else {
        $commandKey.DeleteValue('AutoRun', $false)
    }
    $commandKey.Flush()
    $registryRestored = $true

    $stateKey.Close()
    $stateKey = $null
    $currentUser.DeleteSubKeyTree($statePath, $false)
    if ($renamed -and (Test-Path -LiteralPath $removalPath)) {
        Remove-Item -LiteralPath $removalPath -Recurse -Force
    }
    Write-Host 'Forward Slash Windows cmd adapter uninstalled and the previous AutoRun value restored.'
    Write-Host 'Already-open Command Prompt windows keep their in-memory macros until closed.'
} catch {
    if (-not $registryRestored -and $renamed -and
        -not (Test-Path -LiteralPath $installRoot) -and
        (Test-Path -LiteralPath $removalPath)) {
        Move-Item -LiteralPath $removalPath -Destination $installRoot
    }
    throw
} finally {
    if ($stateKey) { $stateKey.Close() }
    if ($commandKey) { $commandKey.Close() }
    $currentUser.Close()
}
