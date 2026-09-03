Set-StrictMode -Version 2.0

$script:FswController = Join-Path $PSScriptRoot 'fwdslash.exe'
$script:FswSettingsKey = 'HKCU:\Software\ForwardSlashWindows\Settings'

function Test-ForwardSlashWindowsDisabled {
    try {
        return [int](Get-ItemPropertyValue -LiteralPath $script:FswSettingsKey -Name Disabled -ErrorAction Stop) -ne 0
    } catch {
        return $false
    }
}

function Resolve-ForwardSlashWindowsPath {
    param([Parameter(Mandatory = $true)][string]$Path)

    $resolved = & $script:FswController resolve $Path 2>$null
    if ($LASTEXITCODE -ne 0 -or -not $resolved) {
        return $null
    }
    return [string]($resolved | Select-Object -First 1)
}

function Get-ForwardSlashWindowsRoots {
    $status = & $script:FswController status --json 2>$null
    if ($LASTEXITCODE -ne 0 -or -not $status) {
        return
    }
    try {
        $data = $status | ConvertFrom-Json
        foreach ($distribution in @($data.distributions)) {
            [pscustomobject]@{
                PSTypeName   = 'ForwardSlashWindows.Distribution'
                Name         = "/$distribution"
                FullName     = "\\wsl.localhost\$distribution"
                Distribution = [string]$distribution
            }
        }
    } catch {
        Write-Error -ErrorRecord $_
    }
}

function Invoke-ForwardSlashWindowsChildItem {
    if (Test-ForwardSlashWindowsDisabled) {
        Microsoft.PowerShell.Management\Get-ChildItem @args
        return
    }

    $forward = @($args)
    $pathIndexes = New-Object System.Collections.Generic.List[int]
    $expectPath = $false
    for ($index = 0; $index -lt $forward.Count; $index++) {
        $argument = $forward[$index]
        if ($expectPath) {
            $pathIndexes.Add($index)
            $expectPath = $false
            continue
        }
        if ($argument -is [string] -and
            ($argument -eq '-Path' -or $argument -eq '-LiteralPath')) {
            $expectPath = $true
            continue
        }
        if ($argument -is [string] -and $argument.StartsWith('/')) {
            $pathIndexes.Add($index)
        }
    }

    if ($pathIndexes.Count -eq 0) {
        Microsoft.PowerShell.Management\Get-ChildItem @forward
        return
    }

    $hasBareRoot = $false
    foreach ($index in $pathIndexes) {
        $value = $forward[$index]
        if ($value -is [string]) {
            $resolved = Resolve-ForwardSlashWindowsPath -Path $value
            if (-not $resolved) {
                Microsoft.PowerShell.Management\Get-ChildItem @forward
                return
            }
            if ($resolved -eq '\\wsl.localhost') {
                # Bare "/" resolved to the provider root: list distributions.
                $hasBareRoot = $true
            }
            $forward[$index] = $resolved
            continue
        }
        if ($value -is [System.Collections.IEnumerable]) {
            $replacement = @()
            foreach ($item in $value) {
                if ($item -is [string] -and $item.StartsWith('/')) {
                    $resolved = Resolve-ForwardSlashWindowsPath -Path $item
                    if (-not $resolved) {
                        Microsoft.PowerShell.Management\Get-ChildItem @forward
                        return
                    }
                    if ($resolved -eq '\\wsl.localhost') {
                        $hasBareRoot = $true
                    }
                    $replacement += $resolved
                } else {
                    $replacement += $item
                }
            }
            $forward[$index] = $replacement
        }
    }

    if ($hasBareRoot) {
        if ($pathIndexes.Count -ne 1) {
            throw "Bare '/' cannot be combined with other Get-ChildItem arguments. Use 'fwdslash list /' for advanced root queries."
        }
        Get-ForwardSlashWindowsRoots
        return
    }

    Microsoft.PowerShell.Management\Get-ChildItem @forward
}

Set-Alias -Name dir -Value Invoke-ForwardSlashWindowsChildItem -Scope Global -Option AllScope -Force
Set-Alias -Name ls -Value Invoke-ForwardSlashWindowsChildItem -Scope Global -Option AllScope -Force
Export-ModuleMember -Function Invoke-ForwardSlashWindowsChildItem
