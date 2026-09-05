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
$bracketed = Join-Path $temporaryRoot 'resolved[brackets]'
$native = Join-Path $temporaryRoot 'native'
$aliases = @{}
$aliasNames = @('dir', 'ls', 'cd', 'chdir', 'sl', 'pushd')
$previousSetWrapper = Get-Item -Path function:global:Invoke-ForwardSlashWindowsSetLocation -ErrorAction SilentlyContinue
$previousPushWrapper = Get-Item -Path function:global:Invoke-ForwardSlashWindowsPushLocation -ErrorAction SilentlyContinue

try {
    foreach ($name in $aliasNames) {
        $existing = Get-Alias -Name $name -ErrorAction SilentlyContinue
        if ($null -ne $existing) { $aliases[$name] = $existing.Definition }
    }
    New-Item -ItemType Directory -Path $base, $resolved, $bracketed, $native | Out-Null
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

    $setCommand = Get-Command Invoke-ForwardSlashWindowsSetLocation
    $pushCommand = Get-Command Invoke-ForwardSlashWindowsPushLocation
    foreach ($command in @($setCommand, $pushCommand)) {
        Assert-Equal -Expected $true -Actual $command.Parameters.ContainsKey('UseTransaction') -Message "$($command.Name) preserves UseTransaction"
        $pathAttribute = @($command.Parameters['Path'].Attributes | Where-Object { $_ -is [System.Management.Automation.ParameterAttribute] })[0]
        Assert-Equal -Expected $true -Actual $pathAttribute.ValueFromPipeline -Message "$($command.Name) Path preserves pipeline binding"
    }

    & $module { param([string]$Target) $script:FswTestTarget = $Target } $bracketed
    Microsoft.PowerShell.Management\Set-Location -LiteralPath $base
    Invoke-ForwardSlashWindowsSetLocation -Path '/Ubuntu' -PassThru | Out-Null
    Assert-Location -Expected $bracketed -Message 'resolved brackets remain literal for Set-Location'
    Microsoft.PowerShell.Management\Set-Location -LiteralPath $base
    Invoke-ForwardSlashWindowsPushLocation -Path '/Ubuntu' -StackName fsw-brackets -PassThru | Out-Null
    Assert-Location -Expected $bracketed -Message 'resolved brackets remain literal for Push-Location'
    Microsoft.PowerShell.Management\Pop-Location -StackName fsw-brackets
    Assert-Location -Expected $base -Message 'bracketed Push-Location returns through its caller stack'
    & $module { param([string]$Target) $script:FswTestTarget = $Target } $resolved

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
    Microsoft.PowerShell.Management\Push-Location -LiteralPath $base -StackName fsw-set-stack
    Microsoft.PowerShell.Management\Set-Location -LiteralPath $resolved
    Invoke-ForwardSlashWindowsSetLocation -StackName fsw-set-stack -PassThru | Out-Null
    $stackLocation = Microsoft.PowerShell.Management\Get-Location -StackName fsw-set-stack
    Assert-Equal -Expected $base -Actual $stackLocation.ProviderPath -Message 'Set-Location selects the caller named stack'
    Microsoft.PowerShell.Management\Pop-Location -StackName fsw-set-stack

    Microsoft.PowerShell.Management\Set-Location -LiteralPath $base
    Invoke-ForwardSlashWindowsSetLocation -Path $native -PassThru | Out-Null
    Assert-Location -Expected $native -Message 'non-slash Set-Location remains native'
    Invoke-ForwardSlashWindowsPushLocation -LiteralPath $base -StackName fsw-native -PassThru | Out-Null
    Microsoft.PowerShell.Management\Pop-Location -StackName fsw-native
    Assert-Location -Expected $native -Message 'non-slash Push-Location keeps its named stack'

    Microsoft.PowerShell.Management\Set-Location -LiteralPath $base
    $setPipeline = @(@($base, $native) | Invoke-ForwardSlashWindowsSetLocation -PassThru)
    Assert-Equal -Expected 2 -Actual $setPipeline.Count -Message 'Set-Location pipeline emits every input'
    Assert-Equal -Expected $base -Actual $setPipeline[0].ProviderPath -Message 'Set-Location pipeline preserves first input order'
    Assert-Equal -Expected $native -Actual $setPipeline[1].ProviderPath -Message 'Set-Location pipeline preserves second input order'

    Microsoft.PowerShell.Management\Set-Location -LiteralPath $base
    $pushPipeline = @(@($base, $native) | Invoke-ForwardSlashWindowsPushLocation -StackName fsw-pipeline -PassThru)
    Assert-Equal -Expected 2 -Actual $pushPipeline.Count -Message 'Push-Location pipeline emits every input'
    Assert-Equal -Expected $base -Actual $pushPipeline[0].ProviderPath -Message 'Push-Location pipeline preserves first input order'
    Assert-Equal -Expected $native -Actual $pushPipeline[1].ProviderPath -Message 'Push-Location pipeline preserves second input order'
    Microsoft.PowerShell.Management\Pop-Location -StackName fsw-pipeline
    Microsoft.PowerShell.Management\Pop-Location -StackName fsw-pipeline
} finally {
    Microsoft.PowerShell.Management\Set-Location -Path $originalLocation
    Remove-Module -Name ForwardSlashWindows -Force -ErrorAction SilentlyContinue
    foreach ($name in $aliasNames) {
        Remove-Item -Path "alias:$name" -Force -ErrorAction SilentlyContinue
        if ($aliases.ContainsKey($name)) { Set-Alias -Name $name -Value $aliases[$name] -Scope Global -Force }
    }
    if ($null -eq $previousSetWrapper) {
        Remove-Item -Path function:global:Invoke-ForwardSlashWindowsSetLocation -Force -ErrorAction SilentlyContinue
    } else {
        Set-Item -Path function:global:Invoke-ForwardSlashWindowsSetLocation -Value $previousSetWrapper.ScriptBlock
    }
    if ($null -eq $previousPushWrapper) {
        Remove-Item -Path function:global:Invoke-ForwardSlashWindowsPushLocation -Force -ErrorAction SilentlyContinue
    } else {
        Set-Item -Path function:global:Invoke-ForwardSlashWindowsPushLocation -Value $previousPushWrapper.ScriptBlock
    }
    Remove-Item -LiteralPath $temporaryRoot -Recurse -Force -ErrorAction SilentlyContinue
}
