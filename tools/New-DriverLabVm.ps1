#Requires -Version 5.1
<#
.SYNOPSIS
    Creates the checkpointed Hyper-V guest that is the ONLY place the fwdslash
    minifilter may ever be loaded.

.DESCRIPTION
    LAB GUEST ONLY. Everything this script sets up exists so an unsigned,
    test-signed kernel driver can be loaded without endangering a real machine.
    Never run the driver, Bootstrap-DriverLabGuest.ps1 or Test-Driver.ps1 on a
    physical workstation: they turn test signing on, disable Secure Boot,
    enable Driver Verifier and load unsigned kernel code. The guest is
    disposable; a workstation is not.

    Runs on the HOST, elevated. Creates a Generation 2 Windows 11 VM with:
      * Secure Boot OFF   - `bcdedit /set testsigning on` is refused while
                            Secure Boot is on, so the lab cannot work with it.
      * 4 virtual processors, optional nested virtualization for WSL.
      * The installation ISO mounted and first in the boot order.
      * Guest Service Interface enabled so Copy-VMFile can push the driver
        package in without a network share.
      * Standard (not production) checkpoints and no automatic checkpoints,
        so a restore rolls memory back too after a bugcheck.

    The script deliberately stops before Windows Setup. Creating the 'clean-os'
    checkpoint is an operator step, because it has to happen AFTER OOBE.

    Idempotent: if the VM already exists the script prints its state and exits 0
    without touching it.

.PARAMETER Name
    VM name. Default 'fwdslash-lab-arm64'.

.PARAMETER IsoPath
    Windows 11 installation ISO. ARM64 on an ARM64 host, x64 on an x64 host -
    Hyper-V cannot run a guest of a foreign architecture.

.PARAMETER MemoryStartupBytes
    Startup memory. Default 4GB.

.PARAMETER VhdSizeBytes
    Dynamic VHDX maximum size. Default 64GB.

.PARAMETER Switch
    Virtual switch to attach. Default 'Default Switch'. Pass '' for no network.

.PARAMETER ExposeVirtualization
    Enables nested virtualization (Set-VMProcessor
    -ExposeVirtualizationExtensions) so WSL2 can run inside the guest. On x64
    hosts this has worked for years. On ARM64 hosts nested virtualization is
    only available on recent silicon and recent Windows builds; if the call
    fails the script warns and continues, and the harness must then be run with
    -FakeShare instead of a real distribution.

.EXAMPLE
    .\tools\New-DriverLabVm.ps1 -IsoPath D:\iso\Win11_ARM64.iso -ExposeVirtualization
#>
[CmdletBinding()]
param(
    [string]$Name = 'fwdslash-lab-arm64',

    [Parameter(Mandatory = $true)]
    [string]$IsoPath,

    [long]$MemoryStartupBytes = 4GB,

    [long]$VhdSizeBytes = 64GB,

    [string]$Switch = 'Default Switch',

    [switch]$ExposeVirtualization
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# $PSScriptRoot is empty while parameter defaults are bound under
# [CmdletBinding()], so repo-relative values are resolved here, in the body.
$repo = Split-Path -Parent $PSScriptRoot
$switchName = $Switch

function Write-Step {
    param([string]$Message)
    Write-Host "  $Message"
}

function Test-Elevated {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

Write-Host 'fwdslash driver lab - guest creation (host side)'
Write-Host '------------------------------------------------'

if (-not (Test-Elevated)) {
    throw 'Hyper-V VM creation requires an elevated shell. Re-run as Administrator.'
}

$vmms = Get-Service -Name vmms -ErrorAction SilentlyContinue
if ($null -eq $vmms) {
    throw 'The Hyper-V Virtual Machine Management service (vmms) is not installed. Enable the Hyper-V feature first.'
}
if ($vmms.Status -ne 'Running') {
    throw "The Hyper-V Virtual Machine Management service is $($vmms.Status); start it before creating the lab guest."
}
if (-not (Get-Command -Name New-VM -ErrorAction SilentlyContinue)) {
    throw 'The Hyper-V PowerShell module is not available. Install the Hyper-V Management Tools feature.'
}

$existing = Get-VM -Name $Name -ErrorAction SilentlyContinue
if ($null -ne $existing) {
    Write-Host "VM '$Name' already exists - nothing to do."
    Write-Host "  State           : $($existing.State)"
    Write-Host "  Generation      : $($existing.Generation)"
    Write-Host "  Processors      : $($existing.ProcessorCount)"
    Write-Host "  Memory (startup): $([math]::Round($existing.MemoryStartup / 1GB, 2)) GB"
    $snapshots = @(Get-VMSnapshot -VMName $Name -ErrorAction SilentlyContinue)
    if ($snapshots.Count -eq 0) {
        Write-Host '  Checkpoints     : none yet - create clean-os after OOBE (see below).'
    } else {
        Write-Host "  Checkpoints     : $(($snapshots | ForEach-Object { $_.Name }) -join ', ')"
    }
    Write-Host ''
    Write-Host "Restore the clean baseline before every run:  Restore-VMSnapshot -VMName $Name -Name clean-os -Confirm:`$false"
    exit 0
}

if (-not (Test-Path -LiteralPath $IsoPath)) {
    throw 'The ISO path given does not exist.'
}

$hostArchitecture = $env:PROCESSOR_ARCHITECTURE
Write-Host "Host architecture: $hostArchitecture"
Write-Host "A Hyper-V guest runs the host's architecture. Use a Windows 11 $hostArchitecture ISO;"
Write-Host 'an x64 lab needs a separate x64 machine.'
Write-Host ''

$vhdRoot = (Get-VMHost).VirtualHardDiskPath
if ([string]::IsNullOrWhiteSpace($vhdRoot)) {
    $vhdRoot = Join-Path $env:PUBLIC 'Documents\Hyper-V\Virtual Hard Disks'
}
if (-not (Test-Path -LiteralPath $vhdRoot)) {
    New-Item -ItemType Directory -Force -Path $vhdRoot | Out-Null
}
$vhdPath = Join-Path $vhdRoot "$Name.vhdx"
if (Test-Path -LiteralPath $vhdPath) {
    throw "A virtual hard disk for '$Name' already exists but the VM does not. Remove the stale disk or choose another -Name."
}

Write-Host "Creating VM '$Name'..."
$newVmArguments = @{
    Name               = $Name
    Generation         = 2
    MemoryStartupBytes = $MemoryStartupBytes
    NewVHDPath         = $vhdPath
    NewVHDSizeBytes    = $VhdSizeBytes
}
if (-not [string]::IsNullOrWhiteSpace($switchName)) {
    $availableSwitch = Get-VMSwitch -Name $switchName -ErrorAction SilentlyContinue
    if ($null -eq $availableSwitch) {
        Write-Warning "Virtual switch '$switchName' was not found; the guest is created without a network adapter connection."
    } else {
        $newVmArguments['SwitchName'] = $switchName
    }
}
$vm = New-VM @newVmArguments
Write-Step "created, generation $($vm.Generation)"

# Secure Boot must be off: the boot loader refuses `bcdedit /set testsigning on`
# while Secure Boot is enforcing, and without test signing no lab driver loads.
Set-VMFirmware -VMName $Name -EnableSecureBoot Off
Write-Step 'Secure Boot disabled (required for test signing)'

Set-VMProcessor -VMName $Name -Count 4
Write-Step 'processor count set to 4'

if ($ExposeVirtualization) {
    try {
        Set-VMProcessor -VMName $Name -ExposeVirtualizationExtensions $true
        Write-Step 'nested virtualization enabled (WSL2 can run inside the guest)'
    } catch {
        Write-Warning 'Nested virtualization could not be enabled on this host. ARM64 hosts support it only on recent hardware and Windows builds.'
        Write-Warning 'Run Bootstrap-DriverLabGuest.ps1 -FakeShare and Test-Driver.ps1 -FakeShare instead of installing WSL in the guest.'
    }
}

Add-VMDvdDrive -VMName $Name -Path $IsoPath
$dvd = Get-VMDvdDrive -VMName $Name | Select-Object -First 1
Set-VMFirmware -VMName $Name -FirstBootDevice $dvd
Write-Step 'installation ISO attached and set as the first boot device'

Enable-VMIntegrationService -VMName $Name -Name 'Guest Service Interface'
Write-Step 'Guest Service Interface enabled (Copy-VMFile works)'

# Standard checkpoints capture memory as well as disk. After a bugcheck a
# production checkpoint would restore a clean-shutdown image and hide the state
# the crash happened in.
Set-VM -VMName $Name -CheckpointType Standard -AutomaticCheckpointsEnabled $false
Write-Step 'standard checkpoints, automatic checkpoints off'

Write-Host ''
Write-Host 'VM created. The rest is manual, in this order:'
Write-Host ''
Write-Host "  1. Start it and install Windows:"
Write-Host "       Start-VM -Name $Name"
Write-Host "       vmconnect.exe localhost $Name"
Write-Host '     Complete Setup and OOBE. A local account is fine and is easier to'
Write-Host '     script against than a Microsoft account.'
Write-Host ''
Write-Host '  2. Detach the ISO so the guest does not boot back into Setup:'
Write-Host "       Get-VMDvdDrive -VMName $Name | Set-VMDvdDrive -Path `$null"
Write-Host ''
Write-Host '  3. Take the baseline checkpoint - this is the state every harness run'
Write-Host '     starts from:'
Write-Host "       Checkpoint-VM -Name $Name -SnapshotName clean-os"
Write-Host ''
Write-Host '  4. Copy the lab package in (Guest Service Interface, no network share'
Write-Host '     and no file sharing with the host required):'
Write-Host "       Copy-VMFile -Name $Name ``"
Write-Host '           -SourcePath <host path to fwdslash-filter-*.zip> ``'
Write-Host "           -DestinationPath 'C:\FswLab\fwdslash-filter.zip' ``"
Write-Host '           -CreateFullPath -FileSource Host'
Write-Host '     Repeat for tools\Bootstrap-DriverLabGuest.ps1 and tools\Test-Driver.ps1.'
Write-Host ''
Write-Host '  5. Inside the guest, elevated:'
Write-Host '       C:\FswLab\Bootstrap-DriverLabGuest.ps1 -CertificatePath C:\FswLab\fwdslash-lab.cer -FakeShare -Reboot'
Write-Host '       C:\FswLab\Test-Driver.ps1 -PackageZip C:\FswLab\fwdslash-filter.zip -FakeShare'
Write-Host ''
Write-Host '  6. After each run, roll back before the next one:'
Write-Host "       Restore-VMSnapshot -VMName $Name -Name clean-os -Confirm:`$false"
Write-Host ''
Write-Host "Runbook: $(Join-Path $repo 'docs\driver-lab.md')"
exit 0
