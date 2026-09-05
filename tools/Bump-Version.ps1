<#
.SYNOPSIS
    Windows wrapper around tools/bump_version.py.

.DESCRIPTION
    Finds a Python 3 interpreter (`py -3`, `python3`, then `python`) and forwards
    every argument to tools/bump_version.py verbatim. It carries no logic of its
    own: the registered version literals, the match counts and the --check mode
    all live in the Python script, so the two entry points can never disagree.

.EXAMPLE
    .\tools\Bump-Version.ps1 --check

.EXAMPLE
    .\tools\Bump-Version.ps1 0.0.4

.EXAMPLE
    .\tools\Bump-Version.ps1 0.0.4 --dry-run
#>
[CmdletBinding()]
param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$Arguments
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# $PSScriptRoot is empty while parameter defaults are evaluated under
# [CmdletBinding()], so repo-relative paths are resolved here in the body.
$script = Join-Path $PSScriptRoot 'bump_version.py'
if (-not (Test-Path -LiteralPath $script)) {
    throw "Missing $script"
}

# `py -3` is the launcher shipped with the python.org installer; python3/python
# cover Store and manually installed interpreters. The launcher goes first
# because a bare `python` on Windows is often the Store's stub shim, which
# opens the Store rather than running anything.
$candidates = @(
    @{ Command = 'py';      Prefix = @('-3') },
    @{ Command = 'python3'; Prefix = @() },
    @{ Command = 'python';  Prefix = @() }
)

foreach ($candidate in $candidates) {
    $resolved = Get-Command $candidate.Command -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if (-not $resolved) { continue }

    & $resolved.Source @($candidate.Prefix) $script @Arguments
    exit $LASTEXITCODE
}

throw 'No Python 3 interpreter found (tried "py -3", "python3", "python"). Install Python 3 or run tools/bump_version.py directly.'
