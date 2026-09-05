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

    Runs on the HOST, elevated. Builds a bootable Windows VHDX directly - by
    applying install.wim from the given ISO and running bcdboot - and stamps
    it with an unattend.xml before ever booting it, then creates a
    Generation 2 VM around that disk and boots straight to a ready desktop.
    There is no interactive Setup/OOBE step: `vmconnect` is never needed.

    Why not the declarative <UserAccounts>/<AutoLogon>/<FirstLogonCommands>
    unattend elements most guides recommend: on this DISM-apply + bcdboot
    boot path (Windows never runs its own installer, so most of the setup
    engine that processes those elements never runs either) they silently
    no-op. <Group>Administrators</Group>, <AdministratorPassword> and
    <FirstLogonCommands> were all tried and all three did nothing, across
    three separate rebuilds; forcing full oobeSystem processing by dropping
    SkipMachineOOBE/SkipUserOOBE made it worse (a real interactive OOBE
    timeout, producing a spurious "defaultuser0" account) without fixing any
    of them. The one thing proven reliable is the **specialize** pass
    (ComputerName is always honored there, and it runs as SYSTEM before OOBE
    starts and before any account exists), so the lab account, its
    Administrators membership, LocalAccountTokenFilterPolicy and
    AutoAdminLogon are all provisioned **imperatively** via
    `RunSynchronousCommand` in the specialize pass - `net user`, `net
    localgroup`, and `reg add` - bypassing the declarative machinery
    entirely. oobeSystem is kept only for `SkipMachineOOBE`/`SkipUserOOBE`, so
    OOBE short-circuits straight to the specialize-configured autologon
    instead of re-triggering the defaultuser0 regression.

    The VM itself: Generation 2, Secure Boot OFF (`bcdedit /set testsigning
    on` is refused while Secure Boot is on, so the lab cannot work with it),
    4 virtual processors, optional nested virtualization for WSL2, Guest
    Service Interface enabled so Copy-VMFile can push the driver package in
    without a network share, and standard (not production) checkpoints with
    automatic checkpoints off, so a restore rolls memory back too after a
    bugcheck.

    After the VM is created and started, this script polls PowerShell Direct
    (the guest's own credentials, written to
    out\lab\guest-credentials.txt) until it succeeds - proof the unattended
    first-boot path actually reached a usable desktop - and then takes the
    'clean-os' checkpoint itself, since there is no OOBE step left for an
    operator to finish first.

    Idempotent: if the VM already exists the script prints its state and exits
    0 without touching it.

.PARAMETER Name
    VM name. Default 'fswlab-arm64' - the name every other lab script
    (Bootstrap-DriverLabGuest.ps1's callers, Test-Driver.ps1's callers, the
    out\lab gate-runner scripts) assumes.

.PARAMETER IsoPath
    Windows installation ISO. ARM64 on an ARM64 host, x64 on an x64 host -
    Hyper-V cannot run a guest of a foreign architecture. Required unless
    -DryRunUnattend is given.

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
    only available on recent silicon and recent Windows builds, and the
    failure mode differs from x64: `Set-VMProcessor` itself does not throw,
    only `Start-VM` does, one step later - this script catches that and
    retries with virtualization extensions off. Either way, when it is
    unavailable the harness must be run with -FakeShare instead of a real
    distribution.

.PARAMETER GuestComputerName
    The guest's computer name, set in the unattend specialize pass. Default
    'fswlab'.

.PARAMETER GuestAccountName
    The local administrator account created imperatively in the specialize
    pass. Default 'fswlab' - every lab script that reads
    out\lab\guest-credentials.txt expects this account.

.PARAMETER ImageIndex
    The install.wim image index DISM applies. Default 1. Multi-edition
    consumer ISOs put different editions at different indices; check with
    `dism /Get-WimInfo /WimFile:<path>` if index 1 is not what you want.

.PARAMETER EfiDriveLetter
.PARAMETER WindowsDriveLetter
    Drive letters temporarily assigned to the new VHDX's EFI System Partition
    and Windows partition while this script builds the image (defaults 'S'
    and 'W', matching the guest this lab was validated against). The script
    refuses to proceed if either letter is already in use on the host, rather
    than silently colliding with an existing mapped drive.

.PARAMETER BootTimeoutMinutes
    How long to wait for PowerShell Direct to succeed after first boot before
    giving up. Default 30 - first boot runs the specialize pass and several
    of the usual first-logon housekeeping tasks before autologon completes.

.PARAMETER DryRunUnattend
    Generates the unattend.xml this script would stamp into the VHDX - with a
    freshly generated password, so escaping is exercised for real - and
    writes it to -DryRunOutputPath without touching Hyper-V, DISM, or any
    disk, and without requiring elevation or -IsoPath. For auditing the
    provisioning commands, or as a smoke test after editing this script.

.PARAMETER DryRunOutputPath
    Where -DryRunUnattend writes the preview unattend.xml. Default
    out\lab\preview-unattend.xml (repo-relative).

.EXAMPLE
    .\tools\New-DriverLabVm.ps1 -IsoPath D:\iso\Win11_ARM64.iso -ExposeVirtualization

.EXAMPLE
    .\tools\New-DriverLabVm.ps1 -DryRunUnattend
#>
[CmdletBinding()]
param(
    [string]$Name = 'fswlab-arm64',

    [string]$IsoPath,

    [long]$MemoryStartupBytes = 4GB,

    [long]$VhdSizeBytes = 64GB,

    [string]$Switch = 'Default Switch',

    [switch]$ExposeVirtualization,

    [string]$GuestComputerName = 'fswlab',

    [string]$GuestAccountName = 'fswlab',

    [int]$ImageIndex = 1,

    [string]$EfiDriveLetter = 'S',

    [string]$WindowsDriveLetter = 'W',

    [int]$BootTimeoutMinutes = 30,

    [switch]$DryRunUnattend,

    [string]$DryRunOutputPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# $PSScriptRoot is empty while parameter defaults are bound under
# [CmdletBinding()], so repo-relative values are resolved here, in the body.
$repo = Split-Path -Parent $PSScriptRoot
$switchName = $Switch
$labDir = Join-Path $repo 'out\lab'
if ([string]::IsNullOrWhiteSpace($DryRunOutputPath)) {
    $DryRunOutputPath = Join-Path $labDir 'preview-unattend.xml'
}
$credPath = Join-Path $labDir 'guest-credentials.txt'

function Write-Step {
    param([string]$Message)
    Write-Host "  $Message"
}

function Test-Elevated {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

# A cryptographically random password with at least one character from each
# of four classes, then shuffled so the guaranteed characters are not always
# in the same four positions.
function New-LabPassword {
    param([int]$Length = 20)
    $sets = @(
        'ABCDEFGHJKLMNPQRSTUVWXYZ',
        'abcdefghijkmnopqrstuvwxyz',
        '23456789',
        '!@#-_='
    )
    $rng = [System.Security.Cryptography.RandomNumberGenerator]::Create()
    $getIndex = {
        param($max)
        $b = New-Object byte[] 4
        $rng.GetBytes($b)
        return [Math]::Abs([BitConverter]::ToInt32($b, 0)) % $max
    }
    $passwordChars = New-Object System.Collections.Generic.List[char]
    foreach ($s in $sets) { $passwordChars.Add($s[(& $getIndex $s.Length)]) }
    $all = -join $sets
    while ($passwordChars.Count -lt $Length) {
        $passwordChars.Add($all[(& $getIndex $all.Length)])
    }
    for ($i = $passwordChars.Count - 1; $i -gt 0; $i--) {
        $j = & $getIndex ($i + 1)
        $tmp = $passwordChars[$i]; $passwordChars[$i] = $passwordChars[$j]; $passwordChars[$j] = $tmp
    }
    return -join $passwordChars
}

# The unattend.xml this lab was validated with: everything that matters is in
# the specialize pass, imperatively, because the declarative equivalents
# (Group, AdministratorPassword, FirstLogonCommands) silently no-op on the
# DISM-apply + bcdboot boot path - see the script header for the full story.
# oobeSystem only skips OOBE so autologon takes over immediately.
function New-LabUnattendXml {
    param(
        [Parameter(Mandatory = $true)][string]$ComputerName,
        [Parameter(Mandatory = $true)][string]$AccountName,
        [Parameter(Mandatory = $true)][string]$Password,
        [Parameter(Mandatory = $true)][string]$ProcessorArchitecture
    )
    # net/reg command lines go inside an XML element; only the double-quote
    # needs XML-escaping help beyond what here-string interpolation already
    # does for &lt;/&gt;/&amp; when this is serialized - PowerShell's [xml]
    # parser (used below to validate the result) would fail loudly if this
    # were wrong, which is exactly what -DryRunUnattend is for.
    $escapedPassword = $Password -replace '"', '""'
    return @"
<?xml version="1.0" encoding="utf-8"?>
<unattend xmlns="urn:schemas-microsoft-com:unattend">
  <settings pass="specialize">
    <component name="Microsoft-Windows-Shell-Setup" processorArchitecture="$ProcessorArchitecture" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
      <ComputerName>$ComputerName</ComputerName>
    </component>
    <component name="Microsoft-Windows-Deployment" processorArchitecture="$ProcessorArchitecture" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
      <RunSynchronous>
        <RunSynchronousCommand wcm:action="add">
          <Order>1</Order>
          <Path>net user $AccountName "$escapedPassword" /add /y</Path>
          <Description>Create the lab account imperatively (declarative LocalAccounts/Group has proven unreliable on this boot path)</Description>
        </RunSynchronousCommand>
        <RunSynchronousCommand wcm:action="add">
          <Order>2</Order>
          <Path>net localgroup Administrators $AccountName /add</Path>
          <Description>Grant Administrators imperatively (declarative Group element silently no-op'd three times)</Description>
        </RunSynchronousCommand>
        <RunSynchronousCommand wcm:action="add">
          <Order>3</Order>
          <Path>reg add "HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System" /v LocalAccountTokenFilterPolicy /t REG_DWORD /d 1 /f</Path>
          <Description>Full (unfiltered) token for a non-RID-500 local admin over PowerShell Direct / network-style logons</Description>
        </RunSynchronousCommand>
        <RunSynchronousCommand wcm:action="add">
          <Order>4</Order>
          <Path>reg add "HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon" /v AutoAdminLogon /t REG_SZ /d 1 /f</Path>
          <Description>AutoAdminLogon (imperative, independent of the declarative AutoLogon element)</Description>
        </RunSynchronousCommand>
        <RunSynchronousCommand wcm:action="add">
          <Order>5</Order>
          <Path>reg add "HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon" /v DefaultUserName /t REG_SZ /d $AccountName /f</Path>
          <Description>AutoAdminLogon username</Description>
        </RunSynchronousCommand>
        <RunSynchronousCommand wcm:action="add">
          <Order>6</Order>
          <Path>reg add "HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon" /v DefaultPassword /t REG_SZ /d "$escapedPassword" /f</Path>
          <Description>AutoAdminLogon password</Description>
        </RunSynchronousCommand>
        <RunSynchronousCommand wcm:action="add">
          <Order>7</Order>
          <Path>reg add "HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon" /v AutoLogonCount /t REG_DWORD /d 999 /f</Path>
          <Description>Keep autologon across the many reboots a gate run needs</Description>
        </RunSynchronousCommand>
      </RunSynchronous>
    </component>
  </settings>
  <settings pass="oobeSystem">
    <component name="Microsoft-Windows-Shell-Setup" processorArchitecture="$ProcessorArchitecture" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
      <OOBE>
        <HideEULAPage>true</HideEULAPage>
        <HideOEMRegistrationScreen>true</HideOEMRegistrationScreen>
        <HideOnlineAccountScreens>true</HideOnlineAccountScreens>
        <HideWirelessSetupInOOBE>true</HideWirelessSetupInOOBE>
        <ProtectYourPC>3</ProtectYourPC>
        <NetworkLocation>Work</NetworkLocation>
        <SkipMachineOOBE>true</SkipMachineOOBE>
        <SkipUserOOBE>true</SkipUserOOBE>
      </OOBE>
    </component>
    <component name="Microsoft-Windows-International-Core" processorArchitecture="$ProcessorArchitecture" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
      <InputLocale>en-US</InputLocale>
      <SystemLocale>en-US</SystemLocale>
      <UILanguage>en-US</UILanguage>
      <UserLocale>en-US</UserLocale>
    </component>
  </settings>
</unattend>
"@
}

# unattend.xml's processorArchitecture values do not spell Windows'
# %PROCESSOR_ARCHITECTURE% values consistently (arm64 does; amd64 does not).
function Get-UnattendProcessorArchitecture {
    param([string]$WindowsProcessorArchitecture)
    switch ($WindowsProcessorArchitecture) {
        'ARM64'  { return 'arm64' }
        'AMD64'  { return 'amd64' }
        'x86'    { return 'x86' }
        default  { return $WindowsProcessorArchitecture.ToLowerInvariant() }
    }
}

Write-Host 'fwdslash driver lab - guest creation (host side)'
Write-Host '------------------------------------------------'

if ($DryRunUnattend) {
    Write-Host 'DryRunUnattend: generating the unattend.xml only. No Hyper-V, DISM or disk access.'
    $previewPassword = New-LabPassword -Length 20
    $arch = Get-UnattendProcessorArchitecture -WindowsProcessorArchitecture $env:PROCESSOR_ARCHITECTURE
    $xmlText = New-LabUnattendXml -ComputerName $GuestComputerName -AccountName $GuestAccountName `
        -Password $previewPassword -ProcessorArchitecture $arch
    # Validate it parses before writing it out - the whole point of a dry run.
    [xml]$parsed = $xmlText
    Write-Step "parsed OK: $($parsed.unattend.settings.Count) <settings> pass(es)"
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $DryRunOutputPath) | Out-Null
    Set-Content -LiteralPath $DryRunOutputPath -Value $xmlText -Encoding utf8
    Write-Step "written to $DryRunOutputPath (preview password, not a real credential - nothing here is used by an actual guest)"
    exit 0
}

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
        Write-Host '  Checkpoints     : none yet.'
    } else {
        Write-Host "  Checkpoints     : $(($snapshots | ForEach-Object { $_.Name }) -join ', ')"
    }
    Write-Host ''
    Write-Host "Restore the clean baseline before every run:  Restore-VMSnapshot -VMName $Name -Name clean-os -Confirm:`$false"
    exit 0
}

if ([string]::IsNullOrWhiteSpace($IsoPath)) {
    throw '-IsoPath is required (unless -DryRunUnattend is given).'
}
if (-not (Test-Path -LiteralPath $IsoPath)) {
    throw 'The ISO path given does not exist.'
}
if (Test-Path -LiteralPath "$($EfiDriveLetter):\") {
    throw "Drive letter ${EfiDriveLetter}: is already in use on this host; pass a different -EfiDriveLetter."
}
if (Test-Path -LiteralPath "$($WindowsDriveLetter):\") {
    throw "Drive letter ${WindowsDriveLetter}: is already in use on this host; pass a different -WindowsDriveLetter."
}

$hostArchitecture = $env:PROCESSOR_ARCHITECTURE
Write-Host "Host architecture: $hostArchitecture"
Write-Host "A Hyper-V guest runs the host's architecture. Use a Windows $hostArchitecture ISO;"
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

New-Item -ItemType Directory -Force -Path $labDir | Out-Null

# ============================================================ build the VHDX ==
$isoMounted = $false
$vhdMounted = $false
try {
    Write-Host "Mounting ISO $IsoPath ..."
    $isoMount = Mount-DiskImage -ImagePath $IsoPath -PassThru
    $isoMounted = $true
    $isoDrive = ($isoMount | Get-Volume).DriveLetter
    $wimPath = "${isoDrive}:\sources\install.wim"
    if (-not (Test-Path -LiteralPath $wimPath)) { throw "install.wim not found at $wimPath" }
    Write-Step "ISO mounted at ${isoDrive}: ; wim = $wimPath"

    Write-Host "Creating dynamic VHDX ($([math]::Round($VhdSizeBytes / 1GB, 0))GB) at $vhdPath ..."
    New-VHD -Path $vhdPath -Dynamic -SizeBytes $VhdSizeBytes | Out-Null

    Write-Host 'Mounting VHDX...'
    $mountResult = Mount-VHD -Path $vhdPath -Passthru
    $vhdMounted = $true
    $diskNumber = ($mountResult | Get-Disk).Number
    Write-Step "attached as disk number $diskNumber"

    Write-Host 'Initializing disk as GPT...'
    Initialize-Disk -Number $diskNumber -PartitionStyle GPT

    Write-Host "Creating EFI System Partition (100MB, FAT32, ${EfiDriveLetter}:)..."
    $efiPartition = New-Partition -DiskNumber $diskNumber -Size 100MB `
        -GptType '{c12a7328-f81f-11d2-ba4b-00a0c93ec93b}' -DriveLetter $EfiDriveLetter
    Format-Volume -Partition $efiPartition -FileSystem FAT32 -NewFileSystemLabel 'System' -Confirm:$false | Out-Null

    Write-Host 'Creating MSR partition (16MB, unformatted)...'
    New-Partition -DiskNumber $diskNumber -Size 16MB `
        -GptType '{e3c9e316-0b5c-4db8-817d-f92df00215ae}' | Out-Null

    Write-Host "Creating Windows partition (remaining space, NTFS, ${WindowsDriveLetter}:)..."
    $winPartition = New-Partition -DiskNumber $diskNumber -UseMaximumSize -DriveLetter $WindowsDriveLetter
    Format-Volume -Partition $winPartition -FileSystem NTFS -NewFileSystemLabel 'Windows' -Confirm:$false | Out-Null

    Write-Host "Applying image (index $ImageIndex) from $wimPath to ${WindowsDriveLetter}:\ - this takes several minutes..."
    $applyOutput = & dism.exe /Apply-Image "/ImageFile:$wimPath" "/Index:$ImageIndex" "/ApplyDir:${WindowsDriveLetter}:\" 2>&1 | Out-String
    Write-Host $applyOutput
    if ($LASTEXITCODE -ne 0) { throw "dism /Apply-Image failed with exit code $LASTEXITCODE" }
    Write-Step 'image applied'

    Write-Host "Running bcdboot ${WindowsDriveLetter}:\Windows /s ${EfiDriveLetter}: /f UEFI ..."
    $bcdbootOutput = & bcdboot.exe "${WindowsDriveLetter}:\Windows" /s "${EfiDriveLetter}:" /f UEFI 2>&1 | Out-String
    Write-Host $bcdbootOutput
    if ($LASTEXITCODE -ne 0) { throw "bcdboot failed with exit code $LASTEXITCODE" }
    Write-Step 'bcdboot completed'

    Write-Host 'Generating guest local-admin password and unattend.xml...'
    $password = New-LabPassword -Length 20
    $arch = Get-UnattendProcessorArchitecture -WindowsProcessorArchitecture $hostArchitecture
    $unattendXml = New-LabUnattendXml -ComputerName $GuestComputerName -AccountName $GuestAccountName `
        -Password $password -ProcessorArchitecture $arch
    [xml]$null = $unattendXml   # fail fast, before anything is written, if templating produced bad XML
    $pantherDir = "${WindowsDriveLetter}:\Windows\Panther"
    New-Item -ItemType Directory -Force -Path $pantherDir | Out-Null
    Set-Content -LiteralPath (Join-Path $pantherDir 'unattend.xml') -Value $unattendXml -Encoding utf8
    Write-Step "unattend.xml written to $(Join-Path $pantherDir 'unattend.xml')"

    Set-Content -LiteralPath $credPath -Value @(
        "VM: $Name"
        "Local admin account: $GuestAccountName"
        "Password: $password"
        "Generated: $(Get-Date -Format o)"
    )
    Write-Step "credentials written to $credPath (not echoed here)"
    $password = $null

    Write-Host 'Dismounting VHDX...'
    Dismount-VHD -Path $vhdPath
    $vhdMounted = $false
    Write-Host 'Dismounting ISO...'
    Dismount-DiskImage -ImagePath $IsoPath | Out-Null
    $isoMounted = $false
} catch {
    try { if ($vhdMounted) { Dismount-VHD -Path $vhdPath -ErrorAction SilentlyContinue } } catch {}
    try { if ($isoMounted) { Dismount-DiskImage -ImagePath $IsoPath -ErrorAction SilentlyContinue | Out-Null } } catch {}
    throw
}

# ========================================================== create the VM ====
Write-Host "Creating VM '$Name' from the prebuilt VHDX..."
$newVmArguments = @{
    Name               = $Name
    Generation         = 2
    MemoryStartupBytes = $MemoryStartupBytes
    VHDPath            = $vhdPath
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

Enable-VMIntegrationService -VMName $Name -Name 'Guest Service Interface'
Write-Step 'Guest Service Interface enabled (Copy-VMFile works)'

# Standard checkpoints capture memory as well as disk. After a bugcheck a
# production checkpoint would restore a clean-shutdown image and hide the state
# the crash happened in.
Set-VM -VMName $Name -CheckpointType Standard -AutomaticCheckpointsEnabled $false
Write-Step 'standard checkpoints, automatic checkpoints off'

Write-Host "Starting VM '$Name'..."
try {
    Start-VM -Name $Name -ErrorAction Stop
} catch {
    if ($ExposeVirtualization -and $_.Exception.Message -match '(?i)nested virtualization') {
        # On some ARM64 hosts Set-VMProcessor -ExposeVirtualizationExtensions
        # $true does not fail at set time - it only fails here, one step
        # later, at Start-VM. Fall back to no nested virt: the harness must
        # then run with -FakeShare instead of a real WSL distribution.
        Write-Warning 'Start-VM failed: this platform does not support nested virtualization. Disabling ExposeVirtualizationExtensions and retrying (WSL unavailable -> use -FakeShare).'
        Set-VMProcessor -VMName $Name -ExposeVirtualizationExtensions $false
        Start-VM -Name $Name -ErrorAction Stop
    } else {
        throw
    }
}

$securePassword = ConvertTo-SecureString -String (
    (Get-Content -LiteralPath $credPath | Where-Object { $_ -match '^Password:\s*(.+)$' } |
        Select-Object -First 1) -replace '^Password:\s*', ''
) -AsPlainText -Force
$cred = New-Object System.Management.Automation.PSCredential($GuestAccountName, $securePassword)

Write-Host "Waiting up to $BootTimeoutMinutes minutes for PowerShell Direct (first boot runs specialize + autologon)..."
$deadline = (Get-Date).AddMinutes($BootTimeoutMinutes)
$connected = $false
$lastError = ''
while ((Get-Date) -lt $deadline) {
    try {
        $guestHostname = Invoke-Command -VMName $Name -Credential $cred -ScriptBlock { hostname } -ErrorAction Stop
        Write-Step "PowerShell Direct succeeded; guest hostname: $guestHostname"
        $connected = $true
        break
    } catch {
        $lastError = $_.Exception.Message
        Start-Sleep -Seconds 20
    }
}
if (-not $connected) {
    throw "PowerShell Direct did not succeed within $BootTimeoutMinutes minutes. Last error: $lastError. The VM and its VHDX are left in place for inspection (vmconnect.exe localhost $Name)."
}

Write-Host "Taking checkpoint 'clean-os' (the baseline every gate run restores)..."
Checkpoint-VM -Name $Name -SnapshotName 'clean-os'
Write-Step 'checkpoint created'

Write-Host ''
Write-Host 'Guest ready. Next:'
Write-Host ''
Write-Host '  1. Copy the lab package in (Guest Service Interface, no network share'
Write-Host '     and no file sharing with the host required):'
Write-Host "       Copy-VMFile -Name $Name ``"
Write-Host '           -SourcePath <host path to fwdslash-filter-*.zip> ``'
Write-Host "           -DestinationPath 'C:\FswLab\fwdslash-filter.zip' ``"
Write-Host '           -CreateFullPath -FileSource Host'
Write-Host '     Repeat for tools\Bootstrap-DriverLabGuest.ps1 and tools\Test-Driver.ps1.'
Write-Host ''
Write-Host '  2. Inside the guest, elevated:'
Write-Host "       C:\FswLab\Bootstrap-DriverLabGuest.ps1 -CertificatePath C:\FswLab\fwdslash-lab.cer -FakeShare -Reboot"
Write-Host "       C:\FswLab\Test-Driver.ps1 -PackageZip C:\FswLab\fwdslash-filter.zip -FakeShare"
Write-Host ''
Write-Host '  3. After each run, roll back before the next one:'
Write-Host "       Restore-VMSnapshot -VMName $Name -Name clean-os -Confirm:`$false"
Write-Host ''
Write-Host "Guest credentials: $credPath"
Write-Host "Runbook: $(Join-Path $repo 'docs\driver-lab.md')"
exit 0
