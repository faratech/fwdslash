[CmdletBinding()]
param(
    [string]$Source = (Join-Path (Split-Path -Parent $PSScriptRoot) 'assets\fwdslash-icon-master.png'),
    [string]$Destination = (Join-Path (Split-Path -Parent $PSScriptRoot) 'assets\fwdslash.ico'),
    [string]$TitleBarDestination = (Join-Path (Split-Path -Parent $PSScriptRoot) 'assets\fwdslash-titlebar.png')
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

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
