#Requires -Version 5.1
<#
.SYNOPSIS
    The fswfilter driver release gate. Runs INSIDE the checkpointed lab guest,
    elevated, and drives every step docs\compatibility.md "Driver release gate"
    names.

.DESCRIPTION
    LAB GUEST ONLY - NEVER A PHYSICAL WORKSTATION. This harness installs and
    loads an unsigned/test-signed kernel minifilter, unloads it under load, and
    sends deliberately malformed messages to a kernel port. Any of those can
    bugcheck the machine; that is the point of running them. It belongs in a
    Hyper-V guest with a 'clean-os' checkpoint, built by
    tools\New-DriverLabVm.ps1 and prepared by tools\Bootstrap-DriverLabGuest.ps1.
    It refuses to run on hardware that does not look like a virtual machine
    unless -Force is given.

    Steps, in order, each printing [PASS] / [FAIL] / [SKIPPED]:
      a  preflight: test signing, UNC target reachable, unpack, pnputil install,
         fltmc load, altitude 371120, instances on disk volumes only
      b  broker publish: fwdslash start, driverConnected true within 10 s
      c  alias-versus-UNC parity for the operation matrix, through the
         PowerShell provider, .NET, Win32 CreateFileW, cmd.exe and python
      d  identity rules: standard and elevated redirected; AppContainer and
         SYSTEM not redirected
      e  broker lifecycle: stop, crash, restart, malformed messages, slot reuse
      f  unload under load, then reload
      g  create-rate benchmark on a NON-matching path, filter loaded vs unloaded
      h  teardown: unload, remove from the driver store, summary

    Anything that depends on a piece not present in this guest - no CLI, no real
    WSL distribution, no python, no Invoke-CommandInDesktopPackage - degrades to
    [SKIPPED] with a reason. The harness never crashes on a missing dependency;
    it only fails on a real disagreement.

    Exit code 0 when nothing failed, 1 otherwise. [SKIPPED] does not fail the
    run, but a gate row in docs\compatibility.md may not be marked verified from
    a run that skipped its step.

.PARAMETER PackageZip
    The lab zip from tools\Package-Driver.ps1 -Lab.

.PARAMETER Distribution
    Distribution name the driver maps. Default 'Ubuntu'. Must match the WSL
    distribution, or the fake SMB share name.

.PARAMETER FakeShare
    The UNC target is the loopback SMB share Bootstrap-DriverLabGuest.ps1
    -FakeShare created, not a real WSL distribution. Cases that need real 9P
    semantics are skipped and the summary says the run is not gate evidence.

.PARAMETER SkipVerifier
    Do not require Driver Verifier to be active. The gate requires it.

.PARAMETER Iterations
    Create-rate benchmark iterations. Default 20000.

.PARAMETER CliPath
    Path to fwdslash.exe. Found on PATH or in the usual build outputs otherwise.

.PARAMETER WorkDirectory
    Scratch directory for the unpacked package and probe output.
    Default C:\FswLab\run.

.PARAMETER KeepInstalled
    Skip step h so the driver stays loaded for manual poking. Restore the
    checkpoint afterwards.

.PARAMETER Force
    Proceed even when the machine does not look like a virtual machine.

.EXAMPLE
    .\Test-Driver.ps1 -PackageZip C:\FswLab\fwdslash-filter-0.0.3.0-arm64.zip -FakeShare
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$PackageZip,

    [string]$Distribution = 'Ubuntu',

    [switch]$FakeShare,

    [switch]$SkipVerifier,

    [int]$Iterations = 20000,

    [string]$CliPath,

    [string]$WorkDirectory = 'C:\FswLab\run',

    [switch]$KeepInstalled,

    [switch]$Force
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:FilterName = 'FswFilter'
$script:ExpectedAltitude = '371120'
$script:PortName = '\FswFilterPort'
$script:Passed = 0
$script:Failed = 0
$script:Skipped = 0
$script:Step = 'a'
$script:PublishedName = $null
$script:DriverLoaded = $false
$script:Cli = $null
$script:StartTime = Get-Date

$script:AliasRoot = "C:\$Distribution"
$script:UncRoot = "\\wsl.localhost\$Distribution"
$script:AliasNativeBaseline = $false

# ---------------------------------------------------------------- reporting --

function Write-Banner {
    param([string]$Letter, [string]$Title)
    $script:Step = $Letter
    Write-Host ''
    Write-Host "=== $Letter. $Title"
}

function Add-Pass {
    param([string]$Name, [string]$Detail = '')
    $script:Passed++
    Write-Host "[PASS] $($script:Step). $Name $Detail".TrimEnd()
}

function Add-Fail {
    param([string]$Name, [string]$Detail = '')
    $script:Failed++
    Write-Host "[FAIL] $($script:Step). $Name $Detail".TrimEnd()
}

function Add-Skip {
    param([string]$Name, [string]$Reason)
    $script:Skipped++
    Write-Host "[SKIPPED] $($script:Step). $Name - $Reason"
}

function Assert-True {
    param([bool]$Condition, [string]$Name, [string]$Detail = '')
    if ($Condition) { Add-Pass $Name } else { Add-Fail $Name $Detail }
    return $Condition
}

function Assert-Equal {
    param([string]$Expected, [string]$Actual, [string]$Name)
    if ($Expected -ceq $Actual) {
        Add-Pass $Name
        return $true
    }
    Add-Fail $Name "(expected '$Expected', got '$Actual')"
    return $false
}

# Every step body runs inside this so an unexpected exception is a failure
# rather than an aborted run: a half-run harness leaves a loaded driver behind.
function Invoke-Step {
    param([string]$Name, [scriptblock]$Body)
    try {
        & $Body
    } catch {
        Add-Fail $Name "(unhandled: $($_.Exception.Message))"
    }
}

function Invoke-Native {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [Parameter(Mandatory = $true)][string[]]$Arguments
    )
    $output = ''
    $code = -1
    try {
        $output = (& $FilePath @Arguments 2>&1 | Out-String).Trim()
        $code = $LASTEXITCODE
    } catch {
        $output = $_.Exception.Message
        $code = -1
    }
    return [pscustomobject]@{ ExitCode = $code; Output = $output }
}

function Test-Elevated {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Test-VirtualMachine {
    try {
        $system = Get-CimInstance -ClassName Win32_ComputerSystem
        $haystack = "$($system.Model) $($system.Manufacturer)"
    } catch {
        return $false
    }
    foreach ($needle in @('Virtual Machine', 'VMware', 'VirtualBox', 'QEMU', 'Xen', 'Hyper-V', 'KVM', 'Parallels')) {
        if ($haystack -like "*$needle*") { return $true }
    }
    return $false
}

function Wait-ForCondition {
    param([scriptblock]$Condition, [int]$TimeoutSeconds = 10, [int]$PollMilliseconds = 250)
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        $value = $false
        try { $value = [bool](& $Condition) } catch { $value = $false }
        if ($value) { return $true }
        Start-Sleep -Milliseconds $PollMilliseconds
    }
    return $false
}

# ------------------------------------------------------------------ natives --

if (-not ('FswLab.Native' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Diagnostics;
using System.Runtime.InteropServices;

namespace FswLab {
  public static class Native {
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern IntPtr CreateFileW(string name, uint access, uint share, IntPtr sa,
                                             uint disposition, uint flags, IntPtr template);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool CloseHandle(IntPtr handle);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern uint GetFileAttributesW(string name);

    private const uint GenericRead = 0x80000000;
    private const uint ShareAll = 0x00000007;
    private const uint OpenExisting = 3;
    private const uint BackupSemantics = 0x02000000;
    private static readonly IntPtr Invalid = new IntPtr(-1);

    // 0 on success, else the Win32 error. Comparing the error code as well as
    // success is what makes an alias-versus-UNC comparison meaningful.
    public static int OpenStatus(string path) {
      IntPtr handle = CreateFileW(path, GenericRead, ShareAll, IntPtr.Zero, OpenExisting,
                                  BackupSemantics, IntPtr.Zero);
      if (handle == Invalid) { return Marshal.GetLastWin32Error(); }
      CloseHandle(handle);
      return 0;
    }

    public static uint Attributes(string path) { return GetFileAttributesW(path); }

    public static double MeasureCreateMilliseconds(string path, int iterations) {
      for (int warm = 0; warm < 200; warm++) {
        IntPtr h = CreateFileW(path, GenericRead, ShareAll, IntPtr.Zero, OpenExisting,
                               BackupSemantics, IntPtr.Zero);
        if (h != Invalid) { CloseHandle(h); }
      }
      Stopwatch watch = Stopwatch.StartNew();
      for (int i = 0; i < iterations; i++) {
        IntPtr h = CreateFileW(path, GenericRead, ShareAll, IntPtr.Zero, OpenExisting,
                               BackupSemantics, IntPtr.Zero);
        if (h != Invalid) { CloseHandle(h); }
      }
      watch.Stop();
      return watch.Elapsed.TotalMilliseconds;
    }
  }

  // Layout must match FSW_MAPPING_MESSAGE in include/fsw_filter_protocol.h.
  // Marshal.SizeOf is the size the driver compares InputBufferLength against,
  // so the malformed cases stay correct if the contract grows a field.
  [StructLayout(LayoutKind.Sequential)]
  public struct MappingMessage {
    public uint Version;
    public uint Size;
    public uint Operation;
    public uint Reserved;
    public ulong Generation;
    public uint DistributionCount;
    [MarshalAs(UnmanagedType.ByValArray, SizeConst = 32 * 128)]
    public ushort[] Distributions;
  }

  public static class Port {
    [DllImport("fltlib.dll", CharSet = CharSet.Unicode)]
    private static extern int FilterConnectCommunicationPort(string portName, uint options,
        IntPtr context, ushort contextSize, IntPtr security, out IntPtr port);
    [DllImport("fltlib.dll")]
    private static extern int FilterSendMessage(IntPtr port, IntPtr input, uint inputSize,
        IntPtr output, uint outputSize, out uint returned);
    [DllImport("kernel32.dll")]
    private static extern bool CloseHandle(IntPtr handle);

    public static int MessageSize { get { return Marshal.SizeOf(typeof(MappingMessage)); } }

    // Sequential connect/close. Returns the HRESULT of the last attempt, or the
    // first failure. A driver that leaks a slot per connection fails here.
    public static int ConnectCycle(string portName, int times) {
      int last = 0;
      for (int i = 0; i < times; i++) {
        IntPtr handle;
        last = FilterConnectCommunicationPort(portName, 0, IntPtr.Zero, 0, IntPtr.Zero, out handle);
        if (last < 0) { return last; }
        CloseHandle(handle);
      }
      return last;
    }

    // FswOperationPing writes FSW_PROTOCOL_VERSION into a ULONG output buffer
    // (include/fsw_filter_protocol.h, "Ping reply contract"), so a client can
    // report the protocol the LOADED driver speaks. Returns that version, 0
    // when the driver returned no output, or -1 when the round trip failed.
    public static int Ping(string portName, byte[] buffer) {
      IntPtr handle;
      int hr = FilterConnectCommunicationPort(portName, 0, IntPtr.Zero, 0, IntPtr.Zero, out handle);
      if (hr < 0) { return -1; }
      IntPtr input = Marshal.AllocHGlobal(buffer.Length);
      IntPtr output = Marshal.AllocHGlobal(4);
      try {
        Marshal.Copy(buffer, 0, input, buffer.Length);
        Marshal.WriteInt32(output, 0);
        uint returned;
        int sent = FilterSendMessage(handle, input, (uint)buffer.Length, output, 4, out returned);
        if (sent < 0) { return -1; }
        if (returned < 4) { return 0; }
        return Marshal.ReadInt32(output);
      } finally {
        Marshal.FreeHGlobal(input);
        Marshal.FreeHGlobal(output);
        CloseHandle(handle);
      }
    }

    public static int SendRaw(string portName, byte[] buffer, int declaredSize) {
      IntPtr handle;
      int hr = FilterConnectCommunicationPort(portName, 0, IntPtr.Zero, 0, IntPtr.Zero, out handle);
      if (hr < 0) { return hr; }
      IntPtr native = Marshal.AllocHGlobal(buffer.Length);
      try {
        Marshal.Copy(buffer, 0, native, buffer.Length);
        uint returned;
        return FilterSendMessage(handle, native, (uint)declaredSize, IntPtr.Zero, 0, out returned);
      } finally {
        Marshal.FreeHGlobal(native);
        CloseHandle(handle);
      }
    }
  }
}
'@
}

# Builds an FSW_MAPPING_MESSAGE by hand. Field offsets follow the struct in
# include/fsw_filter_protocol.h: Version 0, Size 4, Operation 8, Reserved 12,
# Generation 16, DistributionCount 24, Distributions 28. The total size comes
# from Marshal.SizeOf so the padding after the name array stays correct.
function New-MappingBuffer {
    param(
        [uint32]$Version = 2,
        [uint32]$Size = 0,
        [uint32]$Operation = 1,
        [uint32]$Reserved = 0,
        [uint64]$Generation = 1,
        [uint32]$Count = 1,
        [string]$FirstName = 'Ubuntu'
    )
    $total = [FswLab.Port]::MessageSize
    if ($Size -eq 0) { $Size = [uint32]$total }
    $buffer = New-Object byte[] $total
    [BitConverter]::GetBytes($Version).CopyTo($buffer, 0)
    [BitConverter]::GetBytes($Size).CopyTo($buffer, 4)
    [BitConverter]::GetBytes($Operation).CopyTo($buffer, 8)
    [BitConverter]::GetBytes($Reserved).CopyTo($buffer, 12)
    [BitConverter]::GetBytes($Generation).CopyTo($buffer, 16)
    [BitConverter]::GetBytes($Count).CopyTo($buffer, 24)
    $nameBytes = [Text.Encoding]::Unicode.GetBytes($FirstName)
    [Array]::Copy($nameBytes, 0, $buffer, 28, [Math]::Min($nameBytes.Length, 254))
    return $buffer
}

# --------------------------------------------------------------------- start --

Write-Host 'fwdslash driver release gate'
Write-Host '----------------------------'
Write-Host 'LAB GUEST ONLY. This loads unsigned kernel code and tries to break it.'
Write-Host "Distribution: $Distribution   UNC target: $($script:UncRoot)   Alias: $($script:AliasRoot)"
if ($FakeShare) {
    Write-Host 'Mode: -FakeShare (loopback SMB share, not a real WSL distribution).'
}

if (-not (Test-Elevated)) {
    throw 'Run this from an elevated PowerShell inside the lab guest.'
}
if (-not (Test-VirtualMachine) -and -not $Force) {
    throw 'This machine does not look like a virtual machine. Refusing to load an unsigned kernel driver. Use -Force only if the detection is wrong.'
}
if (-not (Test-Path -LiteralPath $PackageZip)) {
    throw 'The package zip given does not exist.'
}

New-Item -ItemType Directory -Force -Path $WorkDirectory | Out-Null

# ============================================================== a. preflight ==
Write-Banner 'a' 'Preflight, install and load'

$unpacked = Join-Path $WorkDirectory 'package'
Invoke-Step 'preflight' {
    $bcd = Invoke-Native -FilePath 'bcdedit.exe' -Arguments @('/enum', '{current}')
    $testsigning = $bcd.Output -match '(?im)^\s*testsigning\s+Yes\s*$'
    Assert-True $testsigning 'test signing is on' '(bcdedit /set testsigning on, then reboot; Secure Boot must be off)' | Out-Null

    if ($SkipVerifier) {
        Add-Skip 'Driver Verifier active' '-SkipVerifier was given; a gate run must have Verifier on'
    } else {
        $query = Invoke-Native -FilePath 'verifier.exe' -Arguments @('/query')
        $verifierOn = $query.Output -match '(?i)fswfilter'
        Assert-True $verifierOn 'Driver Verifier is tracking fswfilter.sys' '(Bootstrap-DriverLabGuest.ps1 sets flags 0x93B; it needs a reboot)' | Out-Null
    }

    # The alias may legitimately exist as a real folder. Record the baseline so
    # the lifecycle assertions in step e compare against it instead of assuming
    # False.
    $script:AliasNativeBaseline = Test-Path -LiteralPath $script:AliasRoot
    if ($script:AliasNativeBaseline) {
        Write-Host "   note: $($script:AliasRoot) exists natively - it will be SHADOWED while the mapping is live. That is by design."
    }

    $uncReachable = $false
    try { $uncReachable = Test-Path -LiteralPath $script:UncRoot } catch { $uncReachable = $false }
    if (-not (Assert-True $uncReachable "UNC target $($script:UncRoot) is reachable" '(nothing can pass while the reparse target is dead)')) {
        throw 'The UNC target is unreachable; the rest of the run would fail for the wrong reason.'
    }

    if (Test-Path -LiteralPath $unpacked) { Remove-Item -LiteralPath $unpacked -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $unpacked | Out-Null
    Expand-Archive -LiteralPath $PackageZip -DestinationPath $unpacked -Force
    $inf = Join-Path $unpacked 'fswfilter.inf'
    Assert-True (Test-Path -LiteralPath $inf) 'package unpacked (fswfilter.inf present)' | Out-Null

    $add = Invoke-Native -FilePath 'pnputil.exe' -Arguments @('/add-driver', $inf, '/install')
    if ($add.Output -match '(?im)Published\s+Name\s*:\s*(oem\d+\.inf)') {
        $script:PublishedName = $Matches[1]
    }
    Assert-True ($add.ExitCode -eq 0) 'pnputil /add-driver /install' "(exit $($add.ExitCode): $($add.Output))" | Out-Null
    if ($null -ne $script:PublishedName) {
        Write-Host "   published as $($script:PublishedName)"
    }

    $load = Invoke-Native -FilePath 'fltmc.exe' -Arguments @('load', $script:FilterName)
    $script:DriverLoaded = ($load.ExitCode -eq 0)
    Assert-True $script:DriverLoaded 'fltmc load FswFilter' "(exit $($load.ExitCode): $($load.Output))" | Out-Null

    $filters = Invoke-Native -FilePath 'fltmc.exe' -Arguments @('filters')
    $listed = $filters.Output -match "(?im)^\s*$($script:FilterName)\s"
    Assert-True $listed 'fltmc filters lists FswFilter' "($($filters.Output))" | Out-Null
    $altitudeMatch = $filters.Output -match "(?im)^\s*$($script:FilterName)\s+\S+\s+(\S+)"
    if ($altitudeMatch) {
        Assert-Equal $script:ExpectedAltitude $Matches[1] "altitude is $($script:ExpectedAltitude)" | Out-Null
    } else {
        Add-Skip 'altitude check' 'fltmc filters output could not be parsed'
    }

    $instances = Invoke-Native -FilePath 'fltmc.exe' -Arguments @('instances', '-f', $script:FilterName)
    $networkAttached = $instances.Output -match '(?i)\\Device\\Mup|(?i)LanmanRedirector|(?i)\\Device\\WebDavRedirector'
    Assert-True (-not $networkAttached) 'no instance on a network filesystem (no reparse recursion)' "($($instances.Output))" | Out-Null
    $hasVolume = $instances.Output -match '(?i)[A-Z]:' -or $instances.Output -match '(?i)HarddiskVolume'
    Assert-True $hasVolume 'attached to at least one disk volume' "($($instances.Output))" | Out-Null
}

if (-not $script:DriverLoaded) {
    Write-Host ''
    Write-Host 'The driver did not load. Everything after this point would be noise; stopping.'
    Write-Host "Summary: $($script:Passed) passed, $($script:Failed) failed, $($script:Skipped) skipped."
    exit 1
}

# ========================================================= b. broker publish ==
Write-Banner 'b' 'Broker publishes the mapping'

Invoke-Step 'broker publish' {
    if (-not [string]::IsNullOrWhiteSpace($CliPath) -and (Test-Path -LiteralPath $CliPath)) {
        $script:Cli = (Resolve-Path -LiteralPath $CliPath).Path
    } else {
        $onPath = Get-Command -Name 'fwdslash.exe' -ErrorAction SilentlyContinue
        if ($null -ne $onPath) {
            $script:Cli = $onPath.Source
        } else {
            $candidates = @(
                (Join-Path $WorkDirectory 'fwdslash.exe'),
                'C:\FswLab\fwdslash.exe',
                'C:\FswLab\bin\fwdslash.exe'
            )
            foreach ($candidate in $candidates) {
                if (Test-Path -LiteralPath $candidate) { $script:Cli = $candidate; break }
            }
        }
    }

    if ($null -eq $script:Cli) {
        Add-Skip 'fwdslash start' 'fwdslash.exe was not found (pass -CliPath, or copy the user-mode build into the guest)'
        Add-Skip 'driverConnected within 10 s' 'no CLI'
        return
    }

    $start = Invoke-Native -FilePath $script:Cli -Arguments @('start')
    Assert-True ($start.ExitCode -eq 0) 'fwdslash start' "(exit $($start.ExitCode): $($start.Output))" | Out-Null

    # The broker publishes on start and again on the 5 s health tick, so 10 s is
    # two chances plus slack.
    $connected = Wait-ForCondition -TimeoutSeconds 10 -Condition {
        $status = Invoke-Native -FilePath $script:Cli -Arguments @('status', '--json')
        if ($status.ExitCode -ne 0) { return $false }
        try {
            $json = $status.Output | ConvertFrom-Json
        } catch {
            return $false
        }
        if ($json.PSObject.Properties.Name -notcontains 'driverConnected') { return $false }
        return [bool]$json.driverConnected
    }
    Assert-True $connected 'fwdslash status --json reports driverConnected true within 10 s' | Out-Null

    $probe = Wait-ForCondition -TimeoutSeconds 10 -Condition { Test-Path -LiteralPath $script:AliasRoot }
    Assert-True $probe "the alias root $($script:AliasRoot) resolves once the mapping is published" | Out-Null
}

Invoke-Step 'ping protocol version' {
    # FswOperationPing = 3. The reply carries the protocol the loaded driver
    # speaks, which is what `fwdslash driver status` reports.
    $pingBuffer = New-MappingBuffer -Operation 3 -Count 0 -FirstName ''
    $protocol = -1
    try {
        $protocol = [FswLab.Port]::Ping($script:PortName, $pingBuffer)
    } catch {
        $protocol = -1
    }
    if ($protocol -lt 0) {
        Add-Skip 'ping reports the loaded protocol version' 'the port refused the harness connection or the ping round trip failed'
    } elseif ($protocol -eq 0) {
        Add-Skip 'ping reports the loaded protocol version' 'the driver returned no output for the ping (pre-reply-contract build)'
    } else {
        Write-Host "   loaded driver protocol: v$protocol"
        Assert-True ($protocol -eq 2) 'ping reports protocol v2' "(got v$protocol)" | Out-Null
    }
}

# ================================================== c. alias-vs-UNC parity ====
Write-Banner 'c' 'Alias-versus-UNC parity matrix'

$script:ScratchName = 'fswlab-parity'
$script:UncScratch = Join-Path $script:UncRoot $script:ScratchName
$script:AliasScratch = Join-Path $script:AliasRoot $script:ScratchName

function Get-Probe {
    param([scriptblock]$Body, [string]$Path)
    try {
        $value = & $Body $Path
        if ($null -eq $value) { return '<null>' }
        return [string]$value
    } catch {
        return "ERR:$($_.Exception.GetType().Name)"
    }
}

function Test-Parity {
    param(
        [Parameter(Mandatory = $true)][string]$Method,
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][string]$AliasPath,
        [Parameter(Mandatory = $true)][string]$UncPath,
        [Parameter(Mandatory = $true)][scriptblock]$Body
    )
    $aliasValue = Get-Probe -Body $Body -Path $AliasPath
    $uncValue = Get-Probe -Body $Body -Path $UncPath
    if ($aliasValue -ceq $uncValue) {
        Add-Pass "[$Method] $Name"
        return $true
    }
    Add-Fail "[$Method] $Name" "(alias='$aliasValue' unc='$uncValue')"
    return $false
}

# Probe bodies. Each takes one path and returns a comparable string. They must
# never throw: Get-Probe turns a throw into a comparable ERR: token, so an
# alias that fails the same way the UNC path does still counts as parity.
$probePsExists      = { param($p) [string](Test-Path -LiteralPath $p) }
$probePsContainer   = { param($p) [string](Test-Path -LiteralPath $p -PathType Container) }
$probePsEnumerate   = { param($p) ((Get-ChildItem -LiteralPath $p -Force -ErrorAction Stop | ForEach-Object { $_.Name } | Sort-Object) -join '|') }
$probePsRead        = { param($p) (Get-Content -LiteralPath $p -Raw -ErrorAction Stop) }
$probePsAttributes  = { param($p) $item = Get-Item -LiteralPath $p -Force -ErrorAction Stop; "len=$($item.Length) attr=$($item.Attributes)" }
$probeNetExists     = { param($p) [string]([System.IO.File]::Exists($p)) }
$probeNetDirExists  = { param($p) [string]([System.IO.Directory]::Exists($p)) }
$probeNetEnumerate  = { param($p) (([System.IO.Directory]::GetFileSystemEntries($p) | ForEach-Object { [System.IO.Path]::GetFileName($_) } | Sort-Object) -join '|') }
$probeNetRead       = { param($p) [System.IO.File]::ReadAllText($p) }
$probeNetLength     = { param($p) [string](New-Object System.IO.FileInfo($p)).Length }
$probeWin32Open     = { param($p) [string][FswLab.Native]::OpenStatus($p) }
$probeWin32Attr     = { param($p) [string][FswLab.Native]::Attributes($p) }
$probeCmdDir        = { param($p) $r = Invoke-Native -FilePath 'cmd.exe' -Arguments @('/c', 'dir', '/b', $p); "exit=$($r.ExitCode) names=$((($r.Output -split "`r?`n" | Where-Object { $_ -ne '' } | Sort-Object) -join '|'))" }
$probeCmdType       = { param($p) $r = Invoke-Native -FilePath 'cmd.exe' -Arguments @('/c', 'type', $p); "exit=$($r.ExitCode) text=$($r.Output)" }

$script:Python = $null
$pythonCommand = Get-Command -Name 'python.exe' -ErrorAction SilentlyContinue
if ($null -ne $pythonCommand) {
    # The Store's python.exe stub is an app-execution alias that opens the Store
    # instead of running code; a version probe is the only honest test.
    $version = Invoke-Native -FilePath $pythonCommand.Source -Arguments @('-c', 'print(1)')
    if ($version.ExitCode -eq 0 -and $version.Output -eq '1') {
        $script:Python = $pythonCommand.Source
    }
}
$probePythonListing = {
    param($p)
    $code = 'import os,sys' + "`n" +
            'p=sys.argv[1]' + "`n" +
            'print("isdir=%s" % os.path.isdir(p))' + "`n" +
            'print("names=%s" % ("|".join(sorted(os.listdir(p))) if os.path.isdir(p) else ""))'
    $r = Invoke-Native -FilePath $script:Python -Arguments @('-c', $code, $p)
    "exit=$($r.ExitCode) $($r.Output -replace "`r?`n", ' ')"
}
$probePythonRead = {
    param($p)
    $code = 'import io,sys' + "`n" +
            'print(io.open(sys.argv[1],"r",encoding="utf-8",errors="replace").read())'
    $r = Invoke-Native -FilePath $script:Python -Arguments @('-c', $code, $p)
    "exit=$($r.ExitCode) $($r.Output)"
}

Invoke-Step 'parity matrix' {
    if (-not (Test-Path -LiteralPath $script:UncRoot)) {
        Add-Skip 'parity matrix' 'the UNC target went away'
        return
    }

    # The corpus is built through the UNC path so it exists regardless of
    # whether the alias works; every case then asks whether the alias sees the
    # same thing.
    if (Test-Path -LiteralPath $script:UncScratch) {
        Remove-Item -LiteralPath $script:UncScratch -Recurse -Force -ErrorAction SilentlyContinue
    }
    New-Item -ItemType Directory -Force -Path $script:UncScratch | Out-Null
    New-Item -ItemType Directory -Force -Path (Join-Path $script:UncScratch 'sub') | Out-Null
    Set-Content -LiteralPath (Join-Path $script:UncScratch 'plain.txt') -Value 'plain-content' -NoNewline -Encoding ascii
    Set-Content -LiteralPath (Join-Path $script:UncScratch 'sub\nested.txt') -Value 'nested' -NoNewline -Encoding ascii

    $unicodeName = [string][char]0x00FC + 'n' + [string][char]0x00EF + 'c' + [string][char]0x00F6 + 'd' +
        [string][char]0x00E9 + '-' + [string][char]0x65E5 + [string][char]0x672C + [string][char]0x8A9E + '-' +
        [string][char]0x03A9 + [string][char]0x03BC + '.txt'
    Set-Content -LiteralPath (Join-Path $script:UncScratch $unicodeName) -Value 'unicode-content' -NoNewline -Encoding utf8

    $aliasDirectory = $script:AliasScratch
    $uncDirectory = $script:UncScratch

    # -- (i) PowerShell FileSystem provider ---------------------------------
    Test-Parity -Method 'ps' -Name 'Test-Path container (distribution root)' -AliasPath $script:AliasRoot -UncPath $script:UncRoot -Body $probePsContainer | Out-Null
    Test-Parity -Method 'ps' -Name 'Test-Path container (scratch)' -AliasPath $aliasDirectory -UncPath $uncDirectory -Body $probePsContainer | Out-Null
    Test-Parity -Method 'ps' -Name 'Test-Path file' -AliasPath (Join-Path $aliasDirectory 'plain.txt') -UncPath (Join-Path $uncDirectory 'plain.txt') -Body $probePsExists | Out-Null
    Test-Parity -Method 'ps' -Name 'Get-ChildItem enumerate' -AliasPath $aliasDirectory -UncPath $uncDirectory -Body $probePsEnumerate | Out-Null
    Test-Parity -Method 'ps' -Name 'Get-Content read' -AliasPath (Join-Path $aliasDirectory 'plain.txt') -UncPath (Join-Path $uncDirectory 'plain.txt') -Body $probePsRead | Out-Null
    Test-Parity -Method 'ps' -Name 'Get-Item metadata' -AliasPath (Join-Path $aliasDirectory 'plain.txt') -UncPath (Join-Path $uncDirectory 'plain.txt') -Body $probePsAttributes | Out-Null
    Test-Parity -Method 'ps' -Name 'unicode name' -AliasPath (Join-Path $aliasDirectory $unicodeName) -UncPath (Join-Path $uncDirectory $unicodeName) -Body $probePsRead | Out-Null
    Test-Parity -Method 'ps' -Name 'missing file (same failure)' -AliasPath (Join-Path $aliasDirectory 'no-such-file') -UncPath (Join-Path $uncDirectory 'no-such-file') -Body $probePsExists | Out-Null

    # Write through the alias, read through the UNC path: the strongest single
    # statement that the two names are the same file.
    $writeName = 'written-through-alias.txt'
    $writeAlias = Join-Path $aliasDirectory $writeName
    $writeUnc = Join-Path $uncDirectory $writeName
    $wroteThrough = $false
    try {
        Set-Content -LiteralPath $writeAlias -Value 'round-trip' -NoNewline -Encoding ascii
        $wroteThrough = (Test-Path -LiteralPath $writeUnc) -and ((Get-Content -LiteralPath $writeUnc -Raw) -ceq 'round-trip')
    } catch {
        $wroteThrough = $false
    }
    Assert-True $wroteThrough '[ps] Set-Content through the alias is visible through the UNC path' | Out-Null

    # New-Item / Rename-Item / Remove-Item through the alias, observed through
    # the UNC path.
    $created = $false; $renamed = $false; $removed = $false
    try {
        New-Item -ItemType File -Path (Join-Path $aliasDirectory 'created.txt') -Force | Out-Null
        $created = Test-Path -LiteralPath (Join-Path $uncDirectory 'created.txt')
        Rename-Item -LiteralPath (Join-Path $aliasDirectory 'created.txt') -NewName 'renamed.txt'
        $renamed = (Test-Path -LiteralPath (Join-Path $uncDirectory 'renamed.txt')) -and
                   (-not (Test-Path -LiteralPath (Join-Path $uncDirectory 'created.txt')))
        Remove-Item -LiteralPath (Join-Path $aliasDirectory 'renamed.txt') -Force
        $removed = -not (Test-Path -LiteralPath (Join-Path $uncDirectory 'renamed.txt'))
    } catch {
        Write-Host "   note: mutation sequence threw: $($_.Exception.GetType().Name)"
    }
    Assert-True $created '[ps] New-Item through the alias' | Out-Null
    Assert-True $renamed '[ps] Rename-Item through the alias' | Out-Null
    Assert-True $removed '[ps] Remove-Item through the alias' | Out-Null

    # -- (ii) .NET System.IO -------------------------------------------------
    Test-Parity -Method 'net' -Name 'Directory.Exists' -AliasPath $aliasDirectory -UncPath $uncDirectory -Body $probeNetDirExists | Out-Null
    Test-Parity -Method 'net' -Name 'File.Exists' -AliasPath (Join-Path $aliasDirectory 'plain.txt') -UncPath (Join-Path $uncDirectory 'plain.txt') -Body $probeNetExists | Out-Null
    Test-Parity -Method 'net' -Name 'Directory.GetFileSystemEntries' -AliasPath $aliasDirectory -UncPath $uncDirectory -Body $probeNetEnumerate | Out-Null
    Test-Parity -Method 'net' -Name 'File.ReadAllText' -AliasPath (Join-Path $aliasDirectory 'plain.txt') -UncPath (Join-Path $uncDirectory 'plain.txt') -Body $probeNetRead | Out-Null
    Test-Parity -Method 'net' -Name 'FileInfo.Length' -AliasPath (Join-Path $aliasDirectory 'plain.txt') -UncPath (Join-Path $uncDirectory 'plain.txt') -Body $probeNetLength | Out-Null
    Test-Parity -Method 'net' -Name 'nested path' -AliasPath (Join-Path $aliasDirectory 'sub\nested.txt') -UncPath (Join-Path $uncDirectory 'sub\nested.txt') -Body $probeNetRead | Out-Null

    # -- (iii) Win32 CreateFileW --------------------------------------------
    Test-Parity -Method 'win32' -Name 'CreateFileW on the distribution root' -AliasPath $script:AliasRoot -UncPath $script:UncRoot -Body $probeWin32Open | Out-Null
    Test-Parity -Method 'win32' -Name 'CreateFileW on a file' -AliasPath (Join-Path $aliasDirectory 'plain.txt') -UncPath (Join-Path $uncDirectory 'plain.txt') -Body $probeWin32Open | Out-Null
    Test-Parity -Method 'win32' -Name 'CreateFileW error code on a missing file' -AliasPath (Join-Path $aliasDirectory 'no-such-file') -UncPath (Join-Path $uncDirectory 'no-such-file') -Body $probeWin32Open | Out-Null
    Test-Parity -Method 'win32' -Name 'GetFileAttributesW' -AliasPath (Join-Path $aliasDirectory 'plain.txt') -UncPath (Join-Path $uncDirectory 'plain.txt') -Body $probeWin32Attr | Out-Null

    # -- (iv) cmd.exe --------------------------------------------------------
    Test-Parity -Method 'cmd' -Name 'dir /b' -AliasPath $aliasDirectory -UncPath $uncDirectory -Body $probeCmdDir | Out-Null
    Test-Parity -Method 'cmd' -Name 'type' -AliasPath (Join-Path $aliasDirectory 'plain.txt') -UncPath (Join-Path $uncDirectory 'plain.txt') -Body $probeCmdType | Out-Null
    $cmdRoot = Invoke-Native -FilePath 'cmd.exe' -Arguments @('/c', 'dir', $script:AliasRoot)
    Assert-True ($cmdRoot.ExitCode -eq 0) "[cmd] dir $($script:AliasRoot) succeeds with no adapter installed" "(exit $($cmdRoot.ExitCode))" | Out-Null

    # -- (v) python ----------------------------------------------------------
    if ($null -eq $script:Python) {
        Add-Skip '[python] listing and read' 'no usable python.exe in the guest'
    } else {
        Test-Parity -Method 'python' -Name 'os.listdir' -AliasPath $aliasDirectory -UncPath $uncDirectory -Body $probePythonListing | Out-Null
        Test-Parity -Method 'python' -Name 'io.open read' -AliasPath (Join-Path $aliasDirectory 'plain.txt') -UncPath (Join-Path $uncDirectory 'plain.txt') -Body $probePythonRead | Out-Null
    }

    # -- long paths ----------------------------------------------------------
    # cmd and python have no \\?\ equivalent, so the long-path case is .NET and
    # Win32 only. The UNC side needs the \\?\UNC\ form, not \\?\\\server\share.
    $longSegment = 'l' * 60
    $longRelative = "$longSegment\$longSegment\$longSegment\$longSegment\deep.txt"
    $longUncPlain = Join-Path $uncDirectory $longRelative
    $longAliasPlain = Join-Path $aliasDirectory $longRelative
    $longUncExtended = '\\?\UNC\' + $longUncPlain.Substring(2)
    $longAliasExtended = '\\?\' + $longAliasPlain
    $longReady = $false
    try {
        [void][System.IO.Directory]::CreateDirectory((Split-Path -Parent $longUncExtended))
        [System.IO.File]::WriteAllText($longUncExtended, 'long-content')
        $longReady = $true
    } catch {
        Add-Skip 'long path (> 260 characters)' "the corpus could not be created: $($_.Exception.GetType().Name)"
    }
    if ($longReady) {
        Write-Host "   long path length: alias $($longAliasPlain.Length), unc $($longUncPlain.Length) characters"
        Test-Parity -Method 'net' -Name 'long path read (\\?\ prefixed)' -AliasPath $longAliasExtended -UncPath $longUncExtended -Body $probeNetRead | Out-Null
        Test-Parity -Method 'win32' -Name 'long path CreateFileW (\\?\ prefixed)' -AliasPath $longAliasExtended -UncPath $longUncExtended -Body $probeWin32Open | Out-Null
    }

    # -- trailing dot / trailing space ---------------------------------------
    # Documented semantics: Win32 strips a trailing dot and trailing spaces from
    # a path component before the request reaches the filesystem, so
    # 'name.' opens 'name'. The assertion is that the ALIAS behaves the same way
    # the UNC path does - not that such a name is usable. A divergence here
    # would mean the driver rewrote a name after or before normalization
    # differently from the redirector.
    foreach ($odd in @('trailing.dot.', 'trailing.space ')) {
        $oddUnc = Join-Path $uncDirectory $odd
        $oddAlias = Join-Path $aliasDirectory $odd
        try {
            [System.IO.File]::WriteAllText('\\?\UNC\' + $oddUnc.Substring(2), 'odd-content')
        } catch {
            # Best effort; the parity comparison below is still meaningful
            # because both sides then fail the same way.
            Write-Host "   note: could not create '$odd' through \\?\UNC\ ($($_.Exception.GetType().Name))"
        }
        Test-Parity -Method 'win32' -Name "trailing-character name '$odd' (Win32 normalization)" -AliasPath $oddAlias -UncPath $oddUnc -Body $probeWin32Open | Out-Null
        Test-Parity -Method 'net' -Name "trailing-character name '$odd' (\\?\ literal)" `
            -AliasPath ('\\?\' + $oddAlias) -UncPath ('\\?\UNC\' + $oddUnc.Substring(2)) -Body $probeNetExists | Out-Null
    }

    if ($FakeShare) {
        Add-Skip 'case-sensitive directory, symlink and Linux permission parity' 'the fake SMB share has no 9P semantics; needs a real WSL distribution'
    }
}

# ===================================================== d. identity rules ======
Write-Banner 'd' 'Identity rules'

$probeFile = Join-Path $script:AliasScratch 'plain.txt'
$identityOutput = Join-Path $WorkDirectory 'identity'
New-Item -ItemType Directory -Force -Path $identityOutput | Out-Null

function Wait-ForFile {
    param([string]$Path, [int]$TimeoutSeconds = 30)
    return Wait-ForCondition -TimeoutSeconds $TimeoutSeconds -Condition { Test-Path -LiteralPath $Path }
}

Invoke-Step 'identity rules' {
    $elevated = Test-Elevated

    # -- the harness's own token --------------------------------------------
    $selfRedirected = ([FswLab.Native]::OpenStatus($probeFile) -eq 0)
    $selfLabel = if ($elevated) { 'elevated (high integrity)' } else { 'standard (medium integrity)' }
    Assert-True $selfRedirected "the harness's own $selfLabel process is redirected" | Out-Null

    # -- the other integrity level ------------------------------------------
    $otherFile = Join-Path $identityOutput 'other-integrity.txt'
    if (Test-Path -LiteralPath $otherFile) { Remove-Item -LiteralPath $otherFile -Force }
    if ($elevated) {
        # From high integrity there is no supported way to drop to the
        # interactive medium-integrity token directly; a LIMITED scheduled task
        # running interactively as the logged-on user is the documented route.
        $taskName = 'FswLabStandardProbe'
        $command = "cmd.exe /c dir `"$($script:AliasRoot)`" > `"$otherFile`" 2>&1"
        $user = "$env:USERDOMAIN\$env:USERNAME"
        $create = Invoke-Native -FilePath 'schtasks.exe' -Arguments @(
            '/create', '/tn', $taskName, '/tr', $command, '/sc', 'once', '/st', '00:00',
            '/ru', $user, '/it', '/rl', 'LIMITED', '/f')
        if ($create.ExitCode -ne 0) {
            Add-Skip 'a standard-integrity process is redirected' "schtasks /create returned $($create.ExitCode): $($create.Output)"
        } else {
            Invoke-Native -FilePath 'schtasks.exe' -Arguments @('/run', '/tn', $taskName) | Out-Null
            if (Wait-ForFile -Path $otherFile -TimeoutSeconds 30) {
                $text = Get-Content -LiteralPath $otherFile -Raw
                Assert-True ($text -notmatch '(?i)File Not Found|cannot find|Not Found') 'a standard-integrity process is redirected' "($($text.Trim()))" | Out-Null
            } else {
                Add-Skip 'a standard-integrity process is redirected' 'the LIMITED task produced no output (is an interactive user logged on?)'
            }
            Invoke-Native -FilePath 'schtasks.exe' -Arguments @('/delete', '/tn', $taskName, '/f') | Out-Null
        }
    } else {
        Add-Skip 'an elevated process is redirected' 'the harness is not elevated; re-run elevated, or launch the matrix with Start-Process -Verb RunAs and compare by hand'
    }

    # -- AppContainer: must NOT be redirected --------------------------------
    if (-not (Get-Command -Name 'Invoke-CommandInDesktopPackage' -ErrorAction SilentlyContinue)) {
        Add-Skip 'an AppContainer process is NOT redirected' 'Invoke-CommandInDesktopPackage is not available in this guest'
    } else {
        $appContainerFile = Join-Path $identityOutput 'appcontainer.txt'
        if (Test-Path -LiteralPath $appContainerFile) { Remove-Item -LiteralPath $appContainerFile -Force }
        $package = $null
        try {
            $package = Get-AppxPackage -ErrorAction Stop |
                Where-Object { -not $_.IsFramework -and -not $_.IsResourcePackage } |
                Select-Object -First 1
        } catch {
            $package = $null
        }
        $appId = $null
        if ($null -ne $package) {
            try {
                $manifest = Get-AppxPackageManifest -Package $package.PackageFullName -ErrorAction Stop
                $appId = $manifest.Package.Applications.Application.Id | Select-Object -First 1
            } catch {
                $appId = $null
            }
        }
        if ($null -eq $package -or [string]::IsNullOrWhiteSpace($appId)) {
            Add-Skip 'an AppContainer process is NOT redirected' 'no packaged application with an Id was found to host the probe'
        } else {
            try {
                Invoke-CommandInDesktopPackage -PackageFamilyName $package.PackageFamilyName -AppId $appId `
                    -Command 'cmd.exe' -Args "/c dir `"$($script:AliasRoot)`" > `"$appContainerFile`" 2>&1" -ErrorAction Stop
                if (Wait-ForFile -Path $appContainerFile -TimeoutSeconds 20) {
                    $text = Get-Content -LiteralPath $appContainerFile -Raw
                    $notRedirected = ($text -match '(?i)File Not Found|cannot find|Not Found') -or ($script:AliasNativeBaseline)
                    Assert-True $notRedirected 'an AppContainer process is NOT redirected' "($($text.Trim()))" | Out-Null
                } else {
                    Add-Skip 'an AppContainer process is NOT redirected' 'the package context produced no output (it may be denied write access to the work directory)'
                }
            } catch {
                Add-Skip 'an AppContainer process is NOT redirected' "Invoke-CommandInDesktopPackage failed: $($_.Exception.Message)"
            }
        }
    }

    # -- SYSTEM: must NOT be redirected (session 0, excluded by policy) ------
    $systemFile = Join-Path $identityOutput 'system.txt'
    if (Test-Path -LiteralPath $systemFile) { Remove-Item -LiteralPath $systemFile -Force }
    $systemTask = 'FswLabSystemProbe'
    $systemCommand = "cmd.exe /c dir `"$($script:AliasRoot)`" > `"$systemFile`" 2>&1"
    $createSystem = Invoke-Native -FilePath 'schtasks.exe' -Arguments @(
        '/create', '/tn', $systemTask, '/tr', $systemCommand, '/sc', 'once', '/st', '00:00',
        '/ru', 'SYSTEM', '/rl', 'HIGHEST', '/f')
    if ($createSystem.ExitCode -ne 0) {
        Add-Skip 'a SYSTEM process is NOT redirected' "schtasks /create returned $($createSystem.ExitCode): $($createSystem.Output)"
    } else {
        Invoke-Native -FilePath 'schtasks.exe' -Arguments @('/run', '/tn', $systemTask) | Out-Null
        if (Wait-ForFile -Path $systemFile -TimeoutSeconds 30) {
            $text = Get-Content -LiteralPath $systemFile -Raw
            $notRedirected = ($text -match '(?i)File Not Found|cannot find|Not Found') -or ($script:AliasNativeBaseline)
            Assert-True $notRedirected 'a SYSTEM process is NOT redirected' "($($text.Trim()))" | Out-Null
        } else {
            Add-Skip 'a SYSTEM process is NOT redirected' 'the SYSTEM task produced no output'
        }
        Invoke-Native -FilePath 'schtasks.exe' -Arguments @('/delete', '/tn', $systemTask, '/f') | Out-Null
    }
}

# ==================================================== e. broker lifecycle =====
Write-Banner 'e' 'Broker lifecycle and port hardening'

function Test-AliasResolves {
    return [bool](Test-Path -LiteralPath $script:AliasRoot)
}

Invoke-Step 'broker lifecycle' {
    if ($null -eq $script:Cli) {
        Add-Skip 'fwdslash stop clears the mapping' 'no CLI'
        Add-Skip 'fwdslash start restores the mapping' 'no CLI'
        Add-Skip 'a broker crash clears the mapping' 'no CLI'
    } else {
        Invoke-Native -FilePath $script:Cli -Arguments @('stop') | Out-Null
        $cleared = Wait-ForCondition -TimeoutSeconds 10 -Condition { (Test-AliasResolves) -eq $script:AliasNativeBaseline }
        Assert-True $cleared 'fwdslash stop returns the alias to its native behaviour' "(baseline was $($script:AliasNativeBaseline))" | Out-Null

        Invoke-Native -FilePath $script:Cli -Arguments @('start') | Out-Null
        $restored = Wait-ForCondition -TimeoutSeconds 15 -Condition { Test-AliasResolves }
        Assert-True $restored 'fwdslash start restores the mapping' | Out-Null

        $broker = @(Get-Process -Name 'fswbroker' -ErrorAction SilentlyContinue)
        if ($broker.Count -eq 0) {
            Add-Skip 'a broker crash clears the mapping' 'fswbroker is not running'
        } else {
            $broker | Stop-Process -Force
            $clearedAfterCrash = Wait-ForCondition -TimeoutSeconds 10 -Condition { (Test-AliasResolves) -eq $script:AliasNativeBaseline }
            Assert-True $clearedAfterCrash 'a killed broker clears its slot (disconnect, not a graceful clear)' | Out-Null
            Invoke-Native -FilePath $script:Cli -Arguments @('start') | Out-Null
            Wait-ForCondition -TimeoutSeconds 15 -Condition { Test-AliasResolves } | Out-Null
        }
    }

    # -- malformed messages --------------------------------------------------
    $messageSize = [FswLab.Port]::MessageSize
    Write-Host "   FSW_MAPPING_MESSAGE size: $messageSize bytes"

    $malformed = @(
        @{ Name = 'wrong input length';       Buffer = (New-MappingBuffer); Declared = ($messageSize - 4) },
        @{ Name = 'wrong protocol version';   Buffer = (New-MappingBuffer -Version 99); Declared = $messageSize },
        @{ Name = 'Reserved is not zero';     Buffer = (New-MappingBuffer -Reserved 1); Declared = $messageSize },
        @{ Name = 'DistributionCount > 32';   Buffer = (New-MappingBuffer -Count 33); Declared = $messageSize },
        @{ Name = 'name contains a backslash'; Buffer = (New-MappingBuffer -FirstName 'Ub\untu'); Declared = $messageSize }
    )
    $portReachable = $true
    foreach ($case in $malformed) {
        $hr = 0
        try {
            $hr = [FswLab.Port]::SendRaw($script:PortName, $case.Buffer, $case.Declared)
        } catch {
            Add-Skip "malformed message rejected: $($case.Name)" "the port could not be opened: $($_.Exception.Message)"
            $portReachable = $false
            continue
        }
        if ($hr -eq -2147024891) {
            # E_ACCESSDENIED: the connect itself was refused, not the message.
            Add-Skip "malformed message rejected: $($case.Name)" 'the driver refused the harness connection (access denied)'
            $portReachable = $false
            continue
        }
        Assert-True ($hr -lt 0) "malformed message rejected: $($case.Name)" ("(hr=0x{0:X8}, expected a failure)" -f $hr) | Out-Null
    }
    Assert-True $true 'the machine is still running after the malformed-message set' | Out-Null

    # -- slot accounting -----------------------------------------------------
    if (-not $portReachable) {
        Add-Skip 'a closed connection frees its slot (17 sequential connections)' 'the port refused the harness'
    } else {
        $hr = [FswLab.Port]::ConnectCycle($script:PortName, 17)
        # 17 is one more than FSW_MAX_INTERACTIVE_SESSIONS. Sequentially - each
        # closed before the next - every one must succeed; a driver that leaked
        # a slot per connection would fail at the 17th.
        Assert-True ($hr -ge 0) 'the 17th sequential connection still succeeds (no slot leak)' ("(hr=0x{0:X8})" -f $hr) | Out-Null
    }
}

# ================================================== f. unload under load ======
Write-Banner 'f' 'Unload under load'

Invoke-Step 'unload under load' {
    $loadTarget = $probeFile
    if (-not (Test-Path -LiteralPath $script:AliasRoot)) { $loadTarget = 'C:\Windows\System32\ntdll.dll' }
    $loop = @"
`$deadline = (Get-Date).AddSeconds(20)
while ((Get-Date) -lt `$deadline) {
    try { [System.IO.File]::Exists('$loadTarget') | Out-Null } catch { }
    try { Get-ChildItem -LiteralPath '$($script:AliasRoot)' -ErrorAction SilentlyContinue | Out-Null } catch { }
}
"@
    $worker = Start-Process -FilePath 'powershell.exe' `
        -ArgumentList @('-NoProfile', '-NonInteractive', '-Command', $loop) `
        -WindowStyle Hidden -PassThru
    Start-Sleep -Seconds 2

    $unload = Invoke-Native -FilePath 'fltmc.exe' -Arguments @('unload', $script:FilterName)
    Assert-True ($unload.ExitCode -eq 0) 'fltmc unload while a create loop is running' "(exit $($unload.ExitCode): $($unload.Output))" | Out-Null
    $script:DriverLoaded = ($unload.ExitCode -ne 0)

    try { $worker | Wait-Process -Timeout 40 -ErrorAction SilentlyContinue } catch { }
    if (-not $worker.HasExited) { $worker | Stop-Process -Force -ErrorAction SilentlyContinue }
    Assert-True $true 'the machine survived the unload (no bugcheck)' | Out-Null

    $reload = Invoke-Native -FilePath 'fltmc.exe' -Arguments @('load', $script:FilterName)
    $script:DriverLoaded = ($reload.ExitCode -eq 0)
    Assert-True $script:DriverLoaded 'the filter reloads after an unload under load' "(exit $($reload.ExitCode): $($reload.Output))" | Out-Null
    if ($script:DriverLoaded -and $null -ne $script:Cli) {
        # The broker republishes on its 5 s health tick; give it two.
        Wait-ForCondition -TimeoutSeconds 15 -Condition { Test-AliasResolves } | Out-Null
    }

    Add-Skip 'sleep/resume' 'a manual step: suspend the guest (or Set-VM -AutomaticStopAction Save + Suspend-VM), resume, then re-run steps c and e'
}

# ======================================================== g. benchmark ========
Write-Banner 'g' 'Create-rate benchmark on a NON-matching path (informational)'

Invoke-Step 'benchmark' {
    $benchmarkPath = $null
    foreach ($candidate in @('C:\Windows\notepad.exe', 'C:\Windows\System32\notepad.exe', 'C:\Windows\System32\ntdll.dll')) {
        if (Test-Path -LiteralPath $candidate) { $benchmarkPath = $candidate; break }
    }
    if ($null -eq $benchmarkPath) {
        Add-Skip 'create-rate benchmark' 'no non-matching benchmark target was found'
        return
    }

    if (-not $script:DriverLoaded) {
        $reload = Invoke-Native -FilePath 'fltmc.exe' -Arguments @('load', $script:FilterName)
        $script:DriverLoaded = ($reload.ExitCode -eq 0)
    }
    if (-not $script:DriverLoaded) {
        Add-Skip 'create-rate benchmark' 'the filter is not loaded'
        return
    }

    $loadedMs = [FswLab.Native]::MeasureCreateMilliseconds($benchmarkPath, $Iterations)
    $unload = Invoke-Native -FilePath 'fltmc.exe' -Arguments @('unload', $script:FilterName)
    if ($unload.ExitCode -ne 0) {
        Add-Skip 'create-rate benchmark (unloaded half)' "fltmc unload returned $($unload.ExitCode)"
        return
    }
    $script:DriverLoaded = $false
    $unloadedMs = [FswLab.Native]::MeasureCreateMilliseconds($benchmarkPath, $Iterations)

    $reload = Invoke-Native -FilePath 'fltmc.exe' -Arguments @('load', $script:FilterName)
    $script:DriverLoaded = ($reload.ExitCode -eq 0)

    $loadedRate = 0.0
    $unloadedRate = 0.0
    if ($loadedMs -gt 0) { $loadedRate = $Iterations / ($loadedMs / 1000.0) }
    if ($unloadedMs -gt 0) { $unloadedRate = $Iterations / ($unloadedMs / 1000.0) }
    $delta = 0.0
    if ($unloadedRate -gt 0) { $delta = (($loadedRate - $unloadedRate) / $unloadedRate) * 100.0 }

    Write-Host ("   iterations : {0}" -f $Iterations)
    Write-Host ("   loaded     : {0:N0} opens/s ({1:N1} ms total)" -f $loadedRate, $loadedMs)
    Write-Host ("   unloaded   : {0:N0} opens/s ({1:N1} ms total)" -f $unloadedRate, $unloadedMs)
    Write-Host ("   delta      : {0:N2} %% (negative means the filter costs throughput)" -f $delta)
    Write-Host '   Informational only - there is no threshold. Record it in docs/compatibility.md.'
    Add-Pass 'create-rate benchmark completed'
}

# ========================================================= h. teardown ========
Write-Banner 'h' 'Teardown'

Invoke-Step 'teardown' {
    if ($null -ne $script:Cli) {
        Invoke-Native -FilePath $script:Cli -Arguments @('stop') | Out-Null
    }

    if ($KeepInstalled) {
        Add-Skip 'teardown' '-KeepInstalled was given; restore the guest checkpoint when you are done'
        return
    }

    if ($script:DriverLoaded) {
        $unload = Invoke-Native -FilePath 'fltmc.exe' -Arguments @('unload', $script:FilterName)
        Assert-True ($unload.ExitCode -eq 0) 'fltmc unload' "(exit $($unload.ExitCode): $($unload.Output))" | Out-Null
        $script:DriverLoaded = ($unload.ExitCode -ne 0)
    } else {
        Write-Host '   the filter was already unloaded'
    }

    if ($null -eq $script:PublishedName) {
        $enumerated = Invoke-Native -FilePath 'pnputil.exe' -Arguments @('/enum-drivers')
        $blocks = $enumerated.Output -split '(?m)^\s*$'
        foreach ($block in $blocks) {
            if ($block -match '(?i)fswfilter\.inf' -and $block -match '(?im)Published\s+Name\s*:\s*(oem\d+\.inf)') {
                $script:PublishedName = $Matches[1]
                break
            }
        }
    }
    if ($null -eq $script:PublishedName) {
        Add-Skip 'pnputil /delete-driver' 'the published oemNN.inf name could not be determined; restore the checkpoint'
    } else {
        $delete = Invoke-Native -FilePath 'pnputil.exe' -Arguments @('/delete-driver', $script:PublishedName, '/uninstall', '/force')
        Assert-True ($delete.ExitCode -eq 0) "pnputil /delete-driver $($script:PublishedName) /uninstall /force" "(exit $($delete.ExitCode): $($delete.Output))" | Out-Null
    }

    # Nothing else is restored on purpose: the guest checkpoint is the undo.
    Write-Host '   nothing else is restored - Restore-VMSnapshot -Name clean-os is the undo for the rest.'
}

# --------------------------------------------------------------- bugcheck ----
Invoke-Step 'bugcheck check' {
    $unexpected = @()
    try {
        $unexpected = @(Get-WinEvent -FilterHashtable @{ LogName = 'System'; Id = 41; StartTime = $script:StartTime } -ErrorAction SilentlyContinue)
    } catch {
        $unexpected = @()
    }
    Assert-True ($unexpected.Count -eq 0) 'no unexpected-shutdown (Kernel-Power 41) event during the run' "(found $($unexpected.Count))" | Out-Null
}

# ---------------------------------------------------------------- summary ----
Write-Host ''
Write-Host '=== Summary'
Write-Host "    passed  : $($script:Passed)"
Write-Host "    failed  : $($script:Failed)"
Write-Host "    skipped : $($script:Skipped)"
if ($FakeShare) {
    Write-Host ''
    Write-Host '    -FakeShare run. This proves the reparse mechanism, not WSL semantics.'
    Write-Host '    A docs/compatibility.md row may not be marked verified from this run alone.'
}
if ($script:Skipped -gt 0) {
    Write-Host ''
    Write-Host '    Skipped steps are not evidence. A gate row stays pending until its step passes.'
}
Write-Host ''
if ($script:Failed -gt 0) {
    Write-Host "$($script:Failed) check(s) FAILED. Restore the clean-os checkpoint before the next run."
    exit 1
}
Write-Host 'All executed checks passed. Restore the clean-os checkpoint before the next run.'
exit 0
