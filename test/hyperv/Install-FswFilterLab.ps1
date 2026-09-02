[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateScript({ Test-Path -LiteralPath $_ -PathType Leaf })]
    [string]$InfPath,
    [string]$SignToolPath
)

$ErrorActionPreference = 'Stop'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this script in an elevated PowerShell session inside the disposable driver VM.'
}
$model = (Get-CimInstance Win32_ComputerSystem).Model
if ($model -notmatch 'Virtual Machine') {
    throw 'Lab driver installation is VM-only; there is no physical-host override.'
}

$resolvedInf = [IO.Path]::GetFullPath($InfPath)
$package = Split-Path -Parent $resolvedInf
$catalog = Join-Path $package 'fswfilter.cat'
if (-not (Test-Path -LiteralPath $catalog)) {
    throw "The driver catalog is missing: $catalog"
}

if (-not $SignToolPath) {
    $SignToolPath = Get-ChildItem (Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin') -Recurse -Filter signtool.exe -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match '\\(arm64|x64)\\signtool\.exe$' } |
        Sort-Object FullName -Descending |
        Select-Object -First 1 -ExpandProperty FullName
}
if (-not $SignToolPath -or -not (Test-Path -LiteralPath $SignToolPath)) {
    throw 'SignTool was not found. Run Prepare-FswDriverGuest.ps1 -InstallSigningTools, restart, and retry.'
}

$signature = Get-AuthenticodeSignature -LiteralPath $catalog
if ($signature.Status -ne 'Valid') {
    $subject = 'CN=Forward Slash Windows Driver Lab'
    $certificate = Get-ChildItem Cert:\LocalMachine\My |
        Where-Object { $_.Subject -eq $subject -and $_.HasPrivateKey } |
        Sort-Object NotAfter -Descending |
        Select-Object -First 1
    if (-not $certificate) {
        $certificate = New-SelfSignedCertificate -Subject $subject -Type CodeSigningCert `
            -CertStoreLocation Cert:\LocalMachine\My -KeyExportPolicy Exportable `
            -HashAlgorithm SHA256 -NotAfter (Get-Date).AddYears(1)
    }
    foreach ($storeName in 'Root', 'TrustedPublisher') {
        $store = [Security.Cryptography.X509Certificates.X509Store]::new($storeName, 'LocalMachine')
        try {
            $store.Open([Security.Cryptography.X509Certificates.OpenFlags]::ReadWrite)
            $store.Add($certificate)
        } finally {
            $store.Close()
        }
    }
    & $SignToolPath sign /v /fd sha256 /sm /s My /sha1 $certificate.Thumbprint $catalog
    if ($LASTEXITCODE -ne 0) { throw "SignTool exited with $LASTEXITCODE." }
    & $SignToolPath verify /v /pa $catalog
    if ($LASTEXITCODE -ne 0) { throw "Catalog verification exited with $LASTEXITCODE." }
}

pnputil.exe /add-driver $resolvedInf /install
if ($LASTEXITCODE -ne 0) { throw "PnPUtil exited with $LASTEXITCODE" }
fltmc.exe load FswFilter
if ($LASTEXITCODE -ne 0) { throw "FltMC exited with $LASTEXITCODE" }
fltmc.exe filters
