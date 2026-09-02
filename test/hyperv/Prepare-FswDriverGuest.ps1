[CmdletBinding()]
param(
    [switch]$InstallSigningTools
)

$ErrorActionPreference = 'Stop'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this script in an elevated PowerShell session inside the disposable driver VM.'
}

$model = (Get-CimInstance Win32_ComputerSystem).Model
if ($model -notmatch 'Virtual Machine') {
    throw 'Refusing to change boot signing policy outside a virtual machine.'
}

if ($InstallSigningTools) {
    winget install --id Microsoft.WindowsSDK.10.0.28000 --exact --silent --accept-package-agreements --accept-source-agreements
    if ($LASTEXITCODE -ne 0) {
        throw "Windows SDK installer exited with $LASTEXITCODE."
    }
}

bcdedit.exe /set testsigning on
if ($LASTEXITCODE -ne 0) {
    throw 'Unable to enable test signing. Confirm that Secure Boot is disabled for the guest.'
}
Write-Host 'Test signing is configured. Restart the guest before installing FswFilter.'
