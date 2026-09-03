[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$FrameDirectory,

    [double]$FrameRate = 12.5,

    [ValidateRange(320, 1920)]
    [int]$Width = 900,

    [ValidateRange(16, 256)]
    [int]$MaxColors = 128,

    [ValidateSet('none', 'bayer', 'sierra2_4a', 'floyd_steinberg')]
    [string]$Dither = 'bayer',

    [ValidateRange(0, 5)]
    [int]$BayerScale = 4,

    [string]$OutputPath
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$FrameDirectory = (Resolve-Path -LiteralPath $FrameDirectory).Path
$frames = @(Get-ChildItem -LiteralPath $FrameDirectory -Filter 'f*.png' | Sort-Object Name)
if ($frames.Count -eq 0) {
    throw "No captured frames were found in $FrameDirectory."
}
if (-not $OutputPath) {
    $OutputPath = Join-Path $FrameDirectory 'readme-demo.gif'
}

# ffmpeg is the only dependable palette-aware GIF encoder available here. This
# project already requires WSL, so fall back to the distribution's copy when
# Windows does not have one on PATH.
$useWsl = $false
$ffmpeg = Get-Command ffmpeg.exe -ErrorAction SilentlyContinue
if (-not $ffmpeg) {
    & wsl.exe -e ffmpeg -version *> $null
    if ($LASTEXITCODE -ne 0) {
        throw 'ffmpeg was not found on the Windows PATH or inside WSL. Install it with "wsl sudo apt install ffmpeg".'
    }
    $useWsl = $true
}

function ConvertTo-ToolPath {
    param([string]$WindowsPath)

    if (-not $useWsl) { return $WindowsPath }
    # wsl.exe consumes backslashes when it builds the Linux command line, so hand
    # wslpath the forward-slash spelling of the same Windows path.
    $portable = $WindowsPath.Replace('\', '/')
    $converted = (& wsl.exe wslpath -a -u $portable) | Select-Object -First 1
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($converted)) {
        throw "Could not translate '$WindowsPath' into a WSL path."
    }
    return $converted.Trim()
}

$inputPattern = ConvertTo-ToolPath (Join-Path $FrameDirectory 'f%04d.png')
$outputTarget = ConvertTo-ToolPath $OutputPath

$paletteUse = if ($Dither -eq 'bayer') {
    "paletteuse=dither=bayer:bayer_scale=${BayerScale}:diff_mode=rectangle"
} else {
    "paletteuse=dither=${Dither}:diff_mode=rectangle"
}
$filter = "scale=${Width}:-2:flags=lanczos,split[a][b];" +
          "[a]palettegen=max_colors=${MaxColors}:stats_mode=diff[p];" +
          "[b][p]$paletteUse"

$arguments = @(
    '-hide_banner', '-loglevel', 'error', '-y',
    '-framerate', ([string]$FrameRate),
    '-i', $inputPattern,
    '-filter_complex', $filter,
    '-loop', '0',
    $outputTarget
)

Write-Host "Encoding $($frames.Count) frames at ${FrameRate} fps, ${Width}px wide, $MaxColors colors."
if ($useWsl) {
    & wsl.exe -e ffmpeg @arguments
} else {
    & $ffmpeg.Source @arguments
}
if ($LASTEXITCODE -ne 0) {
    throw "ffmpeg failed with exit code $LASTEXITCODE."
}

$size = (Get-Item -LiteralPath $OutputPath).Length
$seconds = [Math]::Round($frames.Count / $FrameRate, 1)
Write-Host ("GIF: {0}" -f $OutputPath)
Write-Host ("Duration: {0}s   Size: {1:N1} MB" -f $seconds, ($size / 1MB))
