[CmdletBinding()]
param(
    [string]$Source,
    [string]$Destination,
    [string]$TitleBarDestination,

    # MSIX requires its own logo set at several scales. Generated from the same
    # master so the Store tiles, the taskbar and the .ico never drift apart.
    [string]$MsixAssetDirectory
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

# $PSScriptRoot is empty while an advanced function's parameter defaults are
# evaluated, so the repo-relative defaults are resolved here instead.
$repo = Split-Path -Parent $PSScriptRoot
if (-not $Source) { $Source = Join-Path $repo 'assets\fwdslash-icon-master.png' }
if (-not $Destination) { $Destination = Join-Path $repo 'assets\fwdslash.ico' }
if (-not $TitleBarDestination) { $TitleBarDestination = Join-Path $repo 'assets\fwdslash-titlebar.png' }
if (-not $MsixAssetDirectory) { $MsixAssetDirectory = Join-Path $repo 'packaging\Assets' }

$sizes = 16, 20, 24, 32, 40, 48, 64, 128, 256
$sourceImage = [Drawing.Bitmap]::FromFile([IO.Path]::GetFullPath($Source))
$images = [Collections.Generic.List[byte[]]]::new()
try {
    foreach ($size in $sizes) {
        $bitmap = [Drawing.Bitmap]::new($size, $size, [Drawing.Imaging.PixelFormat]::Format32bppArgb)
        try {
            $bitmap.SetResolution(96, 96)
            $graphics = [Drawing.Graphics]::FromImage($bitmap)
            try {
                $graphics.CompositingMode = [Drawing.Drawing2D.CompositingMode]::SourceCopy
                $graphics.CompositingQuality = [Drawing.Drawing2D.CompositingQuality]::HighQuality
                $graphics.InterpolationMode = [Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
                $graphics.PixelOffsetMode = [Drawing.Drawing2D.PixelOffsetMode]::HighQuality
                $graphics.SmoothingMode = [Drawing.Drawing2D.SmoothingMode]::HighQuality
                $graphics.DrawImage($sourceImage, [Drawing.Rectangle]::new(0, 0, $size, $size))
            } finally {
                $graphics.Dispose()
            }
            $stream = [IO.MemoryStream]::new()
            try {
                $bitmap.Save($stream, [Drawing.Imaging.ImageFormat]::Png)
                $images.Add($stream.ToArray())
            } finally {
                $stream.Dispose()
            }
        } finally {
            $bitmap.Dispose()
        }
    }
} finally {
    $sourceImage.Dispose()
}

$destinationPath = [IO.Path]::GetFullPath($Destination)
$destinationDirectory = Split-Path -Parent $destinationPath
New-Item -ItemType Directory -Force -Path $destinationDirectory | Out-Null
$temporary = Join-Path $destinationDirectory ('.fsw-icon-' + [Guid]::NewGuid().ToString('N') + '.tmp')
try {
    $file = [IO.File]::Open($temporary, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    try {
        $writer = [IO.BinaryWriter]::new($file)
        try {
            $writer.Write([uint16]0)
            $writer.Write([uint16]1)
            $writer.Write([uint16]$sizes.Count)
            [uint32]$offset = 6 + (16 * $sizes.Count)
            for ($index = 0; $index -lt $sizes.Count; $index++) {
                $size = $sizes[$index]
                $writer.Write([byte]$(if ($size -eq 256) { 0 } else { $size }))
                $writer.Write([byte]$(if ($size -eq 256) { 0 } else { $size }))
                $writer.Write([byte]0)
                $writer.Write([byte]0)
                $writer.Write([uint16]1)
                $writer.Write([uint16]32)
                $writer.Write([uint32]$images[$index].Length)
                $writer.Write($offset)
                $offset += [uint32]$images[$index].Length
            }
            foreach ($image in $images) {
                $writer.Write($image)
            }
            $writer.Flush()
            $file.Flush($true)
        } finally {
            $writer.Dispose()
        }
    } finally {
        $file.Dispose()
    }
    Move-Item -LiteralPath $temporary -Destination $destinationPath -Force
} finally {
    if (Test-Path -LiteralPath $temporary) {
        Remove-Item -LiteralPath $temporary -Force
    }
}

Write-Host "Generated $destinationPath with $($sizes.Count) PNG-backed icon sizes."

$titleBarPath = [IO.Path]::GetFullPath($TitleBarDestination)
$titleBarDirectory = Split-Path -Parent $titleBarPath
New-Item -ItemType Directory -Force -Path $titleBarDirectory | Out-Null
$titleBarTemporary = Join-Path $titleBarDirectory ('.fsw-titlebar-' + [Guid]::NewGuid().ToString('N') + '.tmp')
try {
    # Entry 6 is the 64 px rendition, large enough for high-DPI title bars while
    # remaining inexpensive to decode at startup.
    [IO.File]::WriteAllBytes($titleBarTemporary, $images[6])
    Move-Item -LiteralPath $titleBarTemporary -Destination $titleBarPath -Force
} finally {
    if (Test-Path -LiteralPath $titleBarTemporary) {
        Remove-Item -LiteralPath $titleBarTemporary -Force
    }
}

Write-Host "Generated $titleBarPath for the WinUI title bar."

# --- MSIX logo set -----------------------------------------------------------

# Renders the master centred on a $Width x $Height transparent canvas. The
# non-square tiles (wide and splash) letterbox the square artwork rather than
# stretching it.
function Save-MsixAsset {
    param(
        [Drawing.Image]$Source,
        [int]$Width,
        [int]$Height,
        [string]$Path
    )

    $bitmap = [Drawing.Bitmap]::new($Width, $Height, [Drawing.Imaging.PixelFormat]::Format32bppArgb)
    try {
        $bitmap.SetResolution(96, 96)
        $graphics = [Drawing.Graphics]::FromImage($bitmap)
        try {
            $graphics.CompositingQuality = [Drawing.Drawing2D.CompositingQuality]::HighQuality
            $graphics.InterpolationMode = [Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
            $graphics.PixelOffsetMode = [Drawing.Drawing2D.PixelOffsetMode]::HighQuality
            $graphics.SmoothingMode = [Drawing.Drawing2D.SmoothingMode]::HighQuality
            $graphics.Clear([Drawing.Color]::Transparent)
            $edge = [Math]::Min($Width, $Height)
            $graphics.DrawImage($Source, [Drawing.Rectangle]::new(
                [int](($Width - $edge) / 2), [int](($Height - $edge) / 2), $edge, $edge))
        } finally {
            $graphics.Dispose()
        }
        $bitmap.Save($Path, [Drawing.Imaging.ImageFormat]::Png)
    } finally {
        $bitmap.Dispose()
    }
}

$msixRoot = [IO.Path]::GetFullPath($MsixAssetDirectory)
New-Item -ItemType Directory -Force -Path $msixRoot | Out-Null

# Base logical sizes; each is emitted at every supported scale factor.
$msixLogos = @(
    @{ Name = 'Square44x44Logo';   Width = 44;  Height = 44 },
    @{ Name = 'Square71x71Logo';   Width = 71;  Height = 71 },
    @{ Name = 'Square150x150Logo'; Width = 150; Height = 150 },
    @{ Name = 'Square310x310Logo'; Width = 310; Height = 310 },
    @{ Name = 'Wide310x150Logo';   Width = 310; Height = 150 },
    @{ Name = 'StoreLogo';         Width = 50;  Height = 50 },
    @{ Name = 'SplashScreen';      Width = 620; Height = 300 }
)
$msixScales = 100, 125, 150, 200, 400

# Square44x44Logo also drives the taskbar and Alt+Tab, which use target sizes
# rather than scales. The unplated variants are what Windows shows on the
# taskbar, where a plate behind the glyph would look wrong.
$msixTargetSizes = 16, 24, 32, 48, 256

$msixCount = 0
$masterImage = [Drawing.Bitmap]::FromFile([IO.Path]::GetFullPath($Source))
try {
    foreach ($logo in $msixLogos) {
        foreach ($scale in $msixScales) {
            $width = [int][Math]::Round($logo.Width * $scale / 100.0)
            $height = [int][Math]::Round($logo.Height * $scale / 100.0)
            $path = Join-Path $msixRoot ('{0}.scale-{1}.png' -f $logo.Name, $scale)
            Save-MsixAsset -Source $masterImage -Width $width -Height $height -Path $path
            $msixCount++
        }
        # A scale-free copy so a manifest reference without a qualifier resolves.
        Save-MsixAsset -Source $masterImage -Width $logo.Width -Height $logo.Height `
            -Path (Join-Path $msixRoot ('{0}.png' -f $logo.Name))
        $msixCount++
    }

    foreach ($target in $msixTargetSizes) {
        foreach ($suffix in @('', '_altform-unplated')) {
            $path = Join-Path $msixRoot ('Square44x44Logo.targetsize-{0}{1}.png' -f $target, $suffix)
            Save-MsixAsset -Source $masterImage -Width $target -Height $target -Path $path
            $msixCount++
        }
    }
} finally {
    $masterImage.Dispose()
}

Write-Host "Generated $msixCount MSIX assets in $msixRoot."
