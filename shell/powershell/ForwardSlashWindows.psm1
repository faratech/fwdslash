Set-StrictMode -Version 2.0

$script:FswController = Join-Path $PSScriptRoot 'fwdslash.exe'

function Test-ForwardSlashWindowsDisabled {
    # Direct RegistryKey read rather than the HKCU: provider, which costs
    # milliseconds on every call. Callers must reach this only after a slash
    # argument has been found, so an ordinary "dir" or "cd .." pays nothing.
    # This key path is the one literal the module cannot share with
    # include/fsw_user_protocol.h -- renaming the value there means editing
    # this line too (CLAUDE.md).
    $key = $null
    try {
        $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Software\ForwardSlashWindows\Settings')
        if ($null -eq $key) {
            return $false
        }
        $value = $key.GetValue('Disabled', 0)
        if ($null -eq $value) {
            return $false
        }
        return ([int]$value -ne 0)
    } catch {
        return $false
    } finally {
        if ($null -ne $key) {
            $key.Dispose()
        }
    }
}

function Resolve-ForwardSlashWindowsTarget {
    param([Parameter(Mandatory = $true)][string]$Path)

    # One spawn per slash argument. shell-resolve answers from the settings
    # snapshot alone -- no broker round trip, no filter-port probe -- and
    # returns the kind, the target and the distribution list together.
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

    if ($pathIndexes.Count -eq 0 -or (Test-ForwardSlashWindowsDisabled)) {
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
    param([object[]]$Arguments = @())

    $forward = @($Arguments)
    $slashIndex = -1
    foreach ($index in (Get-ForwardSlashWindowsPathIndex -Arguments $forward)) {
        $value = $forward[$index]
        if ($value -is [string] -and $value.StartsWith('/')) {
            $slashIndex = $index
            break
        }
    }
    if ($slashIndex -lt 0 -or (Test-ForwardSlashWindowsDisabled)) {
        return $null
    }
    $result = Resolve-ForwardSlashWindowsTarget -Path ([string]$forward[$slashIndex])
    if ($null -eq $result) {
        return $null
    }
    if ($result.Kind -eq 'root') {
        # Not one directory: say which distributions there are instead of
        # moving to the current drive's root.
        $result.Message = Get-ForwardSlashWindowsRootMessage -Distributions $result.Distributions
    }
    return $result
}

function Invoke-ForwardSlashWindowsSetLocation {
    $forward = @($args)
    $result = Resolve-ForwardSlashWindowsLocationTarget -Arguments $forward
    if ($null -eq $result) {
        Microsoft.PowerShell.Management\Set-Location @forward
        return
    }
    if ($result.Message) {
        # A rejected input or the bare root: never fall through to the
        # native cmdlet's misleading "Cannot find path 'C:\etc'".
        Write-Error $result.Message
        return
    }
    # PowerShell, unlike cmd.exe, can make a UNC path current.
    Microsoft.PowerShell.Management\Set-Location -LiteralPath $result.Target
}

# The pushd wrapper is installed as a *global* function built from an unbound
# script block, not as a module function, for two reasons that only affect
# PUSHD:
#   * Push-Location run inside a module pushes onto that module's location
#     stack, and the caller's popd never sees it.
#   * @args re-splats named parameters faithfully only from a simple
#     function's own $args; the same array collected by an advanced function
#     rebinds -LiteralPath as a positional value.
# Running in the caller's session state fixes both, at the cost of calling
# only commands the global scope can see (hence the exported resolver above).
$script:FswPushLocationBody = @'
    $forward = @($args)
    $result = Resolve-ForwardSlashWindowsLocationTarget -Arguments $forward
    if ($null -eq $result) {
        Microsoft.PowerShell.Management\Push-Location @forward
        return
    }
    if ($result.Message) {
        Write-Error $result.Message
        return
    }
    Microsoft.PowerShell.Management\Push-Location -LiteralPath $result.Target
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
    Invoke-ForwardSlashWindowsSetLocation,
    Resolve-ForwardSlashWindowsLocationTarget
