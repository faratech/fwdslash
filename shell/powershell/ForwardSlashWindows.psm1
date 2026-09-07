Set-StrictMode -Version 2.0

$script:FswController = Join-Path $PSScriptRoot 'fwdslash.exe'

function Resolve-ForwardSlashWindowsTarget {
    param([Parameter(Mandatory = $true)][string]$Path)

    # Zombie guard: if the product was uninstalled while this session had the
    # module loaded, the staged controller is gone. Fall back to the native
    # cmdlet rather than spawn a controller that no longer exists (#37).
    if (-not (Test-Path -LiteralPath $script:FswController)) {
        return $null
    }
    # One spawn per slash argument. shell-resolve answers from the settings
    # snapshot alone -- no broker round trip, no filter-port probe -- and
    # returns the kind, the target and the distribution list together. That
    # snapshot includes the global pause, so a paused product comes back as
    # kind:'native' and the module never opens the settings key itself (#135).
    $output = & $script:FswController shell-resolve $Path 2>$null
    if (-not $output) {
        return $null
    }
    $line = [string](@($output) | Select-Object -First 1)
    if (-not $line) {
        return $null
    }
    try {
        $data = $line | ConvertFrom-Json
    } catch {
        return $null
    }
    if ($null -eq $data) {
        return $null
    }
    if ($data.PSObject.Properties['error']) {
        # A rejected input: the message is the resolver's, and the caller
        # shows it instead of letting PowerShell guess at 'C:\etc'.
        return [pscustomobject]@{
            PSTypeName    = 'ForwardSlashWindows.Target'
            Kind          = 'error'
            Target        = ''
            Distributions = @()
            Message       = [string]$data.error
        }
    }
    if (-not $data.PSObject.Properties['kind']) {
        return $null
    }
    $kind = [string]$data.kind
    # 'native' (exit 3) means resolution is paused or the input is not a
    # slash path: the caller runs its own cmdlet untouched.
    if (-not $kind -or $kind -eq 'native') {
        return $null
    }
    $target = ''
    if ($data.PSObject.Properties['target'] -and $null -ne $data.target) {
        $target = [string]$data.target
    }
    $distributions = @()
    if ($data.PSObject.Properties['distributions'] -and $null -ne $data.distributions) {
        $distributions = @($data.distributions)
    }
    return [pscustomobject]@{
        PSTypeName    = 'ForwardSlashWindows.Target'
        Kind          = $kind
        Target        = $target
        Distributions = $distributions
        Message       = ''
        # Whether Message is an answer to a legitimate request rather than
        # a rejection: the caller shows it with Write-Host, not Write-Error.
        Informational = $false
    }
}

function Get-ForwardSlashWindowsRootMessage {
    param([string[]]$Distributions = @())

    if ($null -eq $Distributions -or $Distributions.Count -eq 0) {
        return 'No WSL distributions are registered.'
    }
    $list = ($Distributions | ForEach-Object { "/$_" }) -join ', '
    return "/ lists your WSL distributions: $list. Use cd /<Distro>, or run 'fwdslash bare-slash default' so / opens your default distribution."
}

function Get-ForwardSlashWindowsPathIndex {
    # The argument positions that may carry a path: every -Path/-LiteralPath
    # value and every positional argument that starts with '/'.
    param([object[]]$Arguments = @())

    $indexes = New-Object System.Collections.Generic.List[int]
    $expectPath = $false
    for ($index = 0; $index -lt $Arguments.Count; $index++) {
        $argument = $Arguments[$index]
        if ($expectPath) {
            $indexes.Add($index)
            $expectPath = $false
            continue
        }
        if ($argument -is [string] -and
            ($argument -eq '-Path' -or $argument -eq '-LiteralPath')) {
            $expectPath = $true
            continue
        }
        if ($argument -is [string] -and $argument.StartsWith('/')) {
            $indexes.Add($index)
        }
    }
    return , $indexes
}

function Invoke-ForwardSlashWindowsChildItem {
    $forward = @($args)
    $pathIndexes = Get-ForwardSlashWindowsPathIndex -Arguments $forward

    # No slash argument: never spawn the controller, never read a setting.
    # The global pause is decided by shell-resolve itself -- it answers
    # kind:'native' when Disabled is set (#135), so the module no longer
    # duplicates the settings key literal that crates/fsw-core/src/lib.rs
    # owns for the rest of the product.
    if ($pathIndexes.Count -eq 0) {
        Microsoft.PowerShell.Management\Get-ChildItem @forward
        return
    }

    $rootDistributions = $null
    foreach ($index in $pathIndexes) {
        $value = $forward[$index]
        if ($value -is [string]) {
            if (-not $value.StartsWith('/')) {
                continue
            }
            $result = Resolve-ForwardSlashWindowsTarget -Path $value
            if ($null -eq $result -or $result.Kind -eq 'error') {
                Microsoft.PowerShell.Management\Get-ChildItem @forward
                return
            }
            if ($result.Kind -eq 'root') {
                # Bare "/" in distribution-list mode: the listing below is
                # built from the distributions shell-resolve returned.
                $rootDistributions = $result.Distributions
                continue
            }
            $forward[$index] = $result.Target
            continue
        }
        if ($value -is [System.Collections.IEnumerable]) {
            $replacement = @()
            foreach ($item in $value) {
                if ($item -is [string] -and $item.StartsWith('/')) {
                    $result = Resolve-ForwardSlashWindowsTarget -Path $item
                    if ($null -eq $result -or $result.Kind -eq 'error') {
                        Microsoft.PowerShell.Management\Get-ChildItem @forward
                        return
                    }
                    if ($result.Kind -eq 'root') {
                        $rootDistributions = $result.Distributions
                        continue
                    }
                    $replacement += $result.Target
                } else {
                    $replacement += $item
                }
            }
            $forward[$index] = $replacement
        }
    }

    if ($null -ne $rootDistributions) {
        if ($pathIndexes.Count -ne 1) {
            throw "Bare '/' cannot be combined with other Get-ChildItem arguments. Use 'fwdslash list /' for advanced root queries."
        }
        foreach ($distribution in @($rootDistributions)) {
            [pscustomobject]@{
                PSTypeName   = 'ForwardSlashWindows.Distribution'
                Name         = "/$distribution"
                FullName     = "\\wsl.localhost\$distribution"
                Distribution = [string]$distribution
            }
        }
        return
    }

    Microsoft.PowerShell.Management\Get-ChildItem @forward
}

function Resolve-ForwardSlashWindowsLocationTarget {
    # The shared front half of the cd/chdir/sl and pushd wrappers: find the
    # first slash-prefixed path among the arguments and resolve it. $null
    # means "run the native cmdlet untouched" -- no slash argument at all,
    # resolution paused, or an input the controller handed back (exit 3) --
    # so cd -, cd ~, cd .. and cd C:\ never reach the resolver. A non-empty
    # Message is the outcome that must be shown instead of moving.
    #
    # Exported because the pushd wrapper below is defined in the global scope
    # and can only call commands that are visible there.
    param([System.Collections.IDictionary]$BoundParameters)

    $slashPath = $null
    foreach ($parameterName in @('LiteralPath', 'Path')) {
        if ($BoundParameters.ContainsKey($parameterName)) {
            $value = $BoundParameters[$parameterName]
            if ($value -is [string] -and $value.StartsWith('/')) {
                $slashPath = $value
                break
            }
        }
    }
    $parentOfDistribution = $false
    if ($null -eq $slashPath) {
        # 'cd ..' at a distribution share root. Above \\wsl.localhost\Ubuntu
        # there is only the server name, which no provider can enter, so the
        # native answer is "Cannot find path '\\wsl.localhost'" -- it reads
        # like a broken install rather than "there is nothing above this".
        # In this product '/' is what the parent of a distribution means, so
        # resolve that instead (#132). Both checks are string work on values
        # already in hand, and the location lookup is only reached once the
        # arguments already look like '..', so an ordinary 'cd ..' anywhere
        # else still pays nothing. Paused, shell-resolve answers 'native' and
        # the native error stands (#135).
        if (-not (Test-ForwardSlashWindowsParentReference -Arguments @($BoundParameters.Values))) {
            return $null
        }
        if (-not (Get-ForwardSlashWindowsDistributionRoot)) {
            return $null
        }
        $slashPath = '/'
        $parentOfDistribution = $true
    }
    $result = Resolve-ForwardSlashWindowsTarget -Path $slashPath
    if ($null -eq $result) {
        return $null
    }
    if ($result.Kind -eq 'root') {
        # Not one directory: say which distributions there are instead of
        # moving to the current drive's root.
        $result.Message = Get-ForwardSlashWindowsRootMessage -Distributions $result.Distributions
        # Going up from a distribution is a navigation request, not a typo:
        # the listing is the answer, so it must not be written as an error.
        if ($parentOfDistribution) {
            $result.Informational = $true
        }
    }
    return $result
}

function Get-ForwardSlashWindowsDistributionRoot {
    # The distribution whose share root is the current location, or $null.
    # \\wsl.localhost\Ubuntu (and the \\wsl$ spelling) is a share root: above
    # it is only the server name \\wsl.localhost, which no provider can enter.
    $location = $null
    try {
        $location = (Get-Location).ProviderPath
    } catch {
        return $null
    }
    if ([string]::IsNullOrEmpty($location)) {
        return $null
    }
    $trimmed = $location.TrimEnd('\')
    if (-not $trimmed.StartsWith('\\')) {
        return $null
    }
    # \\server\share splits into exactly two parts; anything deeper has a
    # real parent directory and must keep the native behaviour.
    $parts = $trimmed.Substring(2).Split('\')
    if ($parts.Count -ne 2) {
        return $null
    }
    if ($parts[0] -ne 'wsl.localhost' -and $parts[0] -ne 'wsl$') {
        return $null
    }
    if ([string]::IsNullOrEmpty($parts[1])) {
        return $null
    }
    return $parts[1]
}

function Test-ForwardSlashWindowsParentReference {
    # Whether these arguments ask for the parent directory -- 'cd ..',
    # 'cd ../', 'cd -Path ..'. Nothing else is this special case.
    param([object[]]$Arguments = @())

    foreach ($argument in $Arguments) {
        if ($argument -is [string] -and
            ($argument -eq '..' -or $argument -eq '../' -or $argument -eq '..\')) {
            return $true
        }
    }
    return $false
}

function Invoke-ForwardSlashWindowsSetLocation {
    [CmdletBinding(DefaultParameterSetName = 'Path')]
    param(
        [Parameter(Position = 0, ParameterSetName = 'Path', ValueFromPipeline = $true)]
        [string]$Path,
        [Parameter(Mandatory = $true, ParameterSetName = 'LiteralPath')]
        [string]$LiteralPath,
        [Parameter(Mandatory = $true, ParameterSetName = 'Stack')]
        [string]$StackName,
        [switch]$PassThru,
        [switch]$UseTransaction
    )

    process {
        $forward = $PSBoundParameters
        $result = Resolve-ForwardSlashWindowsLocationTarget -BoundParameters $forward
        if ($null -eq $result) {
            Microsoft.PowerShell.Management\Set-Location @forward
            return
        }
        if ($result.Message) {
        # A rejected input or the bare root: never fall through to the
        # native cmdlet's misleading "Cannot find path 'C:\etc'".
            if ($result.Informational) {
                Write-Host $result.Message
            } else {
                Write-Error $result.Message
            }
            return
        }
        # The resolver's target is filesystem data, never a wildcard pattern.
        # Replace Path rather than retaining it so native wildcard expansion
        # cannot reinterpret a literal '[' or ']' in a resolved directory.
        [void]$forward.Remove('Path')
        $forward['LiteralPath'] = $result.Target
        Microsoft.PowerShell.Management\Set-Location @forward
    }
}

# Set-Location has location stacks too. Its module-scoped function would use a
# module stack for -StackName, so install the same caller-scope wrapper used by
# pushd. The module function above remains exported for module-qualified use.
$script:FswSetLocationBody = @'
    [CmdletBinding(DefaultParameterSetName = 'Path')]
    param(
        [Parameter(Position = 0, ParameterSetName = 'Path', ValueFromPipeline = $true)]
        [string]$Path,
        [Parameter(Mandatory = $true, ParameterSetName = 'LiteralPath')]
        [string]$LiteralPath,
        [Parameter(Mandatory = $true, ParameterSetName = 'Stack')]
        [string]$StackName,
        [switch]$PassThru,
        [switch]$UseTransaction
    )

    process {
        $forward = $PSBoundParameters
        $result = Resolve-ForwardSlashWindowsLocationTarget -BoundParameters $forward
        if ($null -eq $result) {
            Microsoft.PowerShell.Management\Set-Location @forward
            return
        }
        if ($result.Message) {
            if ($result.Informational) {
                Write-Host $result.Message
            } else {
                Write-Error $result.Message
            }
            return
        }
        [void]$forward.Remove('Path')
        $forward['LiteralPath'] = $result.Target
        Microsoft.PowerShell.Management\Set-Location @forward
    }
'@
Set-Item -Path function:global:Invoke-ForwardSlashWindowsSetLocation `
    -Value ([scriptblock]::Create($script:FswSetLocationBody))

# The pushd wrapper is installed as a *global* function built from an unbound
# script block, not as a module function, for two reasons that only affect
# PUSHD:
#   * Push-Location run inside a module pushes onto that module's location
#     stack, and the caller's popd never sees it.
#   * Its advanced parameters preserve PowerShell's native binding semantics;
#     the wrapper splats named parameters after replacing only a slash path.
# Running in the caller's session state fixes both, at the cost of calling
# only commands the global scope can see (hence the exported resolver above).
$script:FswPushLocationBody = @'
    [CmdletBinding(DefaultParameterSetName = 'Path')]
    param(
        [Parameter(Position = 0, ParameterSetName = 'Path', ValueFromPipeline = $true)]
        [string]$Path,
        [Parameter(Mandatory = $true, ParameterSetName = 'LiteralPath')]
        [string]$LiteralPath,
        [string]$StackName,
        [switch]$PassThru,
        [switch]$UseTransaction
    )

    process {
        $forward = $PSBoundParameters
        $result = Resolve-ForwardSlashWindowsLocationTarget -BoundParameters $forward
        if ($null -eq $result) {
            Microsoft.PowerShell.Management\Push-Location @forward
            return
        }
        if ($result.Message) {
            if ($result.Informational) {
                Write-Host $result.Message
            } else {
                Write-Error $result.Message
            }
            return
        }
        [void]$forward.Remove('Path')
        $forward['LiteralPath'] = $result.Target
        Microsoft.PowerShell.Management\Push-Location @forward
    }
'@
Set-Item -Path function:global:Invoke-ForwardSlashWindowsPushLocation `
    -Value ([scriptblock]::Create($script:FswPushLocationBody))

Set-Alias -Name dir -Value Invoke-ForwardSlashWindowsChildItem -Scope Global -Option AllScope -Force
Set-Alias -Name ls -Value Invoke-ForwardSlashWindowsChildItem -Scope Global -Option AllScope -Force
Set-Alias -Name cd -Value Invoke-ForwardSlashWindowsSetLocation -Scope Global -Option AllScope -Force
Set-Alias -Name chdir -Value Invoke-ForwardSlashWindowsSetLocation -Scope Global -Option AllScope -Force
Set-Alias -Name sl -Value Invoke-ForwardSlashWindowsSetLocation -Scope Global -Option AllScope -Force
Set-Alias -Name pushd -Value Invoke-ForwardSlashWindowsPushLocation -Scope Global -Option AllScope -Force
Export-ModuleMember -Function Invoke-ForwardSlashWindowsChildItem,
    Resolve-ForwardSlashWindowsLocationTarget
