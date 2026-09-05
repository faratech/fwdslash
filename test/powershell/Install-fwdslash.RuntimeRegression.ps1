# Runtime-architecture regression fixture for #79.
# Run from native Windows PowerShell without a profile:
#   C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe -NoProfile -File C:\code\fwdslash-audit-20260905\test\powershell\Install-fwdslash.RuntimeRegression.ps1
# Every deployment/network command is mocked; only temporary directories and
# child script scopes are used.

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Assert-True {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw $Message }
}

function Invoke-InstallerFixture {
    param(
        [string]$ProcessArchitecture,
        [AllowEmptyString()][string]$Wow64Architecture,
        [object[]]$RuntimePackages = @()
    )

    $installer = Join-Path $PSScriptRoot '../../tools/Install-fwdslash.ps1'
    $temporaryRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("fsw-runtime-{0}" -f [guid]::NewGuid())
    $previousTemp = $env:TEMP
    $previousArchitecture = $env:PROCESSOR_ARCHITECTURE
    $previousWow64 = $env:PROCESSOR_ARCHITEW6432
    $calls = New-Object System.Collections.Generic.List[string]
    try {
        New-Item -ItemType Directory -Path $temporaryRoot | Out-Null
        $env:TEMP = $temporaryRoot
        $env:PROCESSOR_ARCHITECTURE = $ProcessArchitecture
        $env:PROCESSOR_ARCHITEW6432 = $Wow64Architecture

        function Get-AppxPackage {
            [CmdletBinding()]
            param([string]$Name)
            if ($Name -eq '32827MikeFara.fwdslash') { return }
            if ($Name -eq 'Microsoft.WindowsAppRuntime.2') { return $RuntimePackages }
            return
        }
        function Invoke-RestMethod {
            [CmdletBinding()]
            param([string]$Uri)
            $calls.Add("release:$Uri")
            return [pscustomobject]@{
                assets = @([pscustomobject]@{
                    name = 'fwdslash-test.msixbundle'; browser_download_url = 'https://example.invalid/fwdslash.msixbundle'
                })
            }
        }
        function Invoke-WebRequest {
            [CmdletBinding()]
            param([string]$Uri, [string]$OutFile)
            $calls.Add("download:$Uri")
        }
        function Start-Process {
            [CmdletBinding()]
            param([string]$FilePath, [string]$ArgumentList, [switch]$Wait, [switch]$PassThru)
            $calls.Add("start:$FilePath")
            return [pscustomobject]@{ ExitCode = 0 }
        }
        function Add-AppxPackage {
            [CmdletBinding()]
            param([string]$Path)
            $calls.Add("install:$Path")
        }

        & $installer -Version '0.0.0'
        return @($calls)
    } finally {
        $env:TEMP = $previousTemp
        $env:PROCESSOR_ARCHITECTURE = $previousArchitecture
        $env:PROCESSOR_ARCHITEW6432 = $previousWow64
        Remove-Item -LiteralPath $temporaryRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function Assert-RuntimeDownload {
    param([object[]]$Calls, [string]$Architecture, [string]$Message)
    Assert-True -Condition (@($Calls | Where-Object { $_ -like "download:*windowsappruntimeinstall-$Architecture.exe" }).Count -eq 1) -Message $Message
    Assert-True -Condition (@($Calls | Where-Object { $_ -like 'install:*fwdslash-test.msixbundle' }).Count -eq 1) -Message "$Message must still install the app bundle"
}

Assert-RuntimeDownload -Calls (Invoke-InstallerFixture -ProcessArchitecture 'ARM64' -Wow64Architecture '' ) `
    -Architecture 'arm64' -Message 'native ARM64 must select the ARM64 runtime'
Assert-RuntimeDownload -Calls (Invoke-InstallerFixture -ProcessArchitecture 'AMD64' -Wow64Architecture 'ARM64') `
    -Architecture 'arm64' -Message 'emulated x64 PowerShell on ARM64 must select the ARM64 runtime'
Assert-RuntimeDownload -Calls (Invoke-InstallerFixture -ProcessArchitecture 'AMD64' -Wow64Architecture '') `
    -Architecture 'x64' -Message 'native x64 must select the x64 runtime'

$wrongArchitecture = [pscustomobject]@{
    Name = 'Microsoft.WindowsAppRuntime.2'; IsFramework = $true; Architecture = 'X64'; Version = [version]'2.5.0.0'
}
Assert-RuntimeDownload -Calls (Invoke-InstallerFixture -ProcessArchitecture 'ARM64' -Wow64Architecture '' -RuntimePackages @($wrongArchitecture)) `
    -Architecture 'arm64' -Message 'an x64 framework registration cannot satisfy ARM64'

$wrongName = [pscustomobject]@{
    Name = 'Microsoft.WindowsAppRuntime.2Preview'; IsFramework = $true; Architecture = 'ARM64'; Version = [version]'2.5.0.0'
}
Assert-RuntimeDownload -Calls (Invoke-InstallerFixture -ProcessArchitecture 'ARM64' -Wow64Architecture '' -RuntimePackages @($wrongName)) `
    -Architecture 'arm64' -Message 'a similarly named runtime cannot satisfy the exact framework dependency'

$unsupportedRejected = $false
try {
    Invoke-InstallerFixture -ProcessArchitecture 'x86' -Wow64Architecture '' | Out-Null
} catch {
    $unsupportedRejected = $_.Exception.Message -like '*supports only x64 and ARM64*'
}
Assert-True -Condition $unsupportedRejected -Message 'unsupported architectures must be rejected before installation work starts'
