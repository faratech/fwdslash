# Executes release.yml's real version-step body with mocked Cargo metadata and
# controlled GitHub refs. It never invokes a build, signer, release command,
# or network operation.

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Assert-True {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw $Message }
}

function Get-ReleaseVersionStep {
    $workflow = Join-Path $PSScriptRoot '../../.github/workflows/release.yml'
    $lines = [System.IO.File]::ReadAllLines($workflow)
    $start = -1
    $run = -1
    $end = -1
    for ($index = 0; $index -lt $lines.Length; $index++) {
        if ($lines[$index] -eq '      - name: Derive the package version and validate a tag, if present') {
            $start = $index
            continue
        }
        if ($start -ge 0 -and $lines[$index] -eq '        run: |') {
            $run = $index
            continue
        }
        if ($run -ge 0 -and $lines[$index].StartsWith('      - name: ')) {
            $end = $index
            break
        }
    }
    if ($start -lt 0 -or $run -lt 0 -or $end -lt 0) {
        throw 'Could not locate the release version step in release.yml.'
    }
    $body = for ($index = $run + 1; $index -lt $end; $index++) {
        $line = $lines[$index]
        if ($line.Length -ge 10) { $line.Substring(10) } else { '' }
    }
    return ($body -join [Environment]::NewLine)
}

function Invoke-ReleaseVersionStep {
    param([string]$Ref, [string]$RefName)

    $temporaryRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("fsw-release-version-{0}" -f [guid]::NewGuid())
    $stepScript = Join-Path $temporaryRoot 'version-step.ps1'
    $outputFile = Join-Path $temporaryRoot 'github-output.txt'
    $previousRef = $env:GITHUB_REF
    $previousRefName = $env:GITHUB_REF_NAME
    $previousOutput = $env:GITHUB_OUTPUT
    try {
        New-Item -ItemType Directory -Path $temporaryRoot | Out-Null
        Set-Content -LiteralPath $stepScript -Value (Get-ReleaseVersionStep) -Encoding utf8
        $env:GITHUB_REF = $Ref
        $env:GITHUB_REF_NAME = $RefName
        $env:GITHUB_OUTPUT = $outputFile
        function cargo {
            param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Arguments)
            if ($Arguments.Count -ge 1 -and $Arguments[0] -eq 'metadata') {
                return '{"packages":[{"name":"fsw-core","version":"0.0.5"}]}'
            }
            throw "Unexpected Cargo invocation: $Arguments"
        }
        try {
            & $stepScript
            return [pscustomobject]@{ Succeeded = $true; Error = ''; Output = (Get-Content -LiteralPath $outputFile -Raw) }
        } catch {
            return [pscustomobject]@{ Succeeded = $false; Error = $_.Exception.Message; Output = '' }
        }
    } finally {
        $env:GITHUB_REF = $previousRef
        $env:GITHUB_REF_NAME = $previousRefName
        $env:GITHUB_OUTPUT = $previousOutput
        Remove-Item -LiteralPath $temporaryRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function Assert-VersionSuccess {
    param([psobject]$Result, [string]$Message)
    Assert-True -Condition $Result.Succeeded -Message "${Message}: $($Result.Error)"
    Assert-True -Condition ($Result.Output -match 'version=0\.0\.5\.0') -Message "$Message must derive 0.0.5.0"
    Assert-True -Condition ($Result.Output -match 'short=0\.0\.5') -Message "$Message must derive short version 0.0.5"
}

Assert-VersionSuccess -Result (Invoke-ReleaseVersionStep -Ref 'refs/heads/main' -RefName 'main') `
    -Message 'manual main must use the Cargo workspace version'
Assert-VersionSuccess -Result (Invoke-ReleaseVersionStep -Ref 'refs/heads/version-fix' -RefName 'version-fix') `
    -Message 'a dispatch branch named version-fix must not be treated as a tag'
Assert-VersionSuccess -Result (Invoke-ReleaseVersionStep -Ref 'refs/tags/v0.0.5' -RefName 'v0.0.5') `
    -Message 'a matching release tag must validate'

$mismatch = Invoke-ReleaseVersionStep -Ref 'refs/tags/v0.0.6' -RefName 'v0.0.6'
Assert-True -Condition (-not $mismatch.Succeeded -and $mismatch.Error -like 'Tag v0.0.6 does not match*') `
    -Message 'a mismatched release tag must fail validation'

$malformed = Invoke-ReleaseVersionStep -Ref 'refs/tags/vnot-a-version' -RefName 'vnot-a-version'
Assert-True -Condition (-not $malformed.Succeeded -and $malformed.Error -like 'Tag vnot-a-version does not match*') `
    -Message 'a malformed release tag must fail validation'
