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
    # ------------------------------------------------------------------
    # #134: the profile block installs lazy STUBS, not an Import-Module. The
    # stub text is owned by STUB_BODY in crates/fsw-cli/src/adapters/profile.rs,
    # so it is extracted from there rather than duplicated here -- a change to
    # the shipped block that breaks these cases fails this fixture.
    # ------------------------------------------------------------------
    $profileRs = Join-Path $PSScriptRoot '../../crates/fsw-cli/src/adapters/profile.rs'
    $stubLines = New-Object System.Collections.Generic.List[string]
    $inStub = $false
    foreach ($rustLine in (Get-Content -LiteralPath $profileRs)) {
        if (-not $inStub) {
            if ($rustLine -match '^const STUB_BODY') { $inStub = $true }
            continue
        }
        if ($rustLine -match '^\);') { break }
        $piece = $rustLine.Trim()
        if (-not $piece.StartsWith('"')) { continue }
        $piece = $piece.Substring(1)
        $piece = $piece.Substring(0, $piece.LastIndexOf('"'))
        if ($piece.EndsWith('\r\n')) { $piece = $piece.Substring(0, $piece.Length - 4) }
        $stubLines.Add($piece.Replace('\\', '\'))
    }
    if ($stubLines.Count -lt 20) { throw "STUB_BODY extraction found only $($stubLines.Count) lines." }
    # The block must never reach for a Microsoft.PowerShell.Utility cmdlet: the
    # first one costs ~70 ms of module load. Aliases go through the Management
    # `alias:` provider instead (#134).
    Assert-Equal -Expected $false -Actual ([bool](@($stubLines) -match 'Set-Alias')) -Message 'stub block uses no Utility cmdlet'
    $stubScript = "`$m = '$($modulePath.Replace("'", "''"))'`r`n" + ($stubLines -join "`r`n")
    . ([scriptblock]::Create($stubScript))

    # Before any slash argument the aliases point at the stubs and the module
    # has not been loaded at all -- this is the ~140 ms every session used to
    # pay unconditionally.
    Assert-Equal -Expected 'Invoke-FswStubChildItem' -Actual (Get-Alias -Name dir).Definition -Message 'dir is stubbed before first use'
    Assert-Equal -Expected 'Invoke-FswStubSetLocation' -Actual (Get-Alias -Name cd).Definition -Message 'cd is stubbed before first use'
    Assert-Equal -Expected 'Invoke-FswStubPushLocation' -Actual (Get-Alias -Name pushd).Definition -Message 'pushd is stubbed before first use'
    Assert-Equal -Expected $null -Actual (Get-Module -Name ForwardSlashWindows) -Message 'stub block imports nothing at profile time'

    # The stubs carry the same proxy parameter metadata as the real wrappers,
    # so binding is identical before and after the flip.
    foreach ($stubName in @('Invoke-FswStubSetLocation', 'Invoke-FswStubPushLocation')) {
        $stubCommand = Get-Command $stubName
        Assert-Equal -Expected $true -Actual $stubCommand.Parameters.ContainsKey('UseTransaction') -Message "$stubName preserves UseTransaction"
        Assert-Equal -Expected $true -Actual $stubCommand.Parameters.ContainsKey('StackName') -Message "$stubName preserves StackName"
        Assert-Equal -Expected $true -Actual $stubCommand.Parameters.ContainsKey('PassThru') -Message "$stubName preserves PassThru"
        $stubPathAttribute = @($stubCommand.Parameters['Path'].Attributes | Where-Object { $_ -is [System.Management.Automation.ParameterAttribute] })[0]
        Assert-Equal -Expected $true -Actual $stubPathAttribute.ValueFromPipeline -Message "$stubName Path preserves pipeline binding"
    }

    # Native passthrough: no slash argument, so no import and no registry read.
    Microsoft.PowerShell.Management\Set-Location -LiteralPath $base
    Invoke-FswStubSetLocation -Path $native -PassThru | Out-Null
    Assert-Location -Expected $native -Message 'stub cd passes a native path straight through'
    Invoke-FswStubPushLocation -LiteralPath $base -StackName fsw-stub -PassThru | Out-Null
    Microsoft.PowerShell.Management\Pop-Location -StackName fsw-stub
    Assert-Location -Expected $native -Message 'stub pushd keeps its caller named stack'
    Invoke-FswStubChildItem -LiteralPath $base | Out-Null
    Assert-Equal -Expected $null -Actual (Get-Module -Name ForwardSlashWindows) -Message 'native arguments never load the module'

    # First slash argument: the module loads and its Set-Alias -Force re-points
    # every alias onto the real wrappers. The resolver has no controller next to
    # the module here, so it hands back $null and the native cmdlet runs -- what
    # is under test is the flip, not the resolution.
    Microsoft.PowerShell.Management\Set-Location -LiteralPath $base
    Invoke-FswStubSetLocation -Path '/' 2>$null
    Assert-Equal -Expected $true -Actual ($null -ne (Get-Module -Name ForwardSlashWindows)) -Message 'first slash argument imports the module'
    Assert-Equal -Expected 'Invoke-ForwardSlashWindowsSetLocation' -Actual (Get-Alias -Name cd).Definition -Message 'cd alias flips to the real wrapper'
    Assert-Equal -Expected 'Invoke-ForwardSlashWindowsChildItem' -Actual (Get-Alias -Name dir).Definition -Message 'dir alias flips to the real wrapper'
    Assert-Equal -Expected 'Invoke-ForwardSlashWindowsPushLocation' -Actual (Get-Alias -Name pushd).Definition -Message 'pushd alias flips to the real wrapper'
    foreach ($flippedName in @('Invoke-ForwardSlashWindowsSetLocation', 'Invoke-ForwardSlashWindowsPushLocation')) {
        $flipped = Get-Command $flippedName
        Assert-Equal -Expected $true -Actual $flipped.Parameters.ContainsKey('UseTransaction') -Message "$flippedName preserves UseTransaction after the flip"
        $flippedPathAttribute = @($flipped.Parameters['Path'].Attributes | Where-Object { $_ -is [System.Management.Automation.ParameterAttribute] })[0]
        Assert-Equal -Expected $true -Actual $flippedPathAttribute.ValueFromPipeline -Message "$flippedName Path preserves pipeline binding after the flip"
    }

    $module = Import-Module -Name $modulePath -Force -PassThru
    & $module {
        param([string]$Target)
        $script:FswTestTarget = $Target
        Set-Item -Path function:script:Resolve-ForwardSlashWindowsTarget -Value {
            param([string]$Path)
            return [pscustomobject]@{ Kind = 'path'; Target = $script:FswTestTarget; Distributions = @(); Message = ''; Informational = $false }
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

    # #132: 'cd ..' at a distribution share root. Above \\wsl.localhost\Ubuntu
    # there is only the server name, so the native cmdlet fails with "Cannot
    # find path '\\wsl.localhost'". The wrapper resolves '/' instead, and the
    # distribution listing is informational -- $ErrorActionPreference is Stop
    # here, so a Write-Error would throw and fail this case.
    & $module {
        Set-Item -Path function:script:Get-ForwardSlashWindowsDistributionRoot -Value { return 'Ubuntu' }
        Set-Item -Path function:script:Resolve-ForwardSlashWindowsTarget -Value {
            param([string]$Path)
            $script:FswTestParentProbe = $Path
            return [pscustomobject]@{ Kind = 'root'; Target = ''; Distributions = @('Ubuntu'); Message = ''; Informational = $false }
        }
    }
    Microsoft.PowerShell.Management\Set-Location -LiteralPath $base
    Invoke-ForwardSlashWindowsSetLocation -Path '..' 6>$null
    Assert-Location -Expected $base -Message 'cd .. at a distribution root does not move'
    Assert-Equal -Expected '/' -Actual (& $module { $script:FswTestParentProbe }) `
        -Message "cd .. at a distribution root resolves '/'"

    Microsoft.PowerShell.Management\Set-Location -LiteralPath $base
    Invoke-ForwardSlashWindowsPushLocation -Path '..' 6>$null
    Assert-Location -Expected $base -Message 'pushd .. at a distribution root does not move'

    # Anywhere else '..' must stay native, even with the root mock installed.
    & $module { Set-Item -Path function:script:Get-ForwardSlashWindowsDistributionRoot -Value { return $null } }
    Microsoft.PowerShell.Management\Set-Location -LiteralPath $base
    Invoke-ForwardSlashWindowsSetLocation -Path '..'
    Assert-Location -Expected $temporaryRoot -Message 'cd .. away from a distribution root stays native'
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
    foreach ($stubName in @('Invoke-FswStubChildItem', 'Invoke-FswStubSetLocation', 'Invoke-FswStubPushLocation', 'Import-FswStubModule', 'Test-FswStubSlash', 'Test-FswStubParent')) {
        Remove-Item -Path "function:global:$stubName" -Force -ErrorAction SilentlyContinue
    }
    Remove-Item -Path variable:global:FswStubModule -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $temporaryRoot -Recurse -Force -ErrorAction SilentlyContinue
}
