#Requires -Version 5.1
<#
.SYNOPSIS
    Prepares the checkpointed lab guest to load the test-signed fwdslash
    minifilter.

.DESCRIPTION
    LAB GUEST ONLY - NEVER A PHYSICAL WORKSTATION. This script turns on test
    signing, trusts a self-signed code-signing certificate machine-wide, enables
    Driver Verifier, and (with -FakeShare) weakens loopback SMB name checking.
    Every one of those is a permanent, machine-wide change that lowers the
    security posture of the machine it runs on. It belongs in a disposable
    Hyper-V guest that has a 'clean-os' checkpoint to roll back to, created by
    tools\New-DriverLabVm.ps1. It refuses to run on hardware that does not look
    like a virtual machine unless -Force is given.

    Run elevated, inside the guest. Reboot (or pass -Reboot) before loading the
    driver: test signing and Driver Verifier both take effect at boot.

.PARAMETER CertificatePath
    The lab .cer produced by tools\Package-Driver.ps1 -Lab. Imported into
    LocalMachine\Root (so the chain validates) and LocalMachine\TrustedPublisher
    (so the catalog is accepted without a prompt).

.PARAMETER NoVerifier
    Skips Driver Verifier. Use only for a quick smoke run; the release gate
    requires the harness to run with Verifier active for the whole session.

.PARAMETER KernelDebug
    Also runs `bcdedit /debug on` so a kernel debugger can attach. Configure the
    transport separately (bcdedit /dbgsettings net ...).

.PARAMETER InstallWsl
    Runs `wsl --install -d Ubuntu --no-launch`. Needs nested virtualization on
    the guest (New-DriverLabVm.ps1 -ExposeVirtualization) and a reboot.

.PARAMETER FakeShare
    Stands up a local SMB share that impersonates \\wsl.localhost\<Distribution>
    so the redirection mechanism can be tested without WSL. See the LIMITS note
    printed at the end and docs\driver-lab.md: this proves the reparse path, not
    9P semantics.

.PARAMETER Distribution
    Name used for the fake share and the hosts alias. Default 'Ubuntu'.

.PARAMETER Reboot
    Restarts the guest at the end.

.PARAMETER Force
    Proceeds even when the machine does not look like a virtual machine. There
    is no good reason to use this.

.EXAMPLE
    .\Bootstrap-DriverLabGuest.ps1 -CertificatePath C:\FswLab\fwdslash-lab.cer -FakeShare -Reboot
#>
[CmdletBinding()]
param(
    [string]$CertificatePath,

    [switch]$NoVerifier,

    [switch]$KernelDebug,

    [switch]$InstallWsl,

    [switch]$FakeShare,

    [string]$Distribution = 'Ubuntu',

    [switch]$Reboot,

    [switch]$Force
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Driver Verifier flag mask for the set docs/compatibility.md names.
#   0x0001 Special Pool
#   0x0002 Force IRQL Checking
#   0x0008 Pool Tracking
#   0x0010 I/O Verification
#   0x0020 Deadlock Detection
#   0x0100 Security Checks
#   0x0800 Miscellaneous Checks
# Low-resource simulation (0x0004) is deliberately NOT set: the filter is
# fail-open by design, so randomized allocation failures would mask real bugs
# behind a pass. The unload-under-load and malformed-message cases in
# Test-Driver.ps1 cover the failure paths instead.
$script:VerifierFlagMask = 0x93B
$script:DriverName = 'fswfilter.sys'
$script:LabRoot = 'C:\FswLab'
$script:Warnings = 0

function Write-Section {
    param([string]$Title)
    Write-Host ''
    Write-Host "== $Title"
}

function Write-Step {
    param([string]$Message)
    Write-Host "   $Message"
}

function Write-Soft {
    param([string]$Message)
    $script:Warnings++
    Write-Warning $Message
}

function Test-Elevated {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Test-VirtualMachine {
    try {
        $system = Get-CimInstance -ClassName Win32_ComputerSystem
        $model = "$($system.Model)"
        $manufacturer = "$($system.Manufacturer)"
    } catch {
        return $false
    }
    $needles = @('Virtual Machine', 'VMware', 'VirtualBox', 'QEMU', 'Xen', 'Hyper-V', 'KVM', 'Parallels')
    foreach ($needle in $needles) {
        if ($model -like "*$needle*" -or $manufacturer -like "*$needle*") {
            return $true
        }
    }
    return $false
}

function Invoke-Native {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [Parameter(Mandatory = $true)][string[]]$Arguments,
        [switch]$Tolerate
    )
    $output = & $FilePath @Arguments 2>&1
    $code = $LASTEXITCODE
    if ($code -ne 0 -and -not $Tolerate) {
        throw "$(Split-Path -Leaf $FilePath) $($Arguments -join ' ') exited with $code : $output"
    }
    return [pscustomobject]@{ ExitCode = $code; Output = ($output | Out-String).Trim() }
}

Write-Host 'fwdslash driver lab - guest bootstrap'
Write-Host '-------------------------------------'
Write-Host 'LAB GUEST ONLY. This lowers the machine security posture permanently.'

if (-not (Test-Elevated)) {
    throw 'Run this from an elevated PowerShell inside the lab guest.'
}
if (-not (Test-VirtualMachine)) {
    if (-not $Force) {
        throw 'This machine does not look like a virtual machine. Refusing: test signing, a machine-wide trusted publisher and Driver Verifier must not be configured on a physical workstation. Use -Force only if the detection is wrong.'
    }
    Write-Soft 'Virtual-machine detection failed but -Force was given. You are on your own.'
}

# ---------------------------------------------------------------- boot policy
Write-Section 'Boot policy'
$firmware = Invoke-Native -FilePath 'bcdedit.exe' -Arguments @('/set', '{current}', 'testsigning', 'on') -Tolerate
if ($firmware.ExitCode -ne 0) {
    throw "bcdedit could not enable test signing ($($firmware.Output)). Secure Boot must be OFF in the guest firmware; New-DriverLabVm.ps1 disables it with Set-VMFirmware -EnableSecureBoot Off."
}
Write-Step 'test signing: on (takes effect after reboot)'

if ($KernelDebug) {
    Invoke-Native -FilePath 'bcdedit.exe' -Arguments @('/debug', 'on') | Out-Null
    Write-Step 'kernel debugging: on (set the transport with bcdedit /dbgsettings)'
}

# ---------------------------------------------------------------- certificate
Write-Section 'Lab certificate'
if ([string]::IsNullOrWhiteSpace($CertificatePath)) {
    Write-Soft 'No -CertificatePath given; the test-signed catalog will not be trusted and pnputil will refuse the package.'
} elseif (-not (Test-Path -LiteralPath $CertificatePath)) {
    throw 'The certificate path given does not exist.'
} else {
    foreach ($store in @('Root', 'TrustedPublisher')) {
        if (Get-Command -Name Import-Certificate -ErrorAction SilentlyContinue) {
            Import-Certificate -FilePath $CertificatePath -CertStoreLocation "Cert:\LocalMachine\$store" | Out-Null
        } else {
            Invoke-Native -FilePath 'certutil.exe' -Arguments @('-addstore', '-f', $store, $CertificatePath) | Out-Null
        }
        Write-Step "imported into LocalMachine\$store"
    }
}

# ------------------------------------------------------------ driver verifier
Write-Section 'Driver Verifier'
if ($NoVerifier) {
    Invoke-Native -FilePath 'verifier.exe' -Arguments @('/reset') -Tolerate | Out-Null
    Write-Step 'skipped (-NoVerifier); the release gate requires a Verifier-enabled run'
} else {
    $mask = '0x{0:X}' -f $script:VerifierFlagMask
    $verifier = Invoke-Native -FilePath 'verifier.exe' -Arguments @('/flags', $mask, '/driver', $script:DriverName) -Tolerate
    if ($verifier.ExitCode -ne 0) {
        Write-Soft "verifier.exe returned $($verifier.ExitCode): $($verifier.Output)"
    } else {
        Write-Step "enabled for $($script:DriverName), flags $mask"
        Write-Step '  0x0001 special pool      0x0002 force IRQL checking  0x0008 pool tracking'
        Write-Step '  0x0010 I/O verification  0x0020 deadlock detection   0x0100 security checks'
        Write-Step '  0x0800 miscellaneous checks'
        Write-Step 'Verifier tracks the driver by name; it applies whether or not the driver is installed yet.'
    }
}

# ------------------------------------------------------------------------ WSL
Write-Section 'WSL'
if ($InstallWsl) {
    $wsl = Invoke-Native -FilePath 'wsl.exe' -Arguments @('--install', '-d', 'Ubuntu', '--no-launch') -Tolerate
    if ($wsl.ExitCode -ne 0) {
        Write-Soft "wsl --install returned $($wsl.ExitCode). WSL2 in a guest needs nested virtualization (New-DriverLabVm.ps1 -ExposeVirtualization). Fall back to -FakeShare."
    } else {
        Write-Step 'Ubuntu queued for install; it needs a reboot and one first launch to create a user'
    }
} else {
    Write-Step 'skipped (-InstallWsl not given)'
}

# ----------------------------------------------------------------- fake share
if ($FakeShare) {
    Write-Section "Fake \\wsl.localhost\$Distribution share"

    $hosts = Join-Path $env:SystemRoot 'System32\drivers\etc\hosts'
    $hostsText = Get-Content -LiteralPath $hosts -Raw -ErrorAction SilentlyContinue
    if ($null -eq $hostsText) { $hostsText = '' }
    if ($hostsText -match '(?m)^\s*127\.0\.0\.1\s+wsl\.localhost\s*$') {
        Write-Step 'hosts: 127.0.0.1 wsl.localhost already present'
    } else {
        Add-Content -LiteralPath $hosts -Value "127.0.0.1 wsl.localhost"
        Write-Step 'hosts: added 127.0.0.1 wsl.localhost'
    }

    $shareRoot = Join-Path $script:LabRoot $Distribution
    New-Item -ItemType Directory -Force -Path $shareRoot | Out-Null

    # A small corpus that exercises the shapes the parity matrix cares about.
    $directories = @('etc', 'etc\apt', 'home', 'home\labuser', 'usr\share\doc', 'var\log')
    foreach ($relative in $directories) {
        New-Item -ItemType Directory -Force -Path (Join-Path $shareRoot $relative) | Out-Null
    }
    $files = @{
        'etc\hostname'                = "fswlab`n"
        'etc\hosts'                   = "127.0.0.1 localhost`n"
        'etc\apt\sources.list'        = "deb http://example.invalid stable main`n"
        'home\labuser\notes.txt'      = "lab corpus`n"
        'var\log\empty.log'           = ''
    }
    foreach ($relative in $files.Keys) {
        Set-Content -LiteralPath (Join-Path $shareRoot $relative) -Value $files[$relative] -NoNewline -Encoding ascii
    }

    # Unicode: combining marks, CJK and Greek in one name.
    $unicodeName = [string][char]0x00FC + 'n' + [string][char]0x00EF + 'c' + [string][char]0x00F6 + 'd' +
        [string][char]0x00E9 + '-' + [string][char]0x65E5 + [string][char]0x672C + [string][char]0x8A9E + '-' +
        [string][char]0x03A9 + [string][char]0x03BC + '.txt'
    Set-Content -LiteralPath (Join-Path $shareRoot $unicodeName) -Value 'unicode' -Encoding utf8
    Write-Step "unicode probe file created"

    # Long path: > 260 characters end to end. Created through \\?\ because the
    # guest may not have LongPathsEnabled.
    $longSegment = 'l' * 60
    $longRelative = "$longSegment\$longSegment\$longSegment\$longSegment\deep.txt"
    $longFull = Join-Path $shareRoot $longRelative
    try {
        $longDirectory = Split-Path -Parent $longFull
        [void][System.IO.Directory]::CreateDirectory("\\?\$longDirectory")
        [System.IO.File]::WriteAllText("\\?\$longFull", 'long')
        Write-Step "long-path probe created ($($longFull.Length) characters)"
    } catch {
        Write-Soft "The long-path probe could not be created: $($_.Exception.Message)"
    }

    # Trailing-dot / trailing-space: Win32 strips both, so these names are only
    # reachable through \\?\. Test-Driver.ps1 asserts that the alias and the UNC
    # side agree about that, not that the name is usable.
    foreach ($odd in @('trailing.dot.', 'trailing.space ')) {
        try {
            [System.IO.File]::WriteAllText("\\?\$(Join-Path $shareRoot $odd)", 'odd')
            Write-Step "trailing-character probe created: '$odd'"
        } catch {
            Write-Soft "The trailing-character probe '$odd' could not be created: $($_.Exception.Message)"
        }
    }

    $existingShare = Get-SmbShare -Name $Distribution -ErrorAction SilentlyContinue
    if ($null -eq $existingShare) {
        New-SmbShare -Name $Distribution -Path $shareRoot -FullAccess 'Everyone' | Out-Null
        Write-Step "SMB share '$Distribution' created"
    } elseif ($existingShare.Path -ne $shareRoot) {
        throw "An SMB share named '$Distribution' already points somewhere else. Remove it first."
    } else {
        Write-Step "SMB share '$Distribution' already present"
    }

    # Loopback SMB by an alias needs two machine-wide relaxations:
    #   DisableStrictNameChecking  - the server answers to a name that is not
    #                                its own computer name or a registered
    #                                CNAME (here: wsl.localhost).
    #   BackConnectionHostNames    - the loopback-check mitigation stops
    #                                treating an alias-addressed loopback
    #                                connection as a reflection attack.
    $serverParameters = 'HKLM:\SYSTEM\CurrentControlSet\Services\LanmanServer\Parameters'
    New-ItemProperty -Path $serverParameters -Name 'DisableStrictNameChecking' -Value 1 -PropertyType DWord -Force | Out-Null
    Write-Step 'LanmanServer\Parameters DisableStrictNameChecking = 1'

    $msv = 'HKLM:\SYSTEM\CurrentControlSet\Control\Lsa\MSV1_0'
    if (-not (Test-Path -LiteralPath $msv)) {
        New-Item -Path $msv -Force | Out-Null
    }
    $backConnection = @('wsl.localhost')
    $currentBack = (Get-ItemProperty -Path $msv -Name 'BackConnectionHostNames' -ErrorAction SilentlyContinue)
    if ($null -ne $currentBack -and $currentBack.PSObject.Properties.Name -contains 'BackConnectionHostNames') {
        $backConnection = @($currentBack.BackConnectionHostNames) + 'wsl.localhost' | Sort-Object -Unique
    }
    New-ItemProperty -Path $msv -Name 'BackConnectionHostNames' -Value $backConnection -PropertyType MultiString -Force | Out-Null
    Write-Step 'Lsa\MSV1_0 BackConnectionHostNames += wsl.localhost'

    Write-Host ''
    Write-Host '   LIMITS. The fake share is an SMB share on a local NTFS directory. It'
    Write-Host '   proves the driver rewrites a drive-root path to a UNC path and that the'
    Write-Host '   redirector serves it. It does NOT reproduce 9P/plan9 semantics, WSL'
    Write-Host '   metadata, case-sensitive directories, symlinks or Linux permissions.'
    Write-Host '   A green -FakeShare run is necessary, not sufficient: the gate rows in'
    Write-Host '   docs/compatibility.md stay pending until a real WSL distribution passes.'
    Write-Host ''
    Write-Host '   NOTE: this provisions the SMB share only, not a distribution the broker'
    Write-Host '   can see - the broker publishes what HKCU\...\Lxss lists, and nothing here'
    Write-Host '   writes to it. Test-Driver.ps1 -FakeShare seeds a synthetic Lxss'
    Write-Host '   registration itself (and removes it in step h), because this script only'
    Write-Host '   runs once at checkpoint-creation time, while Test-Driver.ps1 is re-pushed'
    Write-Host '   and re-run against the restored checkpoint on every gate run (issue #39).'
} else {
    Write-Section 'Fake share'
    Write-Step 'skipped (-FakeShare not given); a real WSL distribution must serve \\wsl.localhost'
}

# --------------------------------------------------------------- verification
Write-Section 'Verification'
$testsigning = (Invoke-Native -FilePath 'bcdedit.exe' -Arguments @('/enum', '{current}') -Tolerate).Output
$testsigningOn = $testsigning -match '(?im)^\s*testsigning\s+Yes\s*$'
Write-Step "testsigning reported by bcdedit : $testsigningOn (reads 'Yes' only after the reboot on some builds)"

$sharePath = "\\wsl.localhost\$Distribution"
$shareReachable = $false
try {
    $shareReachable = Test-Path -LiteralPath $sharePath
} catch {
    $shareReachable = $false
}
Write-Step "Test-Path $sharePath : $shareReachable"

Write-Host ''
if (-not $shareReachable) {
    Write-Host '   \\wsl.localhost\<Distribution> is NOT reachable yet. It MUST be true before'
    Write-Host '   any driver test - the driver only rewrites a path to that UNC target, so an'
    Write-Host '   unreachable target makes every parity case fail for the wrong reason.'
    Write-Host '   With -FakeShare this usually clears after the reboot (the LSA and'
    Write-Host '   LanmanServer values are read at service start). With WSL, launch the'
    Write-Host '   distribution once so the 9P server starts.'
} else {
    Write-Host '   The UNC target is reachable. Reboot, then run Test-Driver.ps1.'
}

if ($script:Warnings -gt 0) {
    Write-Host ''
    Write-Host "$($script:Warnings) warning(s) above - read them before trusting a harness run."
}

Write-Host ''
Write-Host 'Next:'
Write-Host '   1. Reboot (test signing and Driver Verifier are boot-time settings).'
Write-Host "   2. Confirm: Test-Path \\wsl.localhost\$Distribution  ->  True"
Write-Host '   3. .\Test-Driver.ps1 -PackageZip <lab zip>' -NoNewline
if ($FakeShare) { Write-Host ' -FakeShare' } else { Write-Host '' }

if ($Reboot) {
    Write-Host ''
    Write-Host 'Rebooting now...'
    Restart-Computer -Force
}
exit 0
