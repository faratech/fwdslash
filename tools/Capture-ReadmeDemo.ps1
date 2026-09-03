[CmdletBinding()]
param(
    [ValidateRange(0, 30)]
    [int]$CountdownSeconds = 3,

    [ValidateRange(30, 900)]
    [int]$TimeoutSeconds = 600,

    [string]$OutputDirectory,

    [switch]$SkipGif
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

Add-Type -AssemblyName System.Windows.Forms, System.Drawing
Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

[StructLayout(LayoutKind.Sequential)]
public struct FswRect {
    public int Left;
    public int Top;
    public int Right;
    public int Bottom;
}

public static class ReadmeCaptureNative {
    public delegate bool EnumProc(IntPtr hwnd, IntPtr lParam);

    [DllImport("user32.dll")]
    public static extern bool EnumWindows(EnumProc proc, IntPtr lParam);

    [DllImport("user32.dll")]
    public static extern IntPtr GetForegroundWindow();

    [DllImport("user32.dll")]
    public static extern short GetAsyncKeyState(int virtualKey);

    [DllImport("user32.dll")]
    public static extern bool IsWindow(IntPtr hwnd);

    [DllImport("user32.dll")]
    public static extern bool IsWindowVisible(IntPtr hwnd);

    [DllImport("user32.dll")]
    public static extern bool GetWindowRect(IntPtr hwnd, out FswRect rect);

    [DllImport("user32.dll")]
    public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint processId);

    [DllImport("dwmapi.dll")]
    public static extern int DwmGetWindowAttribute(IntPtr hwnd, int attribute, out FswRect value, int size);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern int GetClassNameW(IntPtr hwnd, StringBuilder value, int count);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern int GetWindowTextW(IntPtr hwnd, StringBuilder value, int count);

    [DllImport("user32.dll")]
    public static extern bool MoveWindow(IntPtr hwnd, int x, int y, int width, int height, bool repaint);

    [DllImport("user32.dll")]
    public static extern bool PostMessageW(IntPtr hwnd, uint message, IntPtr wParam, IntPtr lParam);

    [DllImport("user32.dll")]
    public static extern bool SetForegroundWindow(IntPtr hwnd);

    [DllImport("user32.dll")]
    public static extern bool SetProcessDPIAware();

    [DllImport("user32.dll")]
    public static extern bool ShowWindow(IntPtr hwnd, int command);

    [DllImport("user32.dll")]
    public static extern void keybd_event(byte key, byte scan, uint flags, UIntPtr extra);

    public static void TapWindowsKey() {
        const byte virtualKey = 0x5B;
        keybd_event(virtualKey, 0, 0, UIntPtr.Zero);
        keybd_event(virtualKey, 0, 2, UIntPtr.Zero);
    }

    public static IntPtr[] Windows(string requiredClass, bool visibleOnly) {
        var result = new List<IntPtr>();
        EnumWindows((hwnd, unused) => {
            if (visibleOnly && !IsWindowVisible(hwnd)) return true;
            var value = new StringBuilder(256);
            GetClassNameW(hwnd, value, value.Capacity);
            if (String.IsNullOrEmpty(requiredClass) ||
                String.Equals(value.ToString(), requiredClass, StringComparison.Ordinal)) {
                result.Add(hwnd);
            }
            return true;
        }, IntPtr.Zero);
        return result.ToArray();
    }

    public static uint ProcessId(IntPtr hwnd) {
        uint processId = 0;
        GetWindowThreadProcessId(hwnd, out processId);
        return processId;
    }

    public static string WindowTitle(IntPtr hwnd) {
        var value = new StringBuilder(512);
        GetWindowTextW(hwnd, value, value.Capacity);
        return value.ToString();
    }
}
'@

[ReadmeCaptureNative]::SetProcessDPIAware() | Out-Null

$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$bin = Join-Path $repo 'out\user\arm64\Release'
$controller = Join-Path $bin 'fwdslash.exe'
$settings = Join-Path $bin 'fswsettings.exe'
if (-not $OutputDirectory) {
    $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
    $OutputDirectory = Join-Path $repo "out\readme-demo\$stamp"
}

if (-not (Test-Path -LiteralPath $controller -PathType Leaf)) {
    throw "Build output not found: $controller"
}
if (-not (Test-Path -LiteralPath $settings -PathType Leaf)) {
    throw "Build output not found: $settings"
}

$integrationState = (& $controller integrations 2>&1 | Out-String)
foreach ($required in @(
    'Windows surfaces: installed',
    'Command Prompt: installed',
    'Windows PowerShell: installed'
)) {
    if ($integrationState -notmatch [regex]::Escape($required)) {
        throw "The README demo requires '$required'. Current state:`n$integrationState"
    }
}

# The demo types bare slash paths such as /usr, so the opt-in bare-slash mode has
# to be the one that resolves them against the default distribution.
$bareSlashState = (& $controller bare-slash 2>&1 | Out-String)
if ($bareSlashState -notmatch 'bare slash mode: default distribution') {
    throw "The README demo requires bare-slash default-distribution mode. Current state:`n$bareSlashState"
}

# Spawned consoles inherit this, so the demo can type "fwdslash" the way the
# documentation writes it.
$env:PATH = "$bin;$env:PATH"

# Consoles start here so the recorded prompt is short and says nothing about the
# machine it was recorded on.
$consoleHome = "$($env:SystemDrive)\"

$createdNew = $false
$mutex = New-Object System.Threading.Mutex($true, 'Local\ForwardSlashWindowsReadmeCapture', [ref]$createdNew)
if (-not $createdNew) {
    $mutex.Dispose()
    throw 'Another README capture is already running.'
}

# The capture region is in physical pixels. Every scene must cover it completely
# so the recording never includes the operator's wallpaper or desktop.
$width = 1440
$height = 900
$frameMs = 80
$typeFramesPerCharacter = 2
$captureSize = New-Object System.Drawing.Size($width, $height)
$chromeColor = [System.Drawing.Color]::FromArgb(25, 25, 25)
$backdropColor = [System.Drawing.Color]::FromArgb(22, 22, 26)

$script:Frame = 0
$script:Deadline = (Get-Date).AddSeconds($TimeoutSeconds)
$script:OwnedWindows = New-Object System.Collections.Generic.List[System.IntPtr]
$script:OwnedProcesses = New-Object System.Collections.Generic.List[System.Diagnostics.Process]
$script:FocusGuard = [IntPtr]::Zero
$script:AllowedProcessIds = $null
$script:SafetyStop = $false
$script:EscapeHeld = $false
$script:Explorer = [IntPtr]::Zero
$script:OriginX = 0
$script:OriginY = 0
$script:Masks = @()
$script:RegistryRestores = New-Object System.Collections.Generic.List[hashtable]
$script:ClearedKeys = New-Object System.Collections.Generic.List[hashtable]
$desktopWasMinimized = $false
$backdrop = $null
$shell = $null
$originalForeground = [ReadmeCaptureNative]::GetForegroundWindow()

# Clear the "pressed since last call" bit so a keystroke from before the run
# cannot abort the capture the moment it starts.
[ReadmeCaptureNative]::GetAsyncKeyState(0x1B) | Out-Null

function Stop-ForSafety {
    param([string]$Reason)

    $script:SafetyStop = $true
    throw $Reason
}

function Assert-Continue {
    if ((Get-Date) -ge $script:Deadline) {
        Stop-ForSafety "Capture exceeded its $TimeoutSeconds-second safety timeout."
    }

    # Only the "currently held" bit. The low bit means "pressed since the last
    # call", which is sticky and system wide, so a keystroke in an unrelated
    # window used to abort the run. Two consecutive polls are required because
    # the broker taps Escape itself after it routes a search.
    if (([ReadmeCaptureNative]::GetAsyncKeyState(0x1B) -band 0x8000) -ne 0) {
        if ($script:EscapeHeld) {
            Stop-ForSafety 'Capture cancelled with Esc.'
        }
        $script:EscapeHeld = $true
    } else {
        $script:EscapeHeld = $false
    }

    if ($script:FocusGuard -ne [IntPtr]::Zero) {
        if (-not [ReadmeCaptureNative]::IsWindow($script:FocusGuard)) {
            Stop-ForSafety 'Capture target closed unexpectedly.'
        }
        if ([ReadmeCaptureNative]::GetForegroundWindow() -ne $script:FocusGuard) {
            Stop-ForSafety 'Capture stopped because focus left its owned demo window.'
        }
    } elseif ($null -ne $script:AllowedProcessIds) {
        # Start and Search own their own windows, so here the guard is the set of
        # processes allowed to receive the keystrokes rather than one handle.
        $foreground = [ReadmeCaptureNative]::GetForegroundWindow()
        if ($foreground -eq [IntPtr]::Zero) {
            Stop-ForSafety 'Capture stopped because no window has focus.'
        }
        if (-not $script:AllowedProcessIds.ContainsKey(
                [ReadmeCaptureNative]::ProcessId($foreground))) {
            Stop-ForSafety 'Capture stopped because focus left the Windows search surface.'
        }
    }
}

function Wait-Safely {
    param([int]$Milliseconds)

    $until = (Get-Date).AddMilliseconds($Milliseconds)
    while ((Get-Date) -lt $until) {
        Assert-Continue
        [System.Windows.Forms.Application]::DoEvents()
        $remaining = ($until - (Get-Date)).TotalMilliseconds
        if ($remaining -le 0) { break }
        Start-Sleep -Milliseconds ([int][Math]::Min(40, [Math]::Max(1, $remaining)))
    }
}

function Clear-Guards {
    $script:FocusGuard = [IntPtr]::Zero
    $script:AllowedProcessIds = $null
}

function Use-FocusGuard {
    param([IntPtr]$Hwnd)

    if ($Hwnd -eq [IntPtr]::Zero -or -not [ReadmeCaptureNative]::IsWindow($Hwnd)) {
        throw 'Refusing to focus an invalid capture window.'
    }
    Clear-Guards
    [ReadmeCaptureNative]::ShowWindow($Hwnd, 9) | Out-Null
    [ReadmeCaptureNative]::SetForegroundWindow($Hwnd) | Out-Null
    Wait-Safely 350
    if ([ReadmeCaptureNative]::GetForegroundWindow() -ne $Hwnd) {
        throw 'Could not safely acquire the capture window.'
    }
    $script:FocusGuard = $Hwnd
}

function Use-ProcessGuard {
    param([string[]]$ProcessNames)

    $allowed = @{}
    foreach ($name in $ProcessNames) {
        foreach ($process in @(Get-Process -Name $name -ErrorAction SilentlyContinue)) {
            $allowed[[uint32]$process.Id] = $true
        }
    }
    if ($allowed.Count -eq 0) {
        throw "None of these processes are running: $($ProcessNames -join ', ')."
    }
    $foreground = [ReadmeCaptureNative]::GetForegroundWindow()
    if ($foreground -eq [IntPtr]::Zero -or
        -not $allowed.ContainsKey([ReadmeCaptureNative]::ProcessId($foreground))) {
        throw 'The expected Windows surface did not take focus.'
    }
    Clear-Guards
    $script:AllowedProcessIds = $allowed
}

function Set-CaptureOrigin {
    param([int]$X = 0, [int]$Y = 0)

    $screen = [System.Windows.Forms.SystemInformation]::VirtualScreen
    $script:OriginX = [Math]::Max(0, [Math]::Min($X, $screen.Width - $width))
    $script:OriginY = [Math]::Max(0, [Math]::Min($Y, $screen.Height - $height))
}

# Rectangles are painted over each frame before it is written. They cover window
# chrome that lists the operator's own folders and accounts, which has no place
# in a published recording.
function Set-PrivacyMasks {
    param([hashtable[]]$Rectangles = @())

    $script:Masks = $Rectangles
}

# Places a window so that its *visible* frame lands on the requested rectangle.
# MoveWindow works on the legacy window rect, which on DWM includes an invisible
# resize border; without this correction every scene shows a sliver of desktop.
function Set-WindowFrame {
    param(
        [IntPtr]$Hwnd,
        [int]$X,
        [int]$Y,
        [int]$W,
        [int]$H
    )

    [ReadmeCaptureNative]::MoveWindow($Hwnd, $X, $Y, $W, $H, $true) | Out-Null
    Wait-Safely 140

    $windowRect = New-Object FswRect
    $frameRect = New-Object FswRect
    if (-not [ReadmeCaptureNative]::GetWindowRect($Hwnd, [ref]$windowRect)) { return }
    if ([ReadmeCaptureNative]::DwmGetWindowAttribute($Hwnd, 9, [ref]$frameRect, 16) -ne 0) { return }

    $dx = $windowRect.Left - $frameRect.Left
    $dy = $windowRect.Top - $frameRect.Top
    $dw = ($windowRect.Right - $windowRect.Left) - ($frameRect.Right - $frameRect.Left)
    $dh = ($windowRect.Bottom - $windowRect.Top) - ($frameRect.Bottom - $frameRect.Top)
    if ($dx -eq 0 -and $dy -eq 0 -and $dw -eq 0 -and $dh -eq 0) { return }

    [ReadmeCaptureNative]::MoveWindow($Hwnd, ($X + $dx), ($Y + $dy), ($W + $dw), ($H + $dh), $true) | Out-Null
    Wait-Safely 140
}

function Set-FullFrame {
    param([IntPtr]$Hwnd)

    Set-WindowFrame -Hwnd $Hwnd -X $script:OriginX -Y $script:OriginY -W $width -H $height
}

# Consoles snap to whole character cells, so ask for a slightly larger window and
# let the fixed capture rectangle crop it. That guarantees full coverage.
function Set-WindowCentered {
    param([IntPtr]$Hwnd)

    $frameRect = New-Object FswRect
    if ([ReadmeCaptureNative]::DwmGetWindowAttribute($Hwnd, 9, [ref]$frameRect, 16) -ne 0) { return }
    $w = $frameRect.Right - $frameRect.Left
    $h = $frameRect.Bottom - $frameRect.Top
    if ($w -le 0 -or $h -le 0) { return }
    Set-WindowFrame -Hwnd $Hwnd -X ($script:OriginX + [int](($width - $w) / 2)) `
        -Y ($script:OriginY + [int](($height - $h) / 2)) -W $w -H $h
}

function Set-ConsoleFrame {
    param([IntPtr]$Hwnd)

    Set-WindowFrame -Hwnd $Hwnd -X $script:OriginX -Y $script:OriginY -W ($width + 48) -H ($height + 48)
}

function Save-Bitmap {
    param([System.Drawing.Bitmap]$Bitmap)

    $script:Frame++
    $path = Join-Path $OutputDirectory ('f{0:D4}.png' -f $script:Frame)
    $Bitmap.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
}

function Save-Frame {
    Assert-Continue
    $bitmap = New-Object System.Drawing.Bitmap($width, $height)
    try {
        $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
        try {
            $graphics.CopyFromScreen($script:OriginX, $script:OriginY, 0, 0, $captureSize)
            foreach ($mask in $script:Masks) {
                $brush = New-Object System.Drawing.SolidBrush($mask.Color)
                try {
                    $graphics.FillRectangle($brush, $mask.X, $mask.Y, $mask.W, $mask.H)
                } finally {
                    $brush.Dispose()
                }
            }
        } finally {
            $graphics.Dispose()
        }
        Save-Bitmap $bitmap
    } finally {
        $bitmap.Dispose()
    }
}

# Records $Count frames on a fixed cadence. The GIF is rendered at a constant
# frame rate, so one frame always means one $frameMs slice of playback.
function Save-Frames {
    param([int]$Count)

    $start = Get-Date
    for ($index = 0; $index -lt $Count; $index++) {
        Save-Frame
        if ($index + 1 -lt $Count) {
            $due = $start.AddMilliseconds(($index + 1) * $frameMs)
            $remaining = ($due - (Get-Date)).TotalMilliseconds
            if ($remaining -gt 0) { Wait-Safely ([int]$remaining) } else { Assert-Continue }
        }
    }
}

function Save-TitleCard {
    param(
        [string]$Title,
        [string]$Subtitle,
        [string]$Details,
        [int]$Frames = 13
    )

    Assert-Continue
    $bitmap = New-Object System.Drawing.Bitmap($width, $height)
    try {
        $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
        try {
            $graphics.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::AntiAliasGridFit
            $graphics.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
            $graphics.Clear([System.Drawing.Color]::FromArgb(22, 22, 26))
            $accent = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::FromArgb(77, 166, 176))
            $secondary = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::FromArgb(176, 183, 192))
            $rule = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::FromArgb(58, 62, 70))
            $slashFont = New-Object System.Drawing.Font('Segoe UI', 135, [System.Drawing.FontStyle]::Bold)
            $titleFont = New-Object System.Drawing.Font('Segoe UI', 40, [System.Drawing.FontStyle]::Bold)
            $subtitleFont = New-Object System.Drawing.Font('Segoe UI', 21)
            $detailsFont = New-Object System.Drawing.Font('Segoe UI', 16)
            try {
                $graphics.DrawString('/', $slashFont, $accent, 52, 312)
                $graphics.FillRectangle($rule, 230, 432, 1150, 2)
                $graphics.DrawString($Title, $titleFont, [System.Drawing.Brushes]::White, 225, 360)
                $graphics.DrawString($Subtitle, $subtitleFont, $secondary, 230, 452)
                $graphics.DrawString($Details, $detailsFont, $secondary, 230, 496)
            } finally {
                $accent.Dispose()
                $secondary.Dispose()
                $rule.Dispose()
                $slashFont.Dispose()
                $titleFont.Dispose()
                $subtitleFont.Dispose()
                $detailsFont.Dispose()
            }
        } finally {
            $graphics.Dispose()
        }

        for ($index = 0; $index -lt $Frames; $index++) {
            Assert-Continue
            Save-Bitmap $bitmap
        }
    } finally {
        $bitmap.Dispose()
    }
}

function Send-CaptureKeys {
    param([string]$Keys)

    if ($script:FocusGuard -eq [IntPtr]::Zero -and $null -eq $script:AllowedProcessIds) {
        throw 'Refusing to send keys without an active guard.'
    }
    Assert-Continue
    [System.Windows.Forms.SendKeys]::SendWait($Keys)
}

# Types one character per beat and records every beat, so the recording shows the
# text appearing inside the box the way a person would see it. Address bars and
# the Run box auto-append a completion drawn from the operator's own history, so
# it is deleted after each character and never reaches a frame.
function Send-CaptureText {
    param(
        [string]$Text,
        [switch]$SuppressAutoComplete
    )

    foreach ($character in $Text.ToCharArray()) {
        Assert-Continue
        Send-CaptureKeys ([string]$character)
        if ($SuppressAutoComplete) {
            Wait-Safely 25
            Send-CaptureKeys '{DEL}'
        }
        Wait-Safely 30
        Save-Frames -Count $typeFramesPerCharacter
    }
}

function Submit-CaptureText {
    param(
        [int]$HoldFrames = 5,
        [int]$SettleFrames = 10
    )

    Save-Frames -Count $HoldFrames
    Send-CaptureKeys '{ENTER}'
    Wait-Safely 120
    Save-Frames -Count $SettleFrames
}

function Get-WindowSet {
    param(
        [string]$ClassName = '',
        [bool]$VisibleOnly = $false
    )

    $set = @{}
    foreach ($hwnd in [ReadmeCaptureNative]::Windows($ClassName, $VisibleOnly)) {
        $set[$hwnd.ToInt64()] = $true
    }
    return $set
}

function Test-WindowProcess {
    param(
        [IntPtr]$Hwnd,
        [string[]]$ProcessNames
    )

    if ($ProcessNames.Count -eq 0) { return $true }
    $process = Get-Process -Id ([int][ReadmeCaptureNative]::ProcessId($Hwnd)) -ErrorAction SilentlyContinue
    if ($null -eq $process) { return $false }
    return $ProcessNames -contains $process.ProcessName
}

# Returns a window this capture is responsible for: either one that did not exist
# before, or a preloaded hidden window that has just become visible and focused.
# Windows the operator already had on screen (including minimized ones, which stay
# WS_VISIBLE) are in $BeforeVisible and can never be adopted.
function Wait-NewWindow {
    param(
        [hashtable]$BeforeAll,
        [hashtable]$BeforeVisible,
        [string]$ClassName = '',
        [string[]]$ProcessNames = @(),
        [int]$Seconds = 15
    )

    $savedFocusGuard = $script:FocusGuard
    $savedProcessGuard = $script:AllowedProcessIds
    Clear-Guards
    try {
        $until = (Get-Date).AddSeconds($Seconds)
        while ((Get-Date) -lt $until) {
            Assert-Continue
            foreach ($hwnd in [ReadmeCaptureNative]::Windows($ClassName, $true)) {
                if (-not $BeforeAll.ContainsKey($hwnd.ToInt64()) -and
                    (Test-WindowProcess -Hwnd $hwnd -ProcessNames $ProcessNames)) {
                    return $hwnd
                }
            }
            $foreground = [ReadmeCaptureNative]::GetForegroundWindow()
            if ($foreground -ne [IntPtr]::Zero -and
                -not $BeforeVisible.ContainsKey($foreground.ToInt64()) -and
                ([ReadmeCaptureNative]::Windows($ClassName, $true) -contains $foreground) -and
                (Test-WindowProcess -Hwnd $foreground -ProcessNames $ProcessNames)) {
                return $foreground
            }
            Wait-Safely 100
        }
        return [IntPtr]::Zero
    } finally {
        $script:FocusGuard = $savedFocusGuard
        $script:AllowedProcessIds = $savedProcessGuard
    }
}

function Start-ClassicConsole {
    param(
        [string]$Executable,
        [string[]]$Arguments = @()
    )

    $beforeAll = Get-WindowSet -ClassName 'ConsoleWindowClass' -VisibleOnly $false
    $beforeVisible = Get-WindowSet -ClassName 'ConsoleWindowClass' -VisibleOnly $true
    $process = Start-Process conhost.exe -ArgumentList (@($Executable) + $Arguments) `
        -WorkingDirectory $consoleHome -PassThru
    $script:OwnedProcesses.Add($process)
    $hwnd = Wait-NewWindow -BeforeAll $beforeAll -BeforeVisible $beforeVisible -ClassName 'ConsoleWindowClass' -Seconds 15
    if ($hwnd -eq [IntPtr]::Zero) {
        throw "No console window appeared for $Executable."
    }
    $script:OwnedWindows.Add($hwnd)
    return $hwnd
}

# Drives the Settings NavigationView through UI Automation so the recording shows
# the real pages rather than a static window. Failure here is never fatal.
function Select-SettingsPage {
    param(
        [IntPtr]$Hwnd,
        [string]$Name
    )

    $root = [System.Windows.Automation.AutomationElement]::FromHandle($Hwnd)
    if ($null -eq $root) { throw 'No automation root for the Settings window.' }
    $condition = New-Object System.Windows.Automation.PropertyCondition(
        [System.Windows.Automation.AutomationElement]::NameProperty, $Name)
    $candidates = $root.FindAll([System.Windows.Automation.TreeScope]::Descendants, $condition)
    if ($null -eq $candidates -or $candidates.Count -eq 0) {
        throw "Settings page '$Name' was not found."
    }

    foreach ($item in $candidates) {
        $selectionPattern = $null
        if ($item.TryGetCurrentPattern(
                [System.Windows.Automation.SelectionItemPattern]::Pattern, [ref]$selectionPattern)) {
            $selectionPattern.Select()
            return
        }
        $invokePattern = $null
        if ($item.TryGetCurrentPattern(
                [System.Windows.Automation.InvokePattern]::Pattern, [ref]$invokePattern)) {
            $invokePattern.Invoke()
            return
        }
    }
    throw "Settings page '$Name' cannot be selected."
}

# Every value is recorded before it is changed and put back in the finally block,
# including when the operator aborts with Esc.
function Set-DemoRegistryValue {
    param(
        [string]$Path,
        [string]$Name,
        $Value,
        [Microsoft.Win32.RegistryValueKind]$Kind
    )

    if (-not (Test-Path -LiteralPath $Path)) {
        New-Item -Path $Path -Force | Out-Null
    }
    $existing = Get-ItemProperty -LiteralPath $Path -Name $Name -ErrorAction SilentlyContinue
    $script:RegistryRestores.Add(@{
        Path = $Path
        Name = $Name
        Existed = ($null -ne $existing)
        Value = $(if ($null -ne $existing) { $existing.$Name } else { $null })
        Kind = $Kind
    })
    New-ItemProperty -LiteralPath $Path -Name $Name -Value $Value -PropertyType $Kind -Force | Out-Null
}

function Restore-DemoRegistry {
    for ($index = $script:RegistryRestores.Count - 1; $index -ge 0; $index--) {
        $entry = $script:RegistryRestores[$index]
        try {
            if ($entry.Existed) {
                New-ItemProperty -LiteralPath $entry.Path -Name $entry.Name -Value $entry.Value `
                    -PropertyType $entry.Kind -Force | Out-Null
            } else {
                Remove-ItemProperty -LiteralPath $entry.Path -Name $entry.Name -ErrorAction SilentlyContinue
            }
        } catch {
            Write-Warning "Could not restore $($entry.Path)\$($entry.Name): $($_.Exception.Message)"
        }
    }
    $script:RegistryRestores.Clear()
}

function Save-AndClearRegistryValues {
    param([string]$Path)

    if (-not (Test-Path -LiteralPath $Path)) { return }
    $key = Get-Item -LiteralPath $Path
    $saved = @()
    foreach ($name in $key.GetValueNames()) {
        if ([string]::IsNullOrEmpty($name)) { continue }
        $saved += @{ Name = $name; Value = $key.GetValue($name); Kind = $key.GetValueKind($name) }
    }
    $script:ClearedKeys.Add(@{ Path = $Path; Values = $saved })
    foreach ($entry in $saved) {
        Remove-ItemProperty -LiteralPath $Path -Name $entry.Name -ErrorAction SilentlyContinue
    }
}

function Restore-ClearedRegistryValues {
    for ($index = $script:ClearedKeys.Count - 1; $index -ge 0; $index--) {
        $item = $script:ClearedKeys[$index]
        try {
            if (-not (Test-Path -LiteralPath $item.Path)) {
                New-Item -Path $item.Path -Force | Out-Null
            }
            # Drop whatever the demo itself typed before putting the originals back.
            $key = Get-Item -LiteralPath $item.Path
            foreach ($name in $key.GetValueNames()) {
                if (-not [string]::IsNullOrEmpty($name)) {
                    Remove-ItemProperty -LiteralPath $item.Path -Name $name -ErrorAction SilentlyContinue
                }
            }
            foreach ($entry in $item.Values) {
                New-ItemProperty -LiteralPath $item.Path -Name $entry.Name -Value $entry.Value `
                    -PropertyType $entry.Kind -Force | Out-Null
            }
        } catch {
            Write-Warning "Could not restore $($item.Path): $($_.Exception.Message)"
        }
    }
    $script:ClearedKeys.Clear()
}

function Set-DemoPrivacyState {
    $explorerAdvanced = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced'
    $sizer = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\Modules\GlobalSettings\Sizer'
    $searchSettings = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\SearchSettings'

    # Collapse the Explorer navigation pane, whose first four bytes are its width.
    # It lists pinned folders and the signed-in account.
    $existing = Get-ItemProperty -LiteralPath $sizer -Name 'PageSpaceControlSizer' -ErrorAction SilentlyContinue
    if ($existing -and $existing.PageSpaceControlSizer.Count -ge 4) {
        $collapsed = [byte[]]$existing.PageSpaceControlSizer.Clone()
        0..3 | ForEach-Object { $collapsed[$_] = 0 }
        Set-DemoRegistryValue -Path $sizer -Name 'PageSpaceControlSizer' -Value $collapsed `
            -Kind ([Microsoft.Win32.RegistryValueKind]::Binary)
    }

    # Empty the Start "Recommended" list and Quick access, which name recent files.
    Set-DemoRegistryValue -Path $explorerAdvanced -Name 'Start_TrackDocs' -Value 0 `
        -Kind ([Microsoft.Win32.RegistryValueKind]::DWord)
    Set-DemoRegistryValue -Path $explorerAdvanced -Name 'ShowRecent' -Value 0 `
        -Kind ([Microsoft.Win32.RegistryValueKind]::DWord)
    Set-DemoRegistryValue -Path $explorerAdvanced -Name 'ShowFrequent' -Value 0 `
        -Kind ([Microsoft.Win32.RegistryValueKind]::DWord)
    Set-DemoRegistryValue -Path $searchSettings -Name 'IsDeviceSearchHistoryEnabled' -Value 0 `
        -Kind ([Microsoft.Win32.RegistryValueKind]::DWord)

    # Emptied for the duration of the run and written back afterwards, including
    # the entries this demo adds itself.
    Save-AndClearRegistryValues 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\TypedPaths'
    Save-AndClearRegistryValues 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\RunMRU'
}

# A plain dark surface behind every scene, so the operator's wallpaper is never
# part of a frame and dialogs are recorded against a consistent background.
function Show-Backdrop {
    $screen = [System.Windows.Forms.SystemInformation]::VirtualScreen
    $form = New-Object System.Windows.Forms.Form
    $form.FormBorderStyle = [System.Windows.Forms.FormBorderStyle]::None
    $form.ShowInTaskbar = $false
    $form.StartPosition = [System.Windows.Forms.FormStartPosition]::Manual
    $form.Bounds = $screen
    $form.BackColor = $backdropColor
    $form.TopMost = $false
    $form.Show()
    $form.Refresh()
    return $form
}

function Show-Countdown {
    param([int]$Seconds)

    if ($Seconds -le 0) { return }

    $form = New-Object System.Windows.Forms.Form
    $form.Text = 'Forward Slash Windows README capture'
    $form.Width = 560
    $form.Height = 210
    $form.FormBorderStyle = [System.Windows.Forms.FormBorderStyle]::FixedDialog
    $form.MaximizeBox = $false
    $form.MinimizeBox = $false
    $form.StartPosition = [System.Windows.Forms.FormStartPosition]::CenterScreen
    $form.TopMost = $true
    $form.BackColor = [System.Drawing.Color]::FromArgb(30, 30, 34)

    $heading = New-Object System.Windows.Forms.Label
    $heading.Left = 20
    $heading.Top = 24
    $heading.Width = 505
    $heading.Height = 60
    $heading.TextAlign = [System.Drawing.ContentAlignment]::MiddleCenter
    $heading.Font = New-Object System.Drawing.Font('Segoe UI', 24, [System.Drawing.FontStyle]::Bold)
    $heading.ForeColor = [System.Drawing.Color]::White
    $form.Controls.Add($heading)

    $warning = New-Object System.Windows.Forms.Label
    $warning.Left = 20
    $warning.Top = 100
    $warning.Width = 505
    $warning.Height = 40
    $warning.TextAlign = [System.Drawing.ContentAlignment]::MiddleCenter
    $warning.Font = New-Object System.Drawing.Font('Segoe UI', 11)
    $warning.ForeColor = [System.Drawing.Color]::FromArgb(190, 196, 204)
    $warning.Text = 'Windows will minimize next. Hold Esc at any time to abort safely.'
    $form.Controls.Add($warning)

    Clear-Guards
    $form.Show()
    $form.Activate()
    for ($remaining = $Seconds; $remaining -ge 1; $remaining--) {
        $heading.Text = "Capture starts in $remaining"
        $form.Refresh()
        Wait-Safely 1000
    }
    $form.Close()
    $heading.Font.Dispose()
    $warning.Font.Dispose()
    $form.Dispose()
}

function Close-OwnedWindow {
    param([IntPtr]$Hwnd)

    Clear-Guards
    if ($Hwnd -ne [IntPtr]::Zero -and [ReadmeCaptureNative]::IsWindow($Hwnd)) {
        [ReadmeCaptureNative]::PostMessageW($Hwnd, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null
    }
}

# A scene that cannot be recorded is skipped; only the safety rules (Esc, focus
# loss, timeout) end the whole run.
function Invoke-Scene {
    param(
        [string]$Name,
        [scriptblock]$Body
    )

    Write-Host "Scene: $Name"
    try {
        & $Body
    } catch {
        if ($script:SafetyStop) { throw }
        Write-Warning "Scene '$Name' was skipped: $($_.Exception.Message)"
    } finally {
        Set-PrivacyMasks @()
        Set-CaptureOrigin
        Clear-Guards
    }
}

New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
try {
    Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
} catch {
    Write-Warning 'UI Automation is unavailable; the Settings scene will not change pages.'
}

# The collapsed Explorer pane still shows a strip of pinned-folder icons, and the
# common file dialog ignores the setting entirely, so both are painted over.
$neutralFolder = Join-Path $env:SystemRoot ''
$explorerMask = @{ X = 0; Y = 196; W = 106; H = 668; Color = $chromeColor }
$dialogMask = @{ X = 0; Y = 160; W = 264; H = 618; Color = $chromeColor }

try {
    Show-Countdown -Seconds $CountdownSeconds
    Set-DemoPrivacyState

    $shell = New-Object -ComObject Shell.Application
    $shell.MinimizeAll()
    $desktopWasMinimized = $true
    Wait-Safely 700
    $backdrop = Show-Backdrop
    Wait-Safely 400
    Set-CaptureOrigin

    Save-TitleCard 'Forward Slash Windows' 'Type a slash path anywhere in Windows.' 'Run   File Explorer   Search   Open dialogs   Terminals' -Frames 17

    Invoke-Scene 'Run dialog' {
        Save-TitleCard 'Run' 'Win+R, then a Linux path' '/etc/apt  opens  \\wsl.localhost\Ubuntu\etc\apt'
        $beforeRunAll = Get-WindowSet -ClassName '#32770' -VisibleOnly $false
        $beforeRunVisible = Get-WindowSet -ClassName '#32770' -VisibleOnly $true
        $beforeExplorerAll = Get-WindowSet -ClassName 'CabinetWClass' -VisibleOnly $false
        $beforeExplorerVisible = Get-WindowSet -ClassName 'CabinetWClass' -VisibleOnly $true
        $shell.FileRun()
        $run = Wait-NewWindow -BeforeAll $beforeRunAll -BeforeVisible $beforeRunVisible -ClassName '#32770' -Seconds 12
        if ($run -eq [IntPtr]::Zero -or [ReadmeCaptureNative]::WindowTitle($run) -ne 'Run') {
            throw 'Run did not create a new, safely owned dialog.'
        }
        $script:OwnedWindows.Add($run)
        Set-WindowCentered $run
        Use-FocusGuard $run
        Save-Frames -Count 5
        Send-CaptureText -Text '/etc/apt' -SuppressAutoComplete
        Save-Frames -Count 6
        Send-CaptureKeys '{ENTER}'
        Clear-Guards
        $runResult = Wait-NewWindow -BeforeAll $beforeExplorerAll -BeforeVisible $beforeExplorerVisible -ClassName 'CabinetWClass' -Seconds 15
        if ($runResult -eq [IntPtr]::Zero) {
            throw 'Run did not open an Explorer window.'
        }
        $script:OwnedWindows.Add($runResult)
        $script:Explorer = $runResult
        Set-FullFrame $runResult
        Set-PrivacyMasks @($explorerMask)
        Use-FocusGuard $runResult
        Save-Frames -Count 14
    }

    Invoke-Scene 'File Explorer address bar' {
        Save-TitleCard 'File Explorer' 'The address bar takes the same path' '/usr  resolves to  \\wsl.localhost\Ubuntu\usr'
        if ($script:Explorer -eq [IntPtr]::Zero -or -not [ReadmeCaptureNative]::IsWindow($script:Explorer)) {
            throw 'No Explorer window from the previous scene is available.'
        }
        Set-PrivacyMasks @($explorerMask)
        Set-FullFrame $script:Explorer
        Use-FocusGuard $script:Explorer
        Save-Frames -Count 4
        Send-CaptureKeys '^l'
        Wait-Safely 300
        Save-Frames -Count 4
        Send-CaptureText -Text '/usr' -SuppressAutoComplete
        Submit-CaptureText -HoldFrames 6 -SettleFrames 16
        # Leaving this window open would make Windows Search reuse it instead of
        # opening a new one, which the next scene waits for.
        Close-OwnedWindow $script:Explorer
        $script:Explorer = [IntPtr]::Zero
        Wait-Safely 700
    }

    Invoke-Scene 'Start menu search' {
        Save-TitleCard 'Windows Search' 'Start menu search understands it too' '/etc  goes to the distribution instead of the web'
        $beforeExplorerAll = Get-WindowSet -ClassName 'CabinetWClass' -VisibleOnly $false
        $beforeExplorerVisible = Get-WindowSet -ClassName 'CabinetWClass' -VisibleOnly $true

        Write-Host '  opening Start'
        [ReadmeCaptureNative]::TapWindowsKey()
        Wait-Safely 1500
        Use-ProcessGuard -ProcessNames @('StartMenuExperienceHost', 'SearchHost', 'SearchApp')

        # Typing swaps Start for the search view, which is a different window, so
        # the geometry is only measurable once a character has been entered.
        Write-Host '  Start has focus; measuring the search panel'
        Send-CaptureKeys '/'
        Wait-Safely 900
        Use-ProcessGuard -ProcessNames @('StartMenuExperienceHost', 'SearchHost', 'SearchApp')
        $panel = [ReadmeCaptureNative]::GetForegroundWindow()
        $panelRect = New-Object FswRect
        if (-not [ReadmeCaptureNative]::GetWindowRect($panel, [ref]$panelRect)) {
            throw 'The search panel could not be measured.'
        }
        $centre = [int](($panelRect.Left + $panelRect.Right) / 2)
        # Anchor on the top of the panel so the account row along its bottom edge
        # falls outside the frame.
        Set-CaptureOrigin -X ($centre - [int]($width / 2)) -Y ([Math]::Max(0, $panelRect.Top - 24))
        # The result list names files and folders found on this machine, so only
        # the query box and its filter row are recorded.
        Set-PrivacyMasks @(@{ X = 0; Y = 205; W = $width; H = ($height - 205); Color = $backdropColor })
        Write-Host "  panel at $($panelRect.Left),$($panelRect.Top); recording"
        Send-CaptureKeys '{BACKSPACE}'
        Wait-Safely 500
        Save-Frames -Count 6
        Send-CaptureText -Text '/etc'
        Save-Frames -Count 10
        Send-CaptureKeys '{ENTER}'
        Clear-Guards
        Write-Host '  waiting for Explorer to open from Search'
        $searchResult = Wait-NewWindow -BeforeAll $beforeExplorerAll -BeforeVisible $beforeExplorerVisible -ClassName 'CabinetWClass' -Seconds 15
        if ($searchResult -eq [IntPtr]::Zero) {
            throw 'Search did not open an Explorer window.'
        }
        $script:OwnedWindows.Add($searchResult)
        Set-CaptureOrigin
        Set-FullFrame $searchResult
        Set-PrivacyMasks @($explorerMask)
        Use-FocusGuard $searchResult
        Save-Frames -Count 14
    }

    Invoke-Scene 'Notepad open dialog' {
        Save-TitleCard 'Open and Save dialogs' 'Any classic file dialog accepts it' 'Notepad, Open, then /etc/apt'
        $beforeNotepadAll = Get-WindowSet -VisibleOnly $false
        $beforeNotepadVisible = Get-WindowSet -VisibleOnly $true
        $notepadProcess = Start-Process notepad.exe -PassThru
        $script:OwnedProcesses.Add($notepadProcess)
        $notepad = Wait-NewWindow -BeforeAll $beforeNotepadAll -BeforeVisible $beforeNotepadVisible `
            -ProcessNames @('Notepad', 'notepad') -Seconds 20
        if ($notepad -eq [IntPtr]::Zero) {
            throw 'Notepad did not create a window.'
        }
        $script:OwnedWindows.Add($notepad)
        Set-FullFrame $notepad
        Use-FocusGuard $notepad
        # Nothing is recorded until the dialog covers the frame: Notepad restores
        # the operator's previous tabs, and those file names stay out of the GIF.

        $beforeDialogAll = Get-WindowSet -ClassName '#32770' -VisibleOnly $false
        $beforeDialogVisible = Get-WindowSet -ClassName '#32770' -VisibleOnly $true
        Send-CaptureKeys '^o'
        Clear-Guards
        $dialog = Wait-NewWindow -BeforeAll $beforeDialogAll -BeforeVisible $beforeDialogVisible -ClassName '#32770' -Seconds 20
        if ($dialog -eq [IntPtr]::Zero) {
            throw 'The Open dialog did not appear.'
        }
        $script:OwnedWindows.Add($dialog)
        Set-FullFrame $dialog
        Set-PrivacyMasks @($dialogMask)
        Use-FocusGuard $dialog
        Send-CaptureKeys '%n'
        Wait-Safely 300
        Send-CaptureKeys $neutralFolder
        Wait-Safely 250
        Send-CaptureKeys '{ENTER}'
        Wait-Safely 1600
        Send-CaptureKeys '%n'
        Wait-Safely 300
        Save-Frames -Count 8
        Send-CaptureText -Text '/etc/apt' -SuppressAutoComplete
        Submit-CaptureText -HoldFrames 6 -SettleFrames 18
        Close-OwnedWindow $dialog
    }

    Invoke-Scene 'Command Prompt adapter' {
        Save-TitleCard 'Command Prompt' 'A reversible adapter keeps dir and ls native' 'dir /etc/apt   and   dir /usr'
        $cmd = Start-ClassicConsole -Executable 'cmd.exe' -Arguments @('/Q', '/K')
        Set-ConsoleFrame -Hwnd $cmd
        Use-FocusGuard $cmd
        Save-Frames -Count 5
        Send-CaptureText -Text 'dir /etc/apt'
        Submit-CaptureText -HoldFrames 4 -SettleFrames 14
        Send-CaptureText -Text 'dir /usr'
        Submit-CaptureText -HoldFrames 4 -SettleFrames 16
    }

    Invoke-Scene 'Windows PowerShell adapter' {
        Save-TitleCard 'Windows PowerShell' 'A profile adapter plus the controller' 'ls /usr   and   fwdslash status'
        $powerShell = Start-ClassicConsole -Executable 'powershell.exe' -Arguments @('-NoLogo')
        Set-ConsoleFrame -Hwnd $powerShell
        Use-FocusGuard $powerShell
        Save-Frames -Count 8
        Send-CaptureText -Text 'ls /usr'
        Submit-CaptureText -HoldFrames 4 -SettleFrames 16
        Send-CaptureText -Text 'fwdslash status'
        Submit-CaptureText -HoldFrames 4 -SettleFrames 18
    }

    Invoke-Scene 'Settings' {
        Save-TitleCard 'Settings' 'Every integration is independent and reversible' 'Pause resolution without forgetting what you installed'
        $settingsProcess = Start-Process $settings -ArgumentList 'fwdslash://settings/general' -WorkingDirectory $bin -PassThru
        $script:OwnedProcesses.Add($settingsProcess)
        $settingsWindow = [IntPtr]::Zero
        $until = (Get-Date).AddSeconds(25)
        while ((Get-Date) -lt $until) {
            Assert-Continue
            $settingsProcess.Refresh()
            if ($settingsProcess.HasExited) {
                throw "Settings exited with code $($settingsProcess.ExitCode)."
            }
            if ($settingsProcess.MainWindowHandle -ne [IntPtr]::Zero) {
                $settingsWindow = $settingsProcess.MainWindowHandle
                break
            }
            Wait-Safely 100
        }
        if ($settingsWindow -eq [IntPtr]::Zero) {
            throw 'Settings did not create a window.'
        }
        $script:OwnedWindows.Add($settingsWindow)
        Set-FullFrame $settingsWindow
        Use-FocusGuard $settingsWindow
        Wait-Safely 900
        Save-Frames -Count 13

        foreach ($page in @('Windows', 'Terminals', 'About')) {
            try {
                Select-SettingsPage -Hwnd $settingsWindow -Name $page
            } catch {
                Write-Warning "Settings page '$page' was skipped: $($_.Exception.Message)"
                continue
            }
            Wait-Safely 260
            Save-Frames -Count 13
        }
    }

    Save-TitleCard 'Forward Slash Windows' 'github.com/faratech/fwdslash' 'Open source   MIT License   Fara Technologies LLC' -Frames 20

    Write-Host "Capture complete: $script:Frame frames"
    Write-Host "Frames: $OutputDirectory"
} finally {
    Clear-Guards

    foreach ($hwnd in $script:OwnedWindows) {
        Close-OwnedWindow $hwnd
    }
    Start-Sleep -Milliseconds 500

    foreach ($process in $script:OwnedProcesses) {
        try {
            $process.Refresh()
            if (-not $process.HasExited) {
                Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
            }
        } catch {
            # The process may already have exited after WM_CLOSE.
        }
    }

    if ($backdrop) {
        try { $backdrop.Close() } catch {}
        try { $backdrop.Dispose() } catch {}
    }

    Restore-DemoRegistry
    Restore-ClearedRegistryValues

    if ($desktopWasMinimized -and $shell) {
        try { $shell.UndoMinimizeAll() } catch {}
        Start-Sleep -Milliseconds 300
    }

    if ($originalForeground -ne [IntPtr]::Zero -and [ReadmeCaptureNative]::IsWindow($originalForeground)) {
        [ReadmeCaptureNative]::SetForegroundWindow($originalForeground) | Out-Null
    }

    if ($shell) {
        [void][Runtime.InteropServices.Marshal]::FinalReleaseComObject($shell)
    }

    if ($createdNew) {
        try { $mutex.ReleaseMutex() } catch {}
    }
    $mutex.Dispose()
}

if (-not $SkipGif -and $script:Frame -gt 0) {
    $builder = Join-Path $PSScriptRoot 'Build-ReadmeGif.ps1'
    if (Test-Path -LiteralPath $builder -PathType Leaf) {
        & $builder -FrameDirectory $OutputDirectory -FrameRate ([Math]::Round(1000.0 / $frameMs, 3))
    }
}
