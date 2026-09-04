<#
.SYNOPSIS
Screenshots each page of the settings window so the Rust build can be diffed
against the C++ one.

.DESCRIPTION
Launches one process per section, so the deep-link path (fwdslash://settings/<s>)
is exercised at the same time. The window is captured through its DWM extended
frame bounds rather than GetWindowRect, otherwise every shot carries a sliver of
desktop from the invisible resize border.

Requires an unlocked, non-minimised interactive desktop: CopyFromScreen reads the
actual framebuffer.
#>
[CmdletBinding()]
param(
    [string]$Exe,
    [string]$OutputDirectory,
    [string[]]$Sections = @('general', 'windows', 'terminals', 'about'),
    [int]$Width = 1280,
    [int]$Height = 860
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

# $PSScriptRoot is empty while parameter defaults are evaluated, so repo-relative
# defaults are resolved here instead.
$repo = Split-Path -Parent $PSScriptRoot
if (-not $Exe) { $Exe = Join-Path $repo 'out\user\arm64\Release\fswsettings.exe' }
if (-not $OutputDirectory) { $OutputDirectory = Join-Path $repo 'out\shots' }
New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

[StructLayout(LayoutKind.Sequential)]
public struct FswRect { public int Left; public int Top; public int Right; public int Bottom; }

public static class FswShot {
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hwnd, out FswRect rect);
    [DllImport("user32.dll")] public static extern bool MoveWindow(IntPtr hwnd, int x, int y, int w, int h, bool repaint);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hwnd, IntPtr hdc, uint flags);
    [DllImport("dwmapi.dll")] public static extern int DwmGetWindowAttribute(IntPtr hwnd, int attribute, out FswRect value, int size);
}
'@

# DWMWA_EXTENDED_FRAME_BOUNDS
$DwmExtendedFrameBounds = 9

function Get-FrameBounds {
    param([IntPtr]$Handle)
    $bounds = New-Object FswRect
    $size = [Runtime.InteropServices.Marshal]::SizeOf([type]'FswRect')
    if ([FswShot]::DwmGetWindowAttribute($Handle, $DwmExtendedFrameBounds, [ref]$bounds, $size) -eq 0) {
        return $bounds
    }
    [void][FswShot]::GetWindowRect($Handle, [ref]$bounds)
    return $bounds
}

foreach ($section in $Sections) {
    $process = Start-Process -FilePath $Exe -ArgumentList "fwdslash://settings/$section" -PassThru
    try {
        $handle = [IntPtr]::Zero
        for ($attempt = 0; $attempt -lt 60; $attempt++) {
            Start-Sleep -Milliseconds 250
            $process.Refresh()
            if ($process.MainWindowHandle -ne [IntPtr]::Zero) {
                $handle = $process.MainWindowHandle
                break
            }
        }
        if ($handle -eq [IntPtr]::Zero) { throw "No window appeared for section '$section'." }

        # Position by the frame the user sees, correcting for the invisible border.
        [void][FswShot]::MoveWindow($handle, 0, 0, $Width, $Height, $true)
        Start-Sleep -Milliseconds 400
        $outer = New-Object FswRect
        [void][FswShot]::GetWindowRect($handle, [ref]$outer)
        $frame = Get-FrameBounds -Handle $handle
        $dx = $frame.Left - $outer.Left
        $dy = $frame.Top - $outer.Top
        [void][FswShot]::MoveWindow($handle, -$dx, -$dy, $Width + (2 * $dx), $Height + $dy, $true)
        [void][FswShot]::SetForegroundWindow($handle)
        # Mica and the InfoBar animate in; let them settle before the grab.
        Start-Sleep -Milliseconds 1200

        $frame = Get-FrameBounds -Handle $handle
        $outer = New-Object FswRect
        [void][FswShot]::GetWindowRect($handle, [ref]$outer)
        $w = $frame.Right - $frame.Left
        $h = $frame.Bottom - $frame.Top
        # PrintWindow with PW_RENDERFULLCONTENT draws the window's own surface, so a
        # notification or another app stealing focus cannot corrupt the shot. Falls back
        # to a framebuffer grab if the window declines to render.
        $outerW = $outer.Right - $outer.Left
        $outerH = $outer.Bottom - $outer.Top
        $bitmap = New-Object System.Drawing.Bitmap($outerW, $outerH)
        $printed = $false
        try {
            $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
            try {
                $hdc = $graphics.GetHdc()
                try { $printed = [FswShot]::PrintWindow($handle, $hdc, 2) }
                finally { $graphics.ReleaseHdc($hdc) }
            } finally { $graphics.Dispose() }
            if (-not $printed) {
                $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
                try {
                    $graphics.CopyFromScreen($outer.Left, $outer.Top, 0, 0,
                        (New-Object System.Drawing.Size($outerW, $outerH)))
                } finally { $graphics.Dispose() }
            }
            # The remaining margin is the invisible DWM resize border, which
            # PrintWindow leaves transparent -- harmless for a visual diff, and cropping
            # it needs a rect that stays valid across the DPI-scaled move above.
            $path = Join-Path $OutputDirectory "rust_$section.png"
            $bitmap.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
            Write-Host "Captured $path ($outerW x $outerH)"
        } finally { $bitmap.Dispose() }
    } finally {
        if (-not $process.HasExited) { $process.Kill() }
        Start-Sleep -Milliseconds 300
    }
}
