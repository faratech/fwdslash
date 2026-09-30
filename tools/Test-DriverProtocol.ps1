[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$labPath = Join-Path $PSScriptRoot 'Test-Driver.ps1'
$tokens = $null
$parseErrors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($labPath, [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count -ne 0) { throw ($parseErrors.Message -join '; ') }

# Compile only the lab's native declarations and load its packet builder.
# No lab script body, driver-port call, install, or filesystem probe is run.
$native = $ast.Find({
    param($node)
    $node -is [Management.Automation.Language.CommandAst] -and $node.GetCommandName() -eq 'Add-Type'
}, $true)
$definition = @($native.CommandElements | Where-Object {
    $_ -is [Management.Automation.Language.StringConstantExpressionAst] -and $_.Value.Contains('namespace FswLab')
})
if ($definition.Count -ne 1) { throw 'Expected one native declaration block' }
if (-not ('FswLab.Native' -as [type])) { Add-Type -TypeDefinition $definition[0].Value }
$builder = $ast.Find({
    param($node)
    $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'New-MappingBuffer'
}, $true)
if ($null -eq $builder) { throw 'Packet builder not found' }
. ([scriptblock]::Create($builder.Extent.Text))

function Assert-Equal($Actual, $Expected, [string]$Name) {
    if ($Actual -ne $Expected) { throw "$Name expected $Expected, got $Actual" }
}

$header = [IO.File]::ReadAllText((Join-Path $repo 'include\fsw_filter_protocol.h'))
if ($header -notmatch '#define FSW_PROTOCOL_VERSION 4u' -or
    $header -notmatch '#define FSW_MAX_VOLUME_NAME 128u') {
    throw 'C protocol version or volume bound changed without updating the lab contract'
}
Assert-Equal ([FswLab.Port]::MessageSize) 8480 'v4 message size'
foreach ($field in @(@('Generation', 16), @('VolumeName', 28), @('Distributions', 284))) {
    $offset = [Runtime.InteropServices.Marshal]::OffsetOf([FswLab.MappingMessage], $field[0]).ToInt64()
    Assert-Equal $offset $field[1] ($field[0] + ' offset')
}

$volume = [FswLab.Port]::NativeCVolume() # Read-only QueryDosDevice; no port is opened.
$mapping = [byte[]](New-MappingBuffer -Generation 42 -FirstName 'Ubuntu' -VolumeName $volume)
Assert-Equal ([BitConverter]::ToUInt32($mapping, 0)) 4 'mapping version'
Assert-Equal ([BitConverter]::ToUInt32($mapping, 4)) 8480 'declared mapping size'
Assert-Equal ([BitConverter]::ToUInt64($mapping, 16)) 42 'mapping generation'
Assert-Equal ([BitConverter]::ToUInt32($mapping, 24)) 1 'distribution count'
Assert-Equal ([Text.Encoding]::Unicode.GetString($mapping, 28, 256).TrimEnd([char]0)) $volume 'native volume field'
Assert-Equal ([Text.Encoding]::Unicode.GetString($mapping, 284, 256).TrimEnd([char]0)) 'Ubuntu' 'distribution field'
foreach ($operation in @(2, 3)) {
    $packet = [byte[]](New-MappingBuffer -Operation $operation -Count 0 -FirstName '' -VolumeName '')
    Assert-Equal ([BitConverter]::ToUInt32($packet, 8)) $operation 'clear/ping operation'
    Assert-Equal ([BitConverter]::ToUInt32($packet, 24)) 0 'clear/ping count'
    Assert-Equal ([Text.Encoding]::Unicode.GetString($packet, 28, 256).TrimEnd([char]0)) '' 'clear/ping volume'
}
Write-Host 'PASS: driver protocol v4 layout, mapping builder, generation encoding, Ping and Clear packets; no driver loaded or contacted.'
