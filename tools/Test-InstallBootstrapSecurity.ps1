# Static contract test for the executable-download portion of Install-fwdslash.
# It never downloads, registers, or launches a package or runtime installer.
[CmdletBinding()]
param(
    [string]$InstallerScript
)

$ErrorActionPreference = 'Stop'
if ([string]::IsNullOrWhiteSpace($InstallerScript)) {
    $InstallerScript = Join-Path $PSScriptRoot 'Install-fwdslash.ps1'
}
$parseErrors = $null
[void][System.Management.Automation.Language.Parser]::ParseFile(
    (Resolve-Path -LiteralPath $InstallerScript), [ref]$null, [ref]$parseErrors)
if ($parseErrors.Count) {
    throw ($parseErrors | ForEach-Object { $_.ToString() } | Out-String)
}

$source = Get-Content -LiteralPath $InstallerScript -Raw
$nativeType = [regex]::Match($source, "Add-Type -TypeDefinition @'(?<code>[\s\S]*?)'@")
if (-not $nativeType.Success) {
    throw 'The no-follow Win32 interop definition is missing.'
}
# Compile the exact embedded C# without invoking the installer script.  This
# catches marshaling/signature regressions that PowerShell parsing cannot see.
Add-Type -TypeDefinition $nativeType.Groups['code'].Value

$requiredFragments = @(
    'FILE_FLAG_OPEN_REPARSE_POINT',
    'GetFileInformationByHandle',
    'FILE_SHARE_READ | FILE_SHARE_WRITE',
    'Lock-ForwardSlashWindowsDirectoryChain',
    'Open-ForwardSlashWindowsNoFollowPath $runtimeFile',
    'Assert-TrustedSignature $runtimeFile',
    "'Windows App SDK', 'Microsoft Windows App Runtime'",
    'Start-Process -FilePath $runtimeFile',
    '$runtimeLock.Dispose()'
)
foreach ($fragment in $requiredFragments) {
    if (-not $source.Contains($fragment)) {
        throw "Bootstrap security contract is missing: $fragment"
    }
}

$runtimeLockOffset = $source.IndexOf('Open-ForwardSlashWindowsNoFollowPath $runtimeFile')
$signatureOffset = $source.IndexOf('Assert-TrustedSignature $runtimeFile')
$launchOffset = $source.IndexOf('Start-Process -FilePath $runtimeFile')
if ($runtimeLockOffset -lt 0 -or $runtimeLockOffset -gt $signatureOffset -or $signatureOffset -gt $launchOffset) {
    throw 'The runtime installer is not locked, verified, and then launched in that order.'
}

Write-Host 'PASS: bootstrap runtime installer uses no-follow staging, locked path ancestry, and pre-launch verification.'
