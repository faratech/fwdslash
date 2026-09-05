# Native location-wrapper regression fixture for #68.
# Run with: pwsh -NoProfile -File test/powershell/ForwardSlashWindows.LocationRegression.ps1
# It changes only a temporary local location stack. Resolver and disabled-state
# seams are mocked inside the module; the location cmdlets are real native ones.

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Assert-Equal {
    param([object]$Expected, [object]$Actual, [string]$Message)
    if ($Expected -ne $Actual) { throw "$Message. Expected '$Expected', got '$Actual'." }
}

function Assert-Location {
    param([string]$Expected, [string]$Message)
    Assert-Equal -Expected ([System.IO.Path]::GetFullPath($Expected)) `
        -Actual (Get-Location).ProviderPath -Message $Message
}

$modulePath = Join-Path $PSScriptRoot '../../shell/powershell/ForwardSlashWindows.psm1'
$originalLocation = Get-Location
$temporaryRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("fsw-location-{0}" -f [guid]::NewGuid())
$base = Join-Path $temporaryRoot 'base'
$resolved = Join-Path $temporaryRoot 'resolved'
$native = Join-Path $temporaryRoot 'native'
$aliases = @{}
$aliasNames = @('dir', 'ls', 'cd', 'chdir', 'sl', 'pushd')
$previousPushWrapper = Get-Item -Path function:global:Invoke-ForwardSlashWindowsPushLocation -ErrorAction SilentlyContinue

try {
    foreach ($name in $aliasNames) {
        $existing = Get-Alias -Name $name -ErrorAction SilentlyContinue
        if ($null -ne $existing) { $aliases[$name] = $existing.Definition }
    }
    New-Item -ItemType Directory -Path $base, $resolved, $native | Out-Null
    $module = Import-Module -Name $modulePath -Force -PassThru
    & $module {
        param([string]$Target)
        $script:FswTestTarget = $Target
        Set-Item -Path function:script:Test-ForwardSlashWindowsDisabled -Value { return $false }
        Set-Item -Path function:script:Resolve-ForwardSlashWindowsTarget -Value {
            param([string]$Path)
            return [pscustomobject]@{ Kind = 'path'; Target = $script:FswTestTarget; Distributions = @(); Message = '' }
        }
    } $resolved

    Microsoft.PowerShell.Management\Set-Location -LiteralPath $base
    $positional = Invoke-ForwardSlashWindowsSetLocation '/Ubuntu' -PassThru
    Assert-Equal -Expected $resolved -Actual $positional.ProviderPath -Message 'positional slash Path preserves PassThru'
    Assert-Location -Expected $resolved -Message 'positional slash Path resolves'

    Microsoft.PowerShell.Management\Set-Location -LiteralPath $base
    $pathOutput = Invoke-ForwardSlashWindowsSetLocation -Path '/Ubuntu' -PassThru
    Assert-Equal -Expected $resolved -Actual $pathOutput.ProviderPath -Message 'named Path preserves PassThru'
    Assert-Location -Expected $resolved -Message 'named Path resolves'

    Microsoft.PowerShell.Management\Set-Location -LiteralPath $base
    $literalOutput = Invoke-ForwardSlashWindowsSetLocation -LiteralPath '/Ubuntu' -PassThru
    Assert-Equal -Expected $resolved -Actual $literalOutput.ProviderPath -Message 'named LiteralPath preserves PassThru'
    Assert-Location -Expected $resolved -Message 'named LiteralPath resolves'

    Microsoft.PowerShell.Management\Set-Location -LiteralPath $base
    $pushOutput = Invoke-ForwardSlashWindowsPushLocation -Path '/Ubuntu' -StackName fsw-regression -PassThru
    Assert-Equal -Expected $resolved -Actual $pushOutput.ProviderPath -Message 'slash pushd preserves PassThru'
    Assert-Location -Expected $resolved -Message 'slash pushd resolves onto the requested stack'
    Microsoft.PowerShell.Management\Pop-Location -StackName fsw-regression
    Assert-Location -Expected $base -Message 'named stack pop returns to the original location'

    Microsoft.PowerShell.Management\Set-Location -LiteralPath $base
    Invoke-ForwardSlashWindowsSetLocation -Path $native -PassThru | Out-Null
    Assert-Location -Expected $native -Message 'non-slash Set-Location remains native'
    Invoke-ForwardSlashWindowsPushLocation -LiteralPath $base -StackName fsw-native -PassThru | Out-Null
    Microsoft.PowerShell.Management\Pop-Location -StackName fsw-native
    Assert-Location -Expected $native -Message 'non-slash Push-Location keeps its named stack'
} finally {
    Microsoft.PowerShell.Management\Set-Location -Path $originalLocation
    Remove-Module -Name ForwardSlashWindows -Force -ErrorAction SilentlyContinue
    foreach ($name in $aliasNames) {
        Remove-Item -Path "alias:$name" -Force -ErrorAction SilentlyContinue
        if ($aliases.ContainsKey($name)) { Set-Alias -Name $name -Value $aliases[$name] -Scope Global -Force }
    }
    if ($null -eq $previousPushWrapper) {
        Remove-Item -Path function:global:Invoke-ForwardSlashWindowsPushLocation -Force -ErrorAction SilentlyContinue
    } else {
        Set-Item -Path function:global:Invoke-ForwardSlashWindowsPushLocation -Value $previousPushWrapper.ScriptBlock
    }
    Remove-Item -LiteralPath $temporaryRoot -Recurse -Force -ErrorAction SilentlyContinue
}
