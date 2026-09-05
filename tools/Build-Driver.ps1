[CmdletBinding()]
param(
    [ValidateSet('x64', 'ARM64', 'All')]
    [string]$Architecture = $(if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'ARM64' } else { 'x64' }),
    [ValidateSet('Debug', 'Release')]
    [string]$Configuration = 'Debug'
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$packageVersion = '10.0.28000.2526'
$kitVersion = '10.0.28000.0'

function Invoke-Checked {
    param(
        [Parameter(Mandatory)]
        [string]$FilePath,
        [Parameter(Mandatory)]
        [string[]]$Arguments
    )

    & $FilePath @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$(Split-Path -Leaf $FilePath) exited with $LASTEXITCODE."
    }
}

# The driver package carries the same version as every other 0.0.x copy, and
# the workspace manifest is the one that already gates a release. Parsed as
# text: PowerShell 5.1 has no TOML reader, and adding one for four characters
# is not worth a dependency.
function Get-WorkspaceVersion {
    param(
        [Parameter(Mandatory)]
        [string]$ManifestPath
    )

    $inWorkspacePackage = $false
    foreach ($line in Get-Content -LiteralPath $ManifestPath) {
        $trimmed = $line.Trim()
        if ($trimmed -match '^\[(?<section>[^\]]+)\]$') {
            $inWorkspacePackage = $Matches['section'] -eq 'workspace.package'
            continue
        }
        if ($inWorkspacePackage -and
            $trimmed -match '^version\s*=\s*"(?<version>\d+\.\d+\.\d+)"') {
            return $Matches['version']
        }
    }
    throw "No [workspace.package] version was found in $ManifestPath."
}

$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
if (-not (Test-Path -LiteralPath $vswhere)) {
    throw 'Visual Studio 2026 with Desktop development with C++ is required.'
}

$installation = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $installation) {
    throw 'Visual Studio Desktop development with C++ was not found.'
}
$msbuild = & $vswhere -latest -products '*' -requires Microsoft.Component.MSBuild -find 'MSBuild\**\Bin\MSBuild.exe' |
    Where-Object { $_ -notmatch '\\amd64\\|\\arm64\\' } |
    Select-Object -First 1
if (-not $msbuild) {
    throw 'MSBuild was not found.'
}

$driverVersion = '{0}.0' -f (Get-WorkspaceVersion -ManifestPath (Join-Path $repo 'Cargo.toml'))

# Named distinctly from $Architecture on purpose: PowerShell variable names are
# case-insensitive, so a loop variable spelled the same way would be re-wrapped
# by the parameter's [string] constraint on every iteration.
function Invoke-DriverBuild {
    param(
        [Parameter(Mandatory)]
        [ValidateSet('x64', 'ARM64')]
        [string]$TargetArchitecture
    )

    & (Join-Path $PSScriptRoot 'Restore-DriverDependencies.ps1') -Architecture $TargetArchitecture

    $project = Join-Path $repo 'driver\fswfilter\fswfilter.vcxproj'
    $registeredToolset = Get-ChildItem -LiteralPath (Join-Path $installation 'MSBuild\Microsoft\VC') -Directory |
        ForEach-Object {
            Join-Path $_.FullName "Platforms\$TargetArchitecture\PlatformToolsets\WindowsKernelModeDriver10.0\Toolset.props"
        } |
        Where-Object { Test-Path -LiteralPath $_ } |
        Select-Object -First 1

    if ($registeredToolset) {
        Invoke-Checked -FilePath $msbuild -Arguments @(
            $project,
            '/m',
            "/p:Configuration=$Configuration",
            "/p:Platform=$TargetArchitecture",
            "/p:StampInfVersion=$driverVersion"
        )
        $msbuildOutput = Join-Path $repo "driver\fswfilter\$TargetArchitecture\$Configuration"
        Write-Host "$TargetArchitecture $Configuration driver package built at $msbuildOutput."
        return
    }

    Write-Warning 'The Visual Studio WDK project component is not registered; using the repository-local WDK command-line build.'
    $targetName = $TargetArchitecture.ToLowerInvariant()
    $wdk = Join-Path $repo "packages\Microsoft.Windows.WDK.$targetName.$packageVersion\c"
    $sdk = Join-Path $repo "packages\Microsoft.Windows.SDK.CPP.$targetName.$packageVersion\c"
    $baseSdk = Join-Path $repo "packages\Microsoft.Windows.SDK.CPP.$packageVersion\c"

    $vcTools = Get-ChildItem -LiteralPath (Join-Path $installation 'VC\Tools\MSVC') -Directory |
        Sort-Object { [Version]$_.Name } -Descending |
        Select-Object -First 1
    if (-not $vcTools) {
        throw 'No MSVC toolset was found.'
    }
    $hostName = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64' -and
        (Test-Path -LiteralPath (Join-Path $vcTools.FullName "bin\HostARM64\$TargetArchitecture"))) {
        'HostARM64'
    } else {
        'Hostx64'
    }
    $compilerRoot = Join-Path $vcTools.FullName "bin\$hostName\$TargetArchitecture"
    $cl = Join-Path $compilerRoot 'cl.exe'
    $link = Join-Path $compilerRoot 'link.exe'
    if (-not (Test-Path -LiteralPath $cl) -or -not (Test-Path -LiteralPath $link)) {
        throw "The MSVC $hostName-to-$TargetArchitecture tools are not installed."
    }

    $output = Join-Path $repo "out\driver\$targetName\$Configuration"
    New-Item -ItemType Directory -Force -Path $output | Out-Null
    $object = Join-Path $output 'fswfilter.obj'
    $binary = Join-Path $output 'fswfilter.sys'
    $pdb = Join-Path $output 'fswfilter.pdb'
    $source = Join-Path $repo 'driver\fswfilter\fswfilter.c'

    $definitions = if ($TargetArchitecture -eq 'ARM64') {
        '/D_ARM64_', '/DARM64', '/D_WIN64'
    } else {
        '/D_AMD64_', '/DAMD64', '/D_WIN64'
    }
    $compileArguments = @(
        '/nologo', '/c', '/kernel', '/W4', '/WX', '/wd4324', '/Z7', '/GS',
        '/Zp8', '/guard:cf', '/D_USE_DECLSPECS_FOR_SAL=1', '/DSTD_CALL',
        '/DPOOL_NX_OPTIN=1', '/DWINNT=1', '/D_WIN32_WINNT=0x0A00',
        '/DNTDDI_VERSION=0x0A00000C'
    ) + $definitions + @(
        "/I$($vcTools.FullName)\include",
        "/I$wdk\Include\$kitVersion\km",
        "/I$wdk\Include\$kitVersion\shared",
        "/I$baseSdk\Include\$kitVersion\shared",
        "/I$baseSdk\Include\$kitVersion\ucrt",
        "/Fo$object",
        $source
    )
    if ($Configuration -eq 'Debug') {
        $compileArguments += '/Od'
    } else {
        $compileArguments += '/O2'
    }
    Invoke-Checked -FilePath $cl -Arguments $compileArguments

    $kmLibraries = Join-Path $wdk "Lib\$kitVersion\km\$TargetArchitecture"
    $linkArguments = @(
        '/nologo', "/out:$binary", "/pdb:$pdb", '/driver',
        '/subsystem:native,10.0', '/entry:GsDriverEntry', '/nodefaultlib', '/kernel',
        "/machine:$TargetArchitecture", '/guard:cf', '/dynamicbase', '/nxcompat',
        '/integritycheck', '/debug', '/incremental:no', '/stack:0x40000,0x2000',
        $object,
        (Join-Path $kmLibraries 'BufferOverflowFastFailK.lib'),
        (Join-Path $kmLibraries 'ntoskrnl.lib'),
        (Join-Path $kmLibraries 'hal.lib'),
        (Join-Path $kmLibraries 'wmilib.lib'),
        (Join-Path $kmLibraries 'wdmsec.lib'),
        (Join-Path $kmLibraries 'rtlver.lib'),
        (Join-Path $kmLibraries 'fltMgr.lib')
    )
    if ($TargetArchitecture -eq 'ARM64') {
        $linkArguments += (Join-Path $sdk 'um\arm64\arm64rt.lib')
    }
    if ($Configuration -eq 'Release') {
        $linkArguments += '/opt:ref', '/opt:icf'
    }
    Invoke-Checked -FilePath $link -Arguments $linkArguments

    # The INF ships a placeholder DriverVer; the shipped value is stamped here
    # from today's date and the workspace version, so the .inf in the package
    # never disagrees with the rest of the release.
    $inf = Join-Path $output 'fswfilter.inf'
    Copy-Item -LiteralPath (Join-Path $repo 'driver\fswfilter\fswfilter.inf') -Destination $inf -Force
    $stampArchitecture = if ($TargetArchitecture -eq 'ARM64') { 'arm64' } else { 'amd64' }
    $stampInf = Join-Path $wdk "bin\$kitVersion\$TargetArchitecture\stampinf.exe"
    $infVerif = Join-Path $wdk "tools\$kitVersion\$TargetArchitecture\infverif.exe"
    $inf2Cat = Join-Path $wdk "bin\$kitVersion\x86\Inf2Cat.exe"
    Invoke-Checked -FilePath $stampInf -Arguments @(
        '-f', $inf,
        '-a', $stampArchitecture,
        '-d', '*',
        '-v', $driverVersion,
        '-c', 'fswfilter.cat'
    )
    Invoke-Checked -FilePath $infVerif -Arguments @('/u', $inf)
    $catalogOs = if ($TargetArchitecture -eq 'ARM64') { '10_GE_ARM64' } else { '10_GE_X64' }
    Invoke-Checked -FilePath $inf2Cat -Arguments @("/driver:$output", "/os:$catalogOs")

    Write-Host "$TargetArchitecture $Configuration driver package built at $output (DriverVer version $driverVersion, unsigned). Load it only in the checkpointed Hyper-V lab VM."
}

$targets = if ($Architecture -eq 'All') { @('x64', 'ARM64') } else { @($Architecture) }
foreach ($target in $targets) {
    Invoke-DriverBuild -TargetArchitecture $target
}
