$ErrorActionPreference = 'Stop'
$artifacts = 'C:\Fsw\artifacts'
$results = 'C:\Fsw\results'
$started = Get-Date
$records = [System.Collections.Generic.List[object]]::new()

function Invoke-FswTest {
    param([string]$Name, [scriptblock]$Action)
    try {
        & $Action
        $records.Add([pscustomobject]@{ name = $Name; passed = $true; error = $null })
    } catch {
        $records.Add([pscustomobject]@{ name = $Name; passed = $false; error = $_.Exception.Message })
    }
}

Invoke-FswTest 'resolver-unit-tests' {
    & (Join-Path $artifacts 'fswcore_tests.exe')
    if ($LASTEXITCODE -ne 0) { throw "Resolver tests exited with $LASTEXITCODE" }
}

Invoke-FswTest 'controller-status' {
    & (Join-Path $artifacts 'fwdslash.exe') status
    if ($LASTEXITCODE -ne 0) { throw "Controller status exited with $LASTEXITCODE" }
}

Invoke-FswTest 'broker-lifecycle' {
    & (Join-Path $artifacts 'fwdslash.exe') start
    if ($LASTEXITCODE -ne 0) { throw "Broker start exited with $LASTEXITCODE" }
    Start-Sleep -Milliseconds 500
    $broker = Get-Process -Name fswbroker -ErrorAction SilentlyContinue
    if (-not $broker) { throw 'Broker did not remain running' }
    & (Join-Path $artifacts 'fwdslash.exe') stop
    if ($LASTEXITCODE -ne 0) { throw "Broker stop exited with $LASTEXITCODE" }
    $deadline = (Get-Date).AddSeconds(5)
    while ((Get-Process -Name fswbroker -ErrorAction SilentlyContinue) -and
           (Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 100
    }
    if (Get-Process -Name fswbroker -ErrorAction SilentlyContinue) {
        throw 'Broker did not stop within five seconds'
    }
}

$report = [pscustomobject]@{
    started = $started.ToUniversalTime().ToString('o')
    finished = (Get-Date).ToUniversalTime().ToString('o')
    machine = $env:COMPUTERNAME
    architecture = $env:PROCESSOR_ARCHITECTURE
    windowsBuild = [Environment]::OSVersion.Version.ToString()
    toolVersion = '0.1.0'
    environment = 'Windows Sandbox'
    wslVersion = $null
    tests = $records
    passed = -not ($records.passed -contains $false)
}
$report | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $results 'sandbox-results.json') -Encoding utf8
