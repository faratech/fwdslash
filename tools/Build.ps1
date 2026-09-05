[CmdletBinding()]
param(
    # 'All' means both driver architectures, x64 and ARM64; the kernel driver
    # has no x86 target, so x86 stays a user-mode-only value.
    [ValidateSet('x86', 'x64', 'ARM64', 'All')]
    [string]$Architecture = $(if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'ARM64' } else { 'x64' }),
    [ValidateSet('Debug', 'Release')]
    [string]$Configuration = 'Debug',
    [switch]$Driver
)

$ErrorActionPreference = 'Stop'

# Named distinctly from $Architecture: PowerShell variable names are
# case-insensitive, so reusing the name inside the loop would re-wrap each
# value through the parameter's [string] constraint.
$userModeTargets = if ($Architecture -eq 'All') { @('x64', 'ARM64') } else { @($Architecture) }
foreach ($userModeTarget in $userModeTargets) {
    & (Join-Path $PSScriptRoot 'Build-UserMode.ps1') -Architecture $userModeTarget -Configuration $Configuration
}

if ($Driver) {
    if ($Architecture -notin 'x64', 'ARM64', 'All') {
        throw 'The kernel driver supports x64 and ARM64 only.'
    }
    & (Join-Path $PSScriptRoot 'Build-Driver.ps1') -Architecture $Architecture -Configuration $Configuration
}
