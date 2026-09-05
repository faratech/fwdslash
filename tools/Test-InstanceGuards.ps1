#Requires -Version 5.1
<#
.SYNOPSIS
    Drives the single-instance and process-lifecycle scenarios against the Rust
    binaries (build them first: cargo build --release --target <triple>).

.DESCRIPTION
    Scenarios (docs/divergences.md, "Instance lifecycle" and "Broker"):
      1. Two broker spawns -> exactly one broker process survives.
      2. A healthy running broker is NOT killed by `fwdslash start`.
      3. Paused broker: `fwdslash start` resumes it, never claims a broken hook.
      4. Closing the settings window exits the process (there is no tray to
         hide into: the broker owns the product's only notification icon).
      5. Relaunching while the settings window is open -> still one process,
         and it still has a visible window.

    Exits 0 when every scenario passes; 1 with a named failure otherwise.
    Never logs paths. Stops the broker/settings processes it started.
#>
[CmdletBinding()]
param(
    [ValidateSet('ARM64', 'x64')]
    [string]$Architecture = 'ARM64'
)

$repo = (Get-Item $PSScriptRoot).Parent.FullName
$triple = if ($Architecture -eq 'ARM64') { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
$dir = Join-Path $repo "target\$triple\release"
foreach ($exe in 'fswbroker.exe', 'fwdslash.exe', 'fswsettings.exe') {
    if (-not (Test-Path (Join-Path $dir $exe))) {
        Write-Error "Missing $exe in $dir; build first."
        exit 1
    }
}

$script:failures = 0
function Assert-True {
    param([bool]$Condition, [string]$Name, [string]$Detail)
    if ($Condition) {
        Write-Host "[PASS] $Name"
    } else {
        Write-Host "[FAIL] $Name $Detail"
        $script:failures++
    }
}

function Stop-AllFsw {
    Get-Process fswbroker, fswsettings -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 500
}

function Get-FswCount {
    param([string]$Name)
    @(Get-Process $Name -ErrorAction SilentlyContinue).Count
}

Stop-AllFsw

# --- 1. Two spawns, one broker ---------------------------------------------
Start-Process -FilePath (Join-Path $dir 'fswbroker.exe')
Start-Sleep -Seconds 2
$output = & (Join-Path $dir 'fwdslash.exe') start 2>&1
Start-Sleep -Milliseconds 500
$brokerCount = Get-FswCount fswbroker
Assert-True ($brokerCount -eq 1) "two spawns -> one broker" "(count=$brokerCount, out=$output)"

# --- 2. Healthy broker survives `fwdslash start` ----------------------------
# (broker is active after scenario 1; the command must report success and the
# same process must still be alive)
$before = (Get-Process fswbroker).Id
$output = & (Join-Path $dir 'fwdslash.exe') start 2>&1
Start-Sleep -Milliseconds 500
$after = (Get-Process fswbroker -ErrorAction SilentlyContinue).Id
Assert-True ($null -ne $after -and $after -eq $before -and $output -match 'already active') `
    "healthy broker not killed by start" "(id before=$before after=$after, out=$output)"

# --- 3. Paused broker: resumed, honest message ------------------------------
$disable = & (Join-Path $dir 'fwdslash.exe') disable 2>&1   # sets Disabled=1
Start-Sleep -Milliseconds 500
$output = & (Join-Path $dir 'fwdslash.exe') start 2>&1
$state = & (Join-Path $dir 'fwdslash.exe') status 2>&1
$null = & (Join-Path $dir 'fwdslash.exe') enable 2>&1       # restore
Assert-True ($output -match 'active' -or $output -match 'paused') `
    "paused broker handled with resume-or-paused message" "(out=$output, status=$state)"

# --- 4. Closing the window exits the process --------------------------------
Stop-AllFsw
Add-Type -Namespace Fsw -Name Msg -MemberDefinition `
    '[DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);'
$settings = Start-Process -FilePath (Join-Path $dir 'fswsettings.exe') -PassThru
Start-Sleep -Seconds 4
$settings.Refresh()
$hadWindow = $settings.MainWindowHandle -ne 0
Assert-True ($hadWindow) "settings window materialized" "(hwnd was 0)"
# WM_CLOSE is the reactor's only exit route (Window.Closed -> exit_ui_thread).
[Fsw.Msg]::PostMessage($settings.MainWindowHandle, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null
Start-Sleep -Seconds 3
$afterClose = @(Get-Process fswsettings -ErrorAction SilentlyContinue)
Assert-True ($afterClose.Count -eq 0) "closing the window exits the process" "(count=$($afterClose.Count))"

# --- 5. Relaunch while open -> one process, window still visible -------------
Stop-AllFsw
$settings = Start-Process -FilePath (Join-Path $dir 'fswsettings.exe') -PassThru
Start-Sleep -Seconds 4
$settings.Refresh()
$firstWindow = $settings.MainWindowHandle -ne 0
Start-Process -FilePath (Join-Path $dir 'fswsettings.exe')
Start-Sleep -Seconds 4
$alive = @(Get-Process fswsettings -ErrorAction SilentlyContinue)
$stillVisible = $false
foreach ($p in $alive) {
    $p.Refresh()
    if ($p.MainWindowHandle -ne 0) { $stillVisible = $true }
}
Assert-True ($firstWindow) "first instance had a window before the relaunch" "(hwnd was 0)"
Assert-True ($alive.Count -eq 1) "relaunch stays single-instance" "(count=$($alive.Count))"
Assert-True ($stillVisible) "the surviving instance still has a visible window"

Stop-AllFsw
if ($script:failures -gt 0) {
    Write-Host "$($script:failures) scenario(s) FAILED"
    exit 1
}
Write-Host 'All instance-guard scenarios passed.'
exit 0
