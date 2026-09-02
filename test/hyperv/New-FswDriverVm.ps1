[CmdletBinding(SupportsShouldProcess)]
param(
    [Parameter(Mandatory)]
    [ValidateScript({ Test-Path -LiteralPath $_ -PathType Leaf })]
    [string]$IsoPath,
    [string]$VmName = 'ForwardSlashWindows-DriverLab',
    [string]$VmRoot = 'C:\ProgramData\ForwardSlashWindows\VMs'
)

$ErrorActionPreference = 'Stop'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this script in an elevated PowerShell session.'
}
if ($env:PROCESSOR_ARCHITECTURE -ne 'ARM64') {
    throw 'This checked-in VM profile is specifically for the ARM64 development host.'
}
if (Get-VM -Name $VmName -ErrorAction SilentlyContinue) {
    throw "A VM named '$VmName' already exists. It was not modified."
}

$resolvedRoot = [IO.Path]::GetFullPath($VmRoot)
$expectedRoot = [IO.Path]::GetFullPath('C:\ProgramData\ForwardSlashWindows\VMs')
$expectedPrefix = $expectedRoot.TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
if (-not $resolvedRoot.Equals($expectedRoot, [StringComparison]::OrdinalIgnoreCase) -and
    -not $resolvedRoot.StartsWith($expectedPrefix, [StringComparison]::OrdinalIgnoreCase)) {
    throw "VM root must remain under $expectedRoot"
}

$vhd = Join-Path $resolvedRoot "$VmName\$VmName.vhdx"
if ($PSCmdlet.ShouldProcess($VmName, 'Create isolated Hyper-V driver lab')) {
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $vhd) | Out-Null
    New-VHD -Path $vhd -Dynamic -SizeBytes 80GB | Out-Null
    $vm = New-VM -Name $VmName -Generation 2 -MemoryStartupBytes 8GB -VHDPath $vhd -SwitchName 'Default Switch'
    Set-VM -VM $vm -ProcessorCount 4 -DynamicMemory -MemoryMinimumBytes 4GB -MemoryMaximumBytes 12GB -AutomaticCheckpointsEnabled $true
    Set-VMFirmware -VM $vm -EnableSecureBoot Off
    Add-VMDvdDrive -VM $vm -Path ([IO.Path]::GetFullPath($IsoPath)) | Out-Null
    $dvd = Get-VMDvdDrive -VM $vm
    Set-VMFirmware -VM $vm -FirstBootDevice $dvd
    Write-Host "Created $VmName. Complete Windows setup, then run Prepare-FswDriverGuest.ps1 inside the VM."
}
