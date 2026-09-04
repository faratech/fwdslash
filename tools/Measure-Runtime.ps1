#Requires -Version 5.1
<#
.SYNOPSIS
    Measures runtime behavior (startup latency, idle RAM, idle CPU, CLI cold
    start, settings launch-to-window) of the C++ and Rust binaries and prints
    a side-by-side comparison.

.DESCRIPTION
    The runtime counterpart to docs/size-baseline.md's on-disk table. Both
    sides must be built first:

      C++:  .\tools\Build-UserMode.ps1 -Architecture <arch> -Configuration ReleaseCpp
            (ReleaseCpp, NOT Release: out\user\<arch>\Release currently holds
            Rust exes staged by tools/package_msix.py and building over them
            destroys that staging)
      Rust: cargo build --release --target <rust-triple> --workspace

    Never logs paths: all output is metric numbers and fixed labels only.

.PARAMETER Architecture
    ARM64 (default) or x64.

.PARAMETER CppConfiguration
    Configuration directory under out\user\<arch> holding the C++ binaries.

.PARAMETER AppendDocs
    Appends a "Runtime baseline" markdown section to docs/size-baseline.md.

.PARAMETER SkipSettings
    Skips the settings-app launch measurements (the C++ settings app needs a
    working Windows App SDK runtime).

.NOTES
    PowerShell gotchas honored here: no $PSScriptRoot in param() defaults
    (resolved in the body), and loop variables never shadow parameters.
#>
[CmdletBinding()]
param(
    [ValidateSet('ARM64', 'x64')]
    [string]$Architecture = 'ARM64',

    [string]$CppConfiguration = 'ReleaseCpp',

    [switch]$AppendDocs,

    [switch]$SkipSettings
)

$repo = (Get-Item $PSScriptRoot).Parent.FullName
$targetName = $Architecture.ToLowerInvariant()
$cppDir = Join-Path $repo "out\user\$targetName\$CppConfiguration"
$rustTriple = if ($Architecture -eq 'ARM64') { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
$rustDir = Join-Path $repo "target\$rustTriple\release"
$docsPath = Join-Path $repo 'docs\size-baseline.md'

foreach ($dir in @($cppDir, $rustDir)) {
    if (-not (Test-Path (Join-Path $dir 'fswbroker.exe'))) {
        Write-Error "Missing fswbroker.exe under $dir; build both sides first (see script header)."
        return
    }
}

Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class FswWin32 {
    public delegate bool EnumProc(IntPtr hwnd, IntPtr lparam);
    [DllImport("user32.dll")]
    public static extern bool EnumWindows(EnumProc callback, IntPtr lparam);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)]
    public static extern int GetClassName(IntPtr hwnd, StringBuilder name, int count);
    // Class scan rather than FindWindow: empirically the EnumWindows route is
    // the reliable one from an interop-spawned host, and it sees both the Rust
    // broker's top-level window and the C++ broker's message-only one.
    public static bool ClassWindowExists(string className) {
        bool found = false;
        EnumWindows(delegate(IntPtr hwnd, IntPtr lparam) {
            var name = new StringBuilder(256);
            GetClassName(hwnd, name, 256);
            if (name.ToString() == className) { found = true; return false; }
            return true;
        }, IntPtr.Zero);
        return found;
    }
}
'@

# The well-known broker window class, shared with include/fsw_user_protocol.h.
$brokerClass = 'ForwardSlashWindows.Broker'

function Stop-Brokers {
    Get-Process fswbroker -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 400
}

function Get-BrokerWindowPresent {
    [FswWin32]::ClassWindowExists($brokerClass)
}

# ---------------------------------------------------------------------------
# Metric: broker startup latency (CreateProcess -> class window enumerable).
# Note on the C++ side: its window is message-only (HWND_MESSAGE), which no
# enumeration route from this host can see (FindWindow/FindWindowEx from an
# interop PowerShell and EnumChildWindows from HWND_MESSAGE were all tried);
# WaitForInputIdle never fires for a hidden-window process either. The C++
# broker's own CLI finds it fine -- a 1 s-granularity probe, useless for
# latency -- so expect n/a there and treat the Rust number as the product's.
# ---------------------------------------------------------------------------
function Measure-BrokerStartup {
    param([string]$Directory, [int]$Count)

    $latencies = New-Object System.Collections.Generic.List[double]
    for ($run = 1; $run -le $Count; $run++) {
        Stop-Brokers
        $clock = [System.Diagnostics.Stopwatch]::StartNew()
        $proc = Start-Process -FilePath (Join-Path $Directory 'fswbroker.exe') -PassThru
        $found = $false
        while ($clock.ElapsedMilliseconds -lt 10000) {
            if (Get-BrokerWindowPresent) { $found = $true; break }
            Start-Sleep -Milliseconds 5
        }
        $clock.Stop()
        if ($found) { $latencies.Add($clock.Elapsed.TotalMilliseconds) }
        Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
    }
    Stop-Brokers
    if ($latencies.Count -eq 0) { return $null }
    $sorted = $latencies | Sort-Object
    [pscustomobject]@{
        Median = $sorted[[int]([math]::Floor($sorted.Count / 2))]
        Min    = $sorted[0]
        Max    = $sorted[-1]
    }
}

# ---------------------------------------------------------------------------
# Metric: idle RAM + idle CPU of the resident broker
# ---------------------------------------------------------------------------
function Measure-BrokerIdle {
    param([string]$Directory)

    Stop-Brokers
    $proc = Start-Process -FilePath (Join-Path $Directory 'fswbroker.exe') -PassThru
    Start-Sleep -Seconds 5   # settle: first-chance allocations, tray, UIA init
    $proc.Refresh()
    $workingSet = $proc.WorkingSet64
    $private = $proc.PrivateMemorySize64
    $cpuBefore = $proc.TotalProcessorTime
    $wall = [System.Diagnostics.Stopwatch]::StartNew()
    Start-Sleep -Seconds 10  # idle window: hook installed, nothing happening
    $wall.Stop()
    $proc.Refresh()
    $cpuDelta = ($proc.TotalProcessorTime - $cpuBefore).TotalMilliseconds
    $alive = -not $proc.HasExited
    Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
    if (-not $alive) { return $null }
    [pscustomobject]@{
        WorkingSetMB    = [math]::Round($workingSet / 1MB, 2)
        PrivateMB       = [math]::Round($private / 1MB, 2)
        IdleCpuPercent  = [math]::Round(($cpuDelta / $wall.Elapsed.TotalMilliseconds) * 100, 3)
    }
}

# ---------------------------------------------------------------------------
# Metric: CLI cold start (spawn -> exit; the shell adapters do this per dir)
# ---------------------------------------------------------------------------
function Measure-CliColdStart {
    param([string]$Directory, [int]$Count)

    $times = New-Object System.Collections.Generic.List[double]
    for ($run = 1; $run -le $Count; $run++) {
        # .NET Process, not Start-Process -Wait: Start-Process -Wait carries
        # ~1,045 ms of PowerShell overhead regardless of the child (measured
        # 1,044 ms vs 17 ms for the same exe), which once made this row
        # report the harness instead of the CLI.
        $psi = [System.Diagnostics.ProcessStartInfo]::new()
        $psi.FileName = Join-Path $Directory 'fwdslash.exe'
        $psi.Arguments = 'status'
        $psi.UseShellExecute = $false
        $psi.RedirectStandardOutput = $true
        $psi.CreateNoWindow = $true
        $clock = [System.Diagnostics.Stopwatch]::StartNew()
        $proc = [System.Diagnostics.Process]::Start($psi)
        $null = $proc.StandardOutput.ReadToEnd()   # drain so a full pipe cannot block exit
        $proc.WaitForExit()
        $clock.Stop()
        if ($proc.ExitCode -eq 0) { $times.Add($clock.Elapsed.TotalMilliseconds) }
    }
    if ($times.Count -eq 0) { return $null }
    $sorted = $times | Sort-Object
    [pscustomobject]@{ Median = $sorted[[int]([math]::Floor($sorted.Count / 2))] }
}

# ---------------------------------------------------------------------------
# Metric: settings app launch-to-window (poll MainWindowHandle)
# ---------------------------------------------------------------------------
function Measure-SettingsLaunch {
    param([string]$Directory, [int]$Count)

    $settingsExe = Join-Path $Directory 'fswsettings.exe'
    if (-not (Test-Path $settingsExe)) {
        Write-Host "    (no fswsettings.exe in $Directory; skipping)"
        return $null
    }

    $times = New-Object System.Collections.Generic.List[double]
    for ($run = 1; $run -le $Count; $run++) {
        Get-Process fswsettings -ErrorAction SilentlyContinue | Stop-Process -Force
        Start-Sleep -Milliseconds 400
        $clock = [System.Diagnostics.Stopwatch]::StartNew()
        $proc = Start-Process -FilePath (Join-Path $Directory 'fswsettings.exe') -PassThru
        $shown = $false
        while ($clock.ElapsedMilliseconds -lt 30000) {
            $proc.Refresh()
            if ($proc.MainWindowHandle -ne 0 -and -not $proc.HasExited) { $shown = $true; break }
            Start-Sleep -Milliseconds 20
        }
        $clock.Stop()
        if ($shown) { $times.Add($clock.Elapsed.TotalMilliseconds) }
        Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
    }
    Get-Process fswsettings -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    if ($times.Count -eq 0) { return $null }
    $sorted = $times | Sort-Object
    [pscustomobject]@{ Median = $sorted[[int]([math]::Floor($sorted.Count / 2))] }
}

# ---------------------------------------------------------------------------

$results = [ordered]@{}
foreach ($side in @(
    [pscustomobject]@{ Label = 'C++';  Dir = $cppDir },
    [pscustomobject]@{ Label = 'Rust'; Dir = $rustDir }
)) {
    Write-Host "=== Measuring C++/$($side.Label): broker startup x10 ==="
    $startup = Measure-BrokerStartup -Directory $side.Dir -Count 10
    Write-Host "=== Measuring $($side.Label): broker idle RAM/CPU ==="
    $idle = Measure-BrokerIdle -Directory $side.Dir
    Write-Host "=== Measuring $($side.Label): CLI cold start x20 ==="
    $cli = Measure-CliColdStart -Directory $side.Dir -Count 20
    $settings = $null
    if (-not $SkipSettings) {
        Write-Host "=== Measuring $($side.Label): settings launch-to-window x5 ==="
        $settings = Measure-SettingsLaunch -Directory $side.Dir -Count 5
    }
    $results[$side.Label] = [pscustomobject]@{
        Startup  = $startup
        Idle     = $idle
        Cli      = $cli
        Settings = $settings
    }
}

function Format-SideValue {
    param($Value, [string]$Unit)
    if ($null -eq $Value) { 'n/a' } else { '{0:N2} {1}' -f $Value, $Unit }
}

$rows = @(
    [pscustomobject]@{
        Metric = 'Broker startup to window (median, 10 runs)'
        Cpp    = Format-SideValue $results['C++'].Startup.Median  'ms'
        Rust   = Format-SideValue $results['Rust'].Startup.Median 'ms'
    }
    [pscustomobject]@{
        Metric = 'Broker idle working set (5 s settle)'
        Cpp    = Format-SideValue $results['C++'].Idle.WorkingSetMB 'MB'
        Rust   = Format-SideValue $results['Rust'].Idle.WorkingSetMB 'MB'
    }
    [pscustomobject]@{
        Metric = 'Broker idle private bytes'
        Cpp    = Format-SideValue $results['C++'].Idle.PrivateMB 'MB'
        Rust   = Format-SideValue $results['Rust'].Idle.PrivateMB 'MB'
    }
    [pscustomobject]@{
        Metric = 'Broker idle CPU (10 s window, % of one core)'
        Cpp    = Format-SideValue $results['C++'].Idle.IdleCpuPercent '%'
        Rust   = Format-SideValue $results['Rust'].Idle.IdleCpuPercent '%'
    }
    [pscustomobject]@{
        Metric = 'CLI cold start `fwdslash status` (median, 20 runs)'
        Cpp    = Format-SideValue $results['C++'].Cli.Median 'ms'
        Rust   = Format-SideValue $results['Rust'].Cli.Median 'ms'
    }
    [pscustomobject]@{
        Metric = 'Settings launch to window (median, 5 runs)'
        Cpp    = Format-SideValue $results['C++'].Settings.Median 'ms'
        Rust   = Format-SideValue $results['Rust'].Settings.Median 'ms'
    }
)

$table = $rows | Format-Table -AutoSize | Out-String
Write-Host $table

if ($AppendDocs) {
    $stamp = Get-Date -Format 'yyyy-MM-dd'
    $section = @"

## Runtime baseline, measured

Produced $stamp by ``tools\Measure-Runtime.ps1 -Architecture $Architecture``
(C++ side from ``out\user\$targetName\$CppConfiguration``, Rust side from
``target\$rustTriple\release``). Idle CPU is the broker's total-processor-time
delta across a 10 s window with nothing happening, as a percentage of one core.

| Metric | C++ | Rust |
|---|---:|---:|
$(($rows | ForEach-Object { "| $($_.Metric) | $($_.Cpp) | $($_.Rust) |" }) -join "`n")
"@
    Add-Content -Path $docsPath -Value $section
    Write-Host "Appended runtime baseline to $docsPath"
}
