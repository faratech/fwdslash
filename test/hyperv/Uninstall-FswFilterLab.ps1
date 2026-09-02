[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this script in an elevated PowerShell session inside the disposable driver VM.'
}
$model = (Get-CimInstance Win32_ComputerSystem).Model
if ($model -notmatch 'Virtual Machine') {
    throw 'Lab driver removal is VM-only; there is no physical-host override.'
}

fltmc.exe unload FswFilter
if ($LASTEXITCODE -ne 0) { Write-Warning "FltMC unload exited with $LASTEXITCODE." }
sc.exe delete FswFilter | Out-Host
if ($LASTEXITCODE -ne 0) { Write-Warning "Service deletion exited with $LASTEXITCODE." }
Write-Host 'The service was unloaded and removed. Remove the published INF with pnputil /delete-driver if one remains in the driver store.'
