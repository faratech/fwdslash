[CmdletBinding()]
param(
    [ValidateSet('x86', 'x64', 'ARM64')]
    [string]$Architecture = $(if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'ARM64' } else { 'x64' }),
    [ValidateSet('Debug', 'Release')]
    [string]$Configuration = 'Debug',
    [switch]$Driver
)

$ErrorActionPreference = 'Stop'
& (Join-Path $PSScriptRoot 'Build-UserMode.ps1') -Architecture $Architecture -Configuration $Configuration

if ($Driver) {
    if ($Architecture -notin 'x64', 'ARM64') {
        throw 'The kernel driver supports x64 and ARM64 only.'
    }
    & (Join-Path $PSScriptRoot 'Build-Driver.ps1') -Architecture $Architecture -Configuration $Configuration
}
