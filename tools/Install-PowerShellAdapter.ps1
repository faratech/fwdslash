[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateSet('WindowsPowerShell', 'PowerShell')]
    [string]$Edition,
    [string]$ControllerPath
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Write-AtomicBytes {
    param([Parameter(Mandatory)][string]$Path, [Parameter(Mandatory)][byte[]]$Bytes)
    $directory = Split-Path -Parent $Path
    New-Item -ItemType Directory -Force -Path $directory | Out-Null
    $temporary = Join-Path $directory ('.fsw-' + [Guid]::NewGuid().ToString('N') + '.tmp')
    $rollback = Join-Path $directory ('.fsw-' + [Guid]::NewGuid().ToString('N') + '.rollback')
    try {
        [IO.File]::WriteAllBytes($temporary, $Bytes)
        $stream = [IO.File]::Open($temporary, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
        try { $stream.Flush($true) } finally { $stream.Dispose() }
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

function Get-ProfileEncoding {
    param([byte[]]$Bytes)
    if ($Bytes.Length -ge 4 -and $Bytes[0] -eq 0x00 -and $Bytes[1] -eq 0x00 -and $Bytes[2] -eq 0xFE -and $Bytes[3] -eq 0xFF) {
        return New-Object Text.UTF32Encoding($true, $false)
    }
    if ($Bytes.Length -ge 4 -and $Bytes[0] -eq 0xFF -and $Bytes[1] -eq 0xFE -and $Bytes[2] -eq 0x00 -and $Bytes[3] -eq 0x00) {
        return New-Object Text.UTF32Encoding($false, $false)
    }
    if ($Bytes.Length -ge 2 -and $Bytes[0] -eq 0xFE -and $Bytes[1] -eq 0xFF) {
        return New-Object Text.UnicodeEncoding($true, $false)
    }
    if ($Bytes.Length -ge 2 -and $Bytes[0] -eq 0xFF -and $Bytes[1] -eq 0xFE) {
        return New-Object Text.UnicodeEncoding($false, $false)
    }
    return New-Object Text.UTF8Encoding($false)
}

function Test-AdapterProfile {
    param([Parameter(Mandatory)][string]$Edition)

    $shell = if ($Edition -eq 'PowerShell') {
        (Get-Command pwsh.exe -ErrorAction Stop).Source
    } else {
        Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
    }
    $command = @'
$ErrorActionPreference = 'Stop'
try {
    $dirAlias = Get-Alias -Name dir
    $lsAlias = Get-Alias -Name ls
    if ($dirAlias.Definition -ne 'Invoke-ForwardSlashWindowsChildItem' -or
        $lsAlias.Definition -ne 'Invoke-ForwardSlashWindowsChildItem') {
        exit 41
    }
    exit 0
} catch {
    exit 42
}
'@
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($command))
    $process = Start-Process -FilePath $shell -ArgumentList '-NoLogo', '-NonInteractive', '-EncodedCommand', $encoded -WindowStyle Hidden -PassThru
    if (-not $process.WaitForExit(15000)) {
        Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
        throw "$Edition profile verification timed out. The installation was rolled back."
    }
    if ($process.ExitCode -ne 0) {
        throw "$Edition did not load the Forward Slash Windows profile adapter (verification exit $($process.ExitCode)). The installation was rolled back."
    }
}

$repo = Split-Path -Parent $PSScriptRoot
$packagedShell = Join-Path $PSScriptRoot 'shell\powershell'
$sourceShell = if (Test-Path -LiteralPath $packagedShell -PathType Container) {
    $packagedShell
} else {
    Join-Path $repo 'shell\powershell'
}
if (-not $ControllerPath) {
    $packagedController = Join-Path $PSScriptRoot 'fwdslash.exe'
    $developmentController = Join-Path $repo 'out\user\arm64\Release\fwdslash.exe'
    $ControllerPath = if (Test-Path -LiteralPath $packagedController) { $packagedController } else { $developmentController }
}
$ControllerPath = [IO.Path]::GetFullPath($ControllerPath)
if (-not (Test-Path -LiteralPath $ControllerPath -PathType Leaf)) {
    throw "fwdslash.exe was not found: $ControllerPath"
}

$documents = [Environment]::GetFolderPath([Environment+SpecialFolder]::MyDocuments)
$profileFolder = if ($Edition -eq 'PowerShell') { 'PowerShell' } else { 'WindowsPowerShell' }
$profilePath = Join-Path (Join-Path $documents $profileFolder) 'profile.ps1'
$installRoot = Join-Path $env:LOCALAPPDATA 'ForwardSlashWindows\PowerShell'
$moduleRoot = Join-Path $installRoot '0.0.1'
$stateRoot = Join-Path (Join-Path $installRoot 'state') $Edition
$stateStaging = "$stateRoot.staging-$([Guid]::NewGuid().ToString('N'))"
$moduleStaging = "$moduleRoot.staging-$([Guid]::NewGuid().ToString('N'))"
$statePath = "Software\ForwardSlashWindows\PowerShellAdapter\$Edition"
$transactionId = [Guid]::NewGuid().ToString('N')
$currentUser = [Microsoft.Win32.Registry]::CurrentUser
$stateKey = $null
$moduleDeployed = $false
$stateDeployed = $false
$profileChanged = $false
$originalPresent = Test-Path -LiteralPath $profilePath -PathType Leaf
[byte[]]$originalBytes = [byte[]]::new(0)
if ($originalPresent) {
    $originalBytes = [IO.File]::ReadAllBytes($profilePath)
}

try {
    $existing = $currentUser.OpenSubKey($statePath, $false)
    if ($existing) {
        $existingState = [string]$existing.GetValue('State', '')
        $existing.Close()
        if ($existingState -eq 'installed') {
            Write-Host "The $Edition adapter is already installed."
            return
        }
        throw "An incomplete $Edition adapter transaction exists. Run Uninstall-PowerShellAdapter.ps1 -Edition $Edition to recover it."
    }

    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $moduleRoot) | Out-Null
    if (-not (Test-Path -LiteralPath $moduleRoot -PathType Container)) {
        New-Item -ItemType Directory -Path $moduleStaging | Out-Null
        Copy-Item -LiteralPath (Join-Path $sourceShell 'ForwardSlashWindows.psm1') -Destination $moduleStaging
        Copy-Item -LiteralPath $ControllerPath -Destination (Join-Path $moduleStaging 'fwdslash.exe')
        Move-Item -LiteralPath $moduleStaging -Destination $moduleRoot
        $moduleDeployed = $true
    }

    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $stateRoot) | Out-Null
    New-Item -ItemType Directory -Path $stateStaging | Out-Null
    [IO.File]::WriteAllBytes((Join-Path $stateStaging 'profile.original'), $originalBytes)
    $escapedModule = (Join-Path $moduleRoot 'ForwardSlashWindows.psm1').Replace("'", "''")
    $prefix = if ($originalBytes.Length -eq 0) { '' } else { "`r`n" }
    $blockText = $prefix +
        "# >>> Forward Slash Windows 0.0.1 $transactionId >>>`r`n" +
        "Import-Module -Name '$escapedModule' -Global -Force`r`n" +
        "# <<< Forward Slash Windows 0.0.1 $transactionId <<<`r`n"
    $encoding = Get-ProfileEncoding -Bytes $originalBytes
    [byte[]]$blockBytes = $encoding.GetBytes($blockText)
    [IO.File]::WriteAllBytes((Join-Path $stateStaging 'profile.block'), $blockBytes)
    Move-Item -LiteralPath $stateStaging -Destination $stateRoot
    $stateDeployed = $true

    $stateKey = $currentUser.CreateSubKey($statePath, $true)
    $stateKey.SetValue('State', 'prepared', [Microsoft.Win32.RegistryValueKind]::String)
    $stateKey.SetValue('Version', '0.0.1', [Microsoft.Win32.RegistryValueKind]::String)
    $stateKey.SetValue('TransactionId', $transactionId, [Microsoft.Win32.RegistryValueKind]::String)
    $stateKey.SetValue('ProfilePath', $profilePath, [Microsoft.Win32.RegistryValueKind]::String)
    $stateKey.SetValue('StateDirectory', $stateRoot, [Microsoft.Win32.RegistryValueKind]::String)
    $stateKey.SetValue('OriginalPresent', [int]$originalPresent, [Microsoft.Win32.RegistryValueKind]::DWord)
    $stateKey.Flush()

    [byte[]]$installedBytes = New-Object byte[] ($originalBytes.Length + $blockBytes.Length)
    [Array]::Copy($originalBytes, 0, $installedBytes, 0, $originalBytes.Length)
    [Array]::Copy($blockBytes, 0, $installedBytes, $originalBytes.Length, $blockBytes.Length)
    Write-AtomicBytes -Path $profilePath -Bytes $installedBytes
    $profileChanged = $true

    $stateKey.SetValue('State', 'installed', [Microsoft.Win32.RegistryValueKind]::String)
    $stateKey.Flush()
    Test-AdapterProfile -Edition $Edition
    Write-Host "Forward Slash Windows installed for $Edition. Open a new session to use it."
} catch {
    if ($profileChanged) {
        if ($originalPresent) { Write-AtomicBytes -Path $profilePath -Bytes $originalBytes }
        elseif (Test-Path -LiteralPath $profilePath) { Remove-Item -LiteralPath $profilePath -Force }
    }
    if ($stateKey) { $stateKey.Close(); $stateKey = $null }
    try { $currentUser.DeleteSubKeyTree($statePath, $false) } catch {}
    if ($stateDeployed -and (Test-Path -LiteralPath $stateRoot)) { Remove-Item -LiteralPath $stateRoot -Recurse -Force }
    if (Test-Path -LiteralPath $stateStaging) { Remove-Item -LiteralPath $stateStaging -Recurse -Force }
    if ($moduleDeployed -and (Test-Path -LiteralPath $moduleRoot)) { Remove-Item -LiteralPath $moduleRoot -Recurse -Force }
    if (Test-Path -LiteralPath $moduleStaging) { Remove-Item -LiteralPath $moduleStaging -Recurse -Force }
    throw
} finally {
    if ($stateKey) { $stateKey.Close() }
    $currentUser.Close()
}
