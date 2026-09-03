[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateSet('WindowsPowerShell', 'PowerShell')]
    [string]$Edition
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Write-AtomicBytes {
    param([Parameter(Mandatory)][string]$Path, [Parameter(Mandatory)][byte[]]$Bytes)
    $directory = Split-Path -Parent $Path
    $temporary = Join-Path $directory ('.fsw-' + [Guid]::NewGuid().ToString('N') + '.tmp')
    $rollback = Join-Path $directory ('.fsw-' + [Guid]::NewGuid().ToString('N') + '.rollback')
    try {
        [IO.File]::WriteAllBytes($temporary, $Bytes)
        if (Test-Path -LiteralPath $Path -PathType Leaf) {
            [IO.File]::Replace($temporary, $Path, $rollback, $true)
            Remove-Item -LiteralPath $rollback -Force
        } else {
            Move-Item -LiteralPath $temporary -Destination $Path
        }
    } finally {
        if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -Force }
        if (Test-Path -LiteralPath $rollback) { Remove-Item -LiteralPath $rollback -Force }
    }
}

function Find-ByteSequence {
    param([byte[]]$Buffer, [byte[]]$Sequence)
    if ($Sequence.Length -eq 0 -or $Sequence.Length -gt $Buffer.Length) { return -1 }
    for ($start = 0; $start -le $Buffer.Length - $Sequence.Length; $start++) {
        $matched = $true
        for ($offset = 0; $offset -lt $Sequence.Length; $offset++) {
            if ($Buffer[$start + $offset] -ne $Sequence[$offset]) { $matched = $false; break }
        }
        if ($matched) { return $start }
    }
    return -1
}

$statePath = "Software\ForwardSlashWindows\PowerShellAdapter\$Edition"
$currentUser = [Microsoft.Win32.Registry]::CurrentUser
$stateKey = $currentUser.OpenSubKey($statePath, $true)
if (-not $stateKey) {
    $currentUser.Close()
    Write-Host "The $Edition adapter is not installed."
    return
}

try {
    $state = [string]$stateKey.GetValue('State', '')
    $profilePath = [string]$stateKey.GetValue('ProfilePath', '')
    $stateRoot = [string]$stateKey.GetValue('StateDirectory', '')
    $originalPresent = [int]$stateKey.GetValue('OriginalPresent', 0) -ne 0
    if ($state -notin 'prepared', 'installed', 'removing') {
        throw "Unknown $Edition adapter transaction state '$state'."
    }
    $originalFile = Join-Path $stateRoot 'profile.original'
    $blockFile = Join-Path $stateRoot 'profile.block'
    if (-not (Test-Path -LiteralPath $originalFile) -or -not (Test-Path -LiteralPath $blockFile)) {
        throw 'The recovery files are missing; refusing to modify the PowerShell profile.'
    }
    [byte[]]$originalBytes = [IO.File]::ReadAllBytes($originalFile)
    [byte[]]$blockBytes = [IO.File]::ReadAllBytes($blockFile)
    $stateKey.SetValue('State', 'removing', [Microsoft.Win32.RegistryValueKind]::String)
    $stateKey.Flush()

    if (Test-Path -LiteralPath $profilePath -PathType Leaf) {
        [byte[]]$currentBytes = [IO.File]::ReadAllBytes($profilePath)
        $blockIndex = Find-ByteSequence -Buffer $currentBytes -Sequence $blockBytes
        if ($blockIndex -ge 0) {
            [byte[]]$remaining = New-Object byte[] ($currentBytes.Length - $blockBytes.Length)
            if ($blockIndex -gt 0) { [Array]::Copy($currentBytes, 0, $remaining, 0, $blockIndex) }
            $tail = $currentBytes.Length - $blockIndex - $blockBytes.Length
            if ($tail -gt 0) { [Array]::Copy($currentBytes, $blockIndex + $blockBytes.Length, $remaining, $blockIndex, $tail) }
            if ($remaining.Length -eq 0 -and -not $originalPresent) {
                Remove-Item -LiteralPath $profilePath -Force
            } else {
                Write-AtomicBytes -Path $profilePath -Bytes $remaining
            }
        } elseif ($state -eq 'installed') {
            throw 'The Forward Slash Windows profile block was changed or removed externally. No profile content was overwritten.'
        }
    }

    $stateKey.Close()
    $stateKey = $null
    $currentUser.DeleteSubKeyTree($statePath, $false)
    if (Test-Path -LiteralPath $stateRoot) { Remove-Item -LiteralPath $stateRoot -Recurse -Force }

    $otherEdition = if ($Edition -eq 'PowerShell') { 'WindowsPowerShell' } else { 'PowerShell' }
    $other = $currentUser.OpenSubKey("Software\ForwardSlashWindows\PowerShellAdapter\$otherEdition", $false)
    if ($other) { $other.Close() }
    else {
        $moduleRoot = Join-Path $env:LOCALAPPDATA 'ForwardSlashWindows\PowerShell\0.0.1'
        if (Test-Path -LiteralPath $moduleRoot) { Remove-Item -LiteralPath $moduleRoot -Recurse -Force }
    }
    Write-Host "Forward Slash Windows removed from $Edition. Already-open sessions retain loaded aliases until closed."
} finally {
    if ($stateKey) { $stateKey.Close() }
    $currentUser.Close()
}
