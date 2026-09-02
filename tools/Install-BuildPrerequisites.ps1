[CmdletBinding(SupportsShouldProcess)]
param()

$ErrorActionPreference = 'Stop'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this prerequisite installer in an elevated PowerShell session.'
}

if ($PSCmdlet.ShouldProcess('Visual Studio Build Tools', 'Install C++ and ARM64 workloads')) {
    winget install --id Microsoft.VisualStudio.2022.BuildTools --exact --silent --accept-package-agreements --accept-source-agreements --override '--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --add Microsoft.VisualStudio.Component.VC.Tools.ARM64 --includeRecommended'
    if ($LASTEXITCODE -ne 0) { throw "Build Tools installer exited with $LASTEXITCODE" }
}

if ($PSCmdlet.ShouldProcess('Windows SDK 10.0.28000', 'Install')) {
    winget install --id Microsoft.WindowsSDK.10.0.28000 --exact --silent --accept-package-agreements --accept-source-agreements
    if ($LASTEXITCODE -ne 0) { throw "Windows SDK installer exited with $LASTEXITCODE" }
}
