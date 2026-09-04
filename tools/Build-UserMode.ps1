[CmdletBinding()]
param(
    [ValidateSet('x86', 'x64', 'ARM64')]
    [string]$Architecture = $(if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'ARM64' } else { 'x64' }),
    # ReleaseCpp exists only for tools/Measure-Runtime.ps1: C++ binaries built
    # into out\user\<arch>\ReleaseCpp so runtime comparisons never clobber the
    # Rust exes that tools/package_msix.py stages into ...Release.
    [ValidateSet('Debug', 'Release', 'ReleaseCpp')]
    [string]$Configuration = 'Debug',

    # Build the settings app for life inside an MSIX. Unpackaged, the Windows App
    # SDK compiles in an auto-initializer that calls MddBootstrapInitialize2 and
    # exit()s on failure; with package identity that bootstrap must not run, and
    # the framework is reached through the manifest PackageDependency instead.
    [switch]$Packaged,

    # Primary resource map name for the packaged build. Must equal the package
    # Identity/Name or ms-appx:// lookups resolve against the wrong map.
    [string]$PackageIdentityName = '32827MikeFara.fwdslash'
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot

function Invoke-Checked {
    param([string]$FilePath, [string[]]$Arguments)
    & $FilePath @Arguments | ForEach-Object { Write-Host $_ }
    if ($LASTEXITCODE -ne 0) {
        throw "$(Split-Path -Leaf $FilePath) exited with $LASTEXITCODE."
    }
}

$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$installation = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $installation) {
    throw 'Visual Studio Desktop development with C++ was not found.'
}
$vcTools = Get-ChildItem -LiteralPath (Join-Path $installation 'VC\Tools\MSVC') -Directory |
    Sort-Object { [Version]$_.Name } -Descending |
    Select-Object -First 1
if (-not $vcTools) { throw 'No MSVC toolset was found.' }

$kitRoot = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10'
$kitVersion = Get-ChildItem -LiteralPath (Join-Path $kitRoot 'Include') -Directory |
    Where-Object { Test-Path -LiteralPath (Join-Path $_.FullName 'um\windows.h') } |
    Sort-Object { [Version]$_.Name } -Descending |
    Select-Object -First 1
if (-not $kitVersion) { throw 'A Windows 10 or 11 SDK was not found.' }

$targetFolder = $Architecture
$hostFolder = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64' -and
    (Test-Path -LiteralPath (Join-Path $vcTools.FullName "bin\HostARM64\$targetFolder"))) {
    'HostARM64'
} else {
    'Hostx64'
}
$compilerRoot = Join-Path $vcTools.FullName "bin\$hostFolder\$targetFolder"
$cl = Join-Path $compilerRoot 'cl.exe'
$link = Join-Path $compilerRoot 'link.exe'
if (-not (Test-Path -LiteralPath $cl) -or -not (Test-Path -LiteralPath $link)) {
    throw "The MSVC $hostFolder-to-$Architecture tools are not installed."
}
$rc = Join-Path $kitRoot "bin\$($kitVersion.Name)\x64\rc.exe"
if (-not (Test-Path -LiteralPath $rc)) {
    throw 'The Windows SDK resource compiler was not found.'
}

$targetName = $Architecture.ToLowerInvariant()
$output = Join-Path $repo "out\user\$targetName\$Configuration"
$objects = Join-Path $output 'obj'
New-Item -ItemType Directory -Force -Path $objects | Out-Null
$sdkInclude = $kitVersion.FullName
$compileBase = @(
    '/nologo', '/c', '/std:c++20', '/EHsc', '/W4', '/WX', '/permissive-',
    '/utf-8', '/MT', '/DUNICODE', '/D_UNICODE', '/DWIN32_LEAN_AND_MEAN',
    '/DNOMINMAX',
    "/I$($vcTools.FullName)\include",
    "/I$sdkInclude\ucrt", "/I$sdkInclude\shared", "/I$sdkInclude\um",
    "/I$repo\src\core\include", "/I$repo\include"
)
if ($Configuration -eq 'Debug') { $compileBase += '/Od', '/Z7' } else { $compileBase += '/O2' }
function Compile-Source {
    param([string]$Name, [string]$Source, [string[]]$ExtraArguments = @())
    $object = Join-Path $objects "$Name.obj"
    Invoke-Checked -FilePath $cl -Arguments ($compileBase + $ExtraArguments + @("/Fo$object", (Join-Path $repo $Source)))
    return $object
}

$pathObject = Compile-Source 'path_resolver' 'src\core\path_resolver.cpp'
$registryObject = Compile-Source 'wsl_registry' 'src\core\wsl_registry.cpp'
$identityObject = Compile-Source 'package_identity' 'src\core\package_identity.cpp'
$sdkLibraryArchitecture = $targetName
$vcLibraryArchitecture = $targetName
$libraryPaths = @(
    "/libpath:$($vcTools.FullName)\lib\$vcLibraryArchitecture",
    "/libpath:$kitRoot\Lib\$($kitVersion.Name)\ucrt\$sdkLibraryArchitecture",
    "/libpath:$kitRoot\Lib\$($kitVersion.Name)\um\$sdkLibraryArchitecture"
)
$machine = $Architecture
$linkBase = @(
    '/nologo', "/machine:$machine", '/dynamicbase', '/nxcompat', '/guard:cf',
    '/incremental:no'
) + $libraryPaths

function Link-Target {
    param(
        [string]$Name,
        [string[]]$ObjectFiles,
        [ValidateSet('console', 'windows', 'dll')]
        [string]$Kind,
        [string[]]$Libraries = @(),
        [string[]]$ExtraArguments = @()
    )
    $extension = if ($Kind -eq 'dll') { '.dll' } else { '.exe' }
    $arguments = $linkBase + @("/out:$output\$Name$extension")
    if ($Kind -eq 'dll') {
        $arguments += '/dll'
    } else {
        $arguments += "/subsystem:$Kind"
    }
    Invoke-Checked -FilePath $link -Arguments ($arguments + $ExtraArguments + $ObjectFiles + $Libraries)
}

$controllerObject = Compile-Source 'controller' 'src\controller\main.cpp'
$brokerObject = Compile-Source 'broker' 'src\broker\main.cpp'
$testObject = Compile-Source 'core_tests' 'tests\core_tests.cpp'
$addressBarTestObject = Compile-Source 'address_bar_integration' 'tests\address_bar_integration.cpp'
$filesystemTestObject = Compile-Source 'filesystem_integration' 'tests\filesystem_integration.cpp'
$appResource = Join-Path $objects 'fwdslash.res'
Invoke-Checked -FilePath $rc -Arguments @(
    '/nologo', "/i$sdkInclude\um", "/i$sdkInclude\shared",
    "/fo$appResource", (Join-Path $repo 'assets\fwdslash.rc'))
Link-Target 'fwdslash' @($controllerObject, $pathObject, $registryObject, $identityObject, $appResource) 'console' @('shell32.lib', 'user32.lib', 'advapi32.lib', 'FltLib.lib')
Link-Target 'fswbroker' @($brokerObject, $pathObject, $registryObject, $identityObject, $appResource) 'windows' @('shell32.lib', 'user32.lib', 'gdi32.lib', 'advapi32.lib', 'ole32.lib', 'oleaut32.lib', 'uiautomationcore.lib', 'FltLib.lib', 'uuid.lib')
Link-Target 'fswcore_tests' @($testObject, $pathObject) 'console' @('advapi32.lib')
Link-Target 'fsw_address_bar_integration' @($addressBarTestObject) 'console' @('shell32.lib', 'ole32.lib', 'oleaut32.lib', 'user32.lib')
Link-Target 'fsw_filesystem_integration' @($filesystemTestObject) 'console'

$msbuild = Join-Path $installation 'MSBuild\Current\Bin\MSBuild.exe'
if (-not (Test-Path -LiteralPath $msbuild)) {
    throw 'MSBuild was not found for the WinUI 3 settings application.'
}
$settingsProject = Join-Path $repo 'src\settings\ForwardSlashWindows.Settings.vcxproj'
$settingsIntermediate = Join-Path $output 'settings-obj'
$settingsPlatform = if ($Architecture -eq 'x86') { 'Win32' } else { $Architecture }
$settingsArguments = @(
    $settingsProject,
    '/restore',
    '/m:1',
    "/p:Configuration=$Configuration",
    "/p:Platform=$settingsPlatform",
    "/p:OutDir=$output\",
    "/p:IntDir=$settingsIntermediate\",
    '/p:UseMultiToolTask=false'
)
if ($Packaged) {
    # WindowsPackageType=MSIX is not usable here: it makes the SDK demand an
    # AppxManifest item on the project, which would drag the whole package
    # definition into one component's vcxproj. Gate the two auto-initializers
    # directly instead. WindowsAppSdkBootstrapInitialize defaults to true only
    # when unset, so setting it false is what removes the MddBootstrapInitialize2
    # call and its exit() on failure.
    $settingsArguments += @(
        '/p:WindowsAppSdkBootstrapInitialize=false',
        '/p:WindowsAppSdkDeploymentManagerInitialize=false',
        '/p:PrependPriInitialPath=false',
        "/p:ProjectPriIndexName=$PackageIdentityName"
    )
}
# ReleaseCpp has no vcxproj configuration (the comparison harness measures the
# cl-built exes; the Rust settings app stands in for the settings surface).
if ('Debug', 'Release' -contains $Configuration) {
    Invoke-Checked -FilePath $msbuild -Arguments $settingsArguments
} else {
    Write-Host "Skipping the WinUI 3 settings app: '$Configuration' has no vcxproj configuration."
}

$payloadDirectories = @(
    @{ Source = 'shell\cmd'; Destination = 'shell\cmd' },
    @{ Source = 'shell\powershell'; Destination = 'shell\powershell' }
)
foreach ($payload in $payloadDirectories) {
    $destination = Join-Path $output $payload.Destination
    New-Item -ItemType Directory -Force -Path $destination | Out-Null
    Copy-Item -Path (Join-Path $repo ($payload.Source + '\*')) -Destination $destination -Force
}
foreach ($script in 'Install-CmdAdapter.ps1', 'Uninstall-CmdAdapter.ps1',
                    'Install-PowerShellAdapter.ps1', 'Uninstall-PowerShellAdapter.ps1') {
    Copy-Item -LiteralPath (Join-Path $repo "tools\$script") -Destination $output -Force
}

Write-Host "User-mode $Architecture artifacts built at $output."
