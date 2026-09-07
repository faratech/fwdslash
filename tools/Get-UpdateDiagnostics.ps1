<#
.SYNOPSIS
Collects everything the self-updater leaves behind on a machine, for a bug
report or a stuck-install investigation (issue #140).

.DESCRIPTION
Read-only. Prints the installed package, the product's processes, the
updater's scheduled tasks, the attempt lock and helper result file, the update
settings, the Store's own install queue for the product, and the last few
hours of Store and AppX deployment events that mention the package. Nothing
here needs elevation.

.PARAMETER Hours
How far back to read the event logs. Default 12.

.EXAMPLE
.\tools\Get-UpdateDiagnostics.ps1 | Set-Content update-diagnostics.txt
#>
[CmdletBinding()]
param(
    [int]$Hours = 12
)

$ErrorActionPreference = 'Continue'
$identity = '32827MikeFara.fwdslash'
$productId = '9P51CM0MTMK2'
$since = (Get-Date).AddHours(-$Hours)

function Section([string]$Title) { ''; "=== $Title ==="; }

Section 'Package'
Get-AppxPackage -Name $identity | Select-Object Name, Version, Architecture, PackageFamilyName, SignatureKind, InstallLocation | Format-List

Section 'Processes'
Get-Process -Name fwdslash, fwdslash-helper, fswbroker, fswsettings -ErrorAction SilentlyContinue |
    Select-Object Name, Id, StartTime, @{n = 'Path'; e = { $_.Path } } | Format-Table -AutoSize

Section 'Scheduled tasks (fwdslash-update*)'
foreach ($task in Get-ScheduledTask -ErrorAction SilentlyContinue | Where-Object TaskName -like 'fwdslash-update*') {
    $info = $task | Get-ScheduledTaskInfo
    [pscustomobject]@{
        TaskName       = $task.TaskName
        State          = $task.State
        Command        = ($task.Actions | ForEach-Object { $_.Execute }) -join ' '
        LastRunTime    = $info.LastRunTime
        LastTaskResult = ('0x{0:X8}' -f $info.LastTaskResult)
        NextRunTime    = $info.NextRunTime
    }
}

Section 'Update directory'
$updateDir = Join-Path $env:LOCALAPPDATA 'ForwardSlashWindows\update'
if (Test-Path -LiteralPath $updateDir) {
    Get-ChildItem -LiteralPath $updateDir | Select-Object Name, Length, LastWriteTime | Format-Table -AutoSize
    foreach ($name in 'update-attempt.lock', 'last-result.txt') {
        $file = Join-Path $updateDir $name
        if (Test-Path -LiteralPath $file) { "$name = '$(Get-Content -LiteralPath $file -Raw)'" }
    }
} else { "(absent: $updateDir)" }

Section 'Task scripts left in %LOCALAPPDATA%\Temp'
Get-ChildItem -Path (Join-Path $env:LOCALAPPDATA 'Temp') -Filter 'fwdslash-update*' -ErrorAction SilentlyContinue |
    Select-Object Name, Length, LastWriteTime | Format-Table -AutoSize

Section 'Update settings (HKCU\Software\ForwardSlashWindows\Settings)'
$settings = Get-ItemProperty -Path 'HKCU:\Software\ForwardSlashWindows\Settings' -ErrorAction SilentlyContinue
if ($settings) {
    $settings | Select-Object AutoUpdate, LastUpdateCheck, AvailableUpdate, UpdateRoute, Disabled | Format-List
    if ($settings.LastUpdateCheck) { 'LastUpdateCheck (local) = ' + [DateTimeOffset]::FromUnixTimeSeconds([int64]$settings.LastUpdateCheck).LocalDateTime }
} else { '(no settings key)' }

Section "Store install queue for $productId"
try {
    [Windows.ApplicationModel.Store.Preview.InstallControl.AppInstallManager, Windows.ApplicationModel.Store.Preview.InstallControl, ContentType = WindowsRuntime] | Out-Null
    $manager = New-Object Windows.ApplicationModel.Store.Preview.InstallControl.AppInstallManager
    $items = @($manager.AppInstallItems)
    "queue length = $($items.Count)"
    $rows = foreach ($item in $items) {
        $status = $item.GetCurrentStatus()
        [pscustomobject]@{
            ProductId  = $item.ProductId
            Family     = $item.PackageFamilyName
            Type       = $item.InstallType
            State      = $status.InstallState
            Percent    = $status.PercentComplete
            Bytes      = $status.BytesDownloaded
            Error      = ('0x{0:X8}' -f $status.ErrorCode.GetHashCode())
        }
    }
    $rows | Format-Table -AutoSize
} catch { "AppInstallManager unavailable: $($_.Exception.Message)" }

Section "Store events since $since (Microsoft-Windows-Store/Operational)"
Get-WinEvent -FilterHashtable @{ LogName = 'Microsoft-Windows-Store/Operational'; StartTime = $since } -ErrorAction SilentlyContinue |
    Where-Object { $_.Id -ne 8001 -and $_.Message -match "$identity|$productId|fwdslash" } |
    Sort-Object TimeCreated |
    ForEach-Object { '{0:yyyy-MM-dd HH:mm:ss} {1,5} {2}' -f $_.TimeCreated, $_.Id, (($_.Message -replace '\s+', ' ').Substring(0, [Math]::Min(220, $_.Message.Length))) }

Section "AppX deployment events since $since"
Get-WinEvent -FilterHashtable @{ LogName = 'Microsoft-Windows-AppXDeployment-Server/Operational'; StartTime = $since } -ErrorAction SilentlyContinue |
    Where-Object { $_.Message -match $identity } |
    Sort-Object TimeCreated |
    ForEach-Object { '{0:yyyy-MM-dd HH:mm:ss} {1,5} {2}' -f $_.TimeCreated, $_.Id, (($_.Message -replace '\s+', ' ').Substring(0, [Math]::Min(220, $_.Message.Length))) }

Section 'Task Scheduler events (fwdslash-update*)'
Get-WinEvent -FilterHashtable @{ LogName = 'Microsoft-Windows-TaskScheduler/Operational'; StartTime = $since } -ErrorAction SilentlyContinue |
    Where-Object { $_.Message -match 'fwdslash-update' } |
    Sort-Object TimeCreated |
    ForEach-Object { '{0:yyyy-MM-dd HH:mm:ss} {1,5} {2}' -f $_.TimeCreated, $_.Id, (($_.Message -replace '\s+', ' ').Substring(0, [Math]::Min(160, $_.Message.Length))) }
