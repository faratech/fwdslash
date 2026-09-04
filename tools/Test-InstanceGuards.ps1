#Requires -Version 5.1
<#
.SYNOPSIS
    Drives the single-instance and tray-lifecycle scenarios against the Rust
    binaries (build them first: cargo build --release --target <triple>).

.DESCRIPTION
    Scenarios (docs/divergences.md, "Instance lifecycle" and "Broker"):
      1. Two broker spawns -> exactly one broker process survives.
      2. A healthy running broker is NOT killed by `fwdslash start`.
      3. Paused broker: `fwdslash start` resumes it, never claims a broken hook.
      4. Close-to-tray + relaunch -> still one process, window re-raised.
      5. Windowless zombie (FSW_SIMULATE_WINDOWLESS) -> takeover by a relaunch.

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

# --- 4. Close-to-tray + relaunch -> one process, window raised ---------------
Stop-AllFsw
$settings = Start-Process -FilePath (Join-Path $dir 'fswsettings.exe') -PassThru
Start-Sleep -Seconds 4
$settings.Refresh()
$hadWindow = $settings.MainWindowHandle -ne 0
# Simulate the user closing the window: WM_CLOSE hides it to the tray.
Add-Type -Namespace Fsw -Name Msg -MemberDefinition `
    '[DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);'
[Fsw.Msg]::PostMessage($settings.MainWindowHandle, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null  # WM_CLOSE
Start-Sleep -Seconds 2
$firstPid = (Get-Process fswsettings -ErrorAction SilentlyContinue).Id
# A second launch of the settings app hits the guard and must re-raise.
Start-Process -FilePath (Join-Path $dir 'fswsettings.exe')
Start-Sleep -Seconds 4
$count = Get-FswCount fswsettings
$stillVisible = $false
foreach ($p in Get-Process fswsettings -ErrorAction SilentlyContinue) {
    $p.Refresh()
    if ($p.MainWindowHandle -ne 0) { $stillVisible = $true }
}
Assert-True ($hadWindow) "settings window materialized" "(hwnd was 0)"
Assert-True ($count -eq 1) "relaunch against tray-hidden window stays single" "(count=$count)"
Assert-True ($stillVisible) "relaunch re-raised the hidden window"

# --- 5. Windowless zombie takeover ------------------------------------------
Stop-AllFsw
$env:FSW_SIMULATE_WINDOWLESS = '1'
$zombie = Start-Process -FilePath (Join-Path $dir 'fswsettings.exe') -PassThru `
    -WindowStyle Hidden
Start-Sleep -Seconds 2
$zombieCount = Get-FswCount fswsettings
Remove-Item Env:\FSW_SIMULATE_WINDOWLESS
# The takeover guard only kills peers older than 15 s (a young peer may be a
# legitimate concurrent launch), so let the zombie age past the threshold.
Start-Sleep -Seconds 14
# The relaunch inherits the (now clean) environment and takes over.
Start-Process -FilePath (Join-Path $dir 'fswsettings.exe')
# Second instance: ~10 s activation poll, then the takeover scan, kill, mutex
# acquisition, and WinUI window materialization. Give the whole path time.
Start-Sleep -Seconds 17
foreach ($p in Get-Process fswsettings -ErrorAction SilentlyContinue) {
    Write-Host "    [diag] fswsettings pid=$($p.Id) hwnd=$($p.MainWindowHandle) start=$($p.StartTime.ToString('HH:mm:ss'))"
}
$alive = @(Get-Process fswsettings -ErrorAction SilentlyContinue)
$zombieGone = $alive.Id -notcontains $zombie.Id
$oneLeft = $alive.Count -eq 1
$visible = $false
foreach ($p in $alive) {
    $p.Refresh()
    if ($p.MainWindowHandle -ne 0) { $visible = $true }
}
Assert-True ($zombieCount -eq 1) "windowless holder started (test fixture)" "(count=$zombieCount)"
Assert-True ($zombieGone) "zombie terminated by takeover" "(zombie pid=$($zombie.Id) alive=$($alive.Id -join ','))"
Assert-True ($oneLeft) "exactly one instance after takeover" "(count=$($alive.Count))"
Assert-True ($visible) "takeover instance has a visible window"

Stop-AllFsw
if ($script:failures -gt 0) {
    Write-Host "$($script:failures) scenario(s) FAILED"
    exit 1
}
Write-Host 'All instance-guard scenarios passed.'
exit 0
