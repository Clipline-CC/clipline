# Stage an app-local x64 probe package; does not install a global runtime.
param(
    [Parameter(Mandatory = $true)][string]$ProbePath,
    [Parameter(Mandatory = $true)][string]$RuntimePath,
    [Parameter(Mandatory = $true)][string]$RuntimeSource,
    [Parameter(Mandatory = $true)][string]$Destination
)
$ErrorActionPreference = 'Stop'
$ProbePath = (Resolve-Path -LiteralPath $ProbePath).Path
$RuntimePath = (Resolve-Path -LiteralPath $RuntimePath).Path
$Destination = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Destination)
if (Test-Path -LiteralPath $Destination) { throw 'Destination must be new.' }
$Destination = $Destination.TrimEnd([char[]]'\/')
$preflightDirectory = $Destination + '.preflight'
if (Test-Path -LiteralPath $preflightDirectory) { throw 'Preflight output directory must be new.' }

function Assert-X64Pe([string]$Path) {
    $bytes = [IO.File]::ReadAllBytes($Path)
    if ($bytes.Length -lt 64 -or [BitConverter]::ToUInt16($bytes, 0) -ne 0x5A4D) { throw "Not a PE file: $Path" }
    $pe = [BitConverter]::ToInt32($bytes, 0x3c)
    if ($pe -lt 0 -or $pe -gt $bytes.Length - 6 -or
        [BitConverter]::ToUInt32($bytes, $pe) -ne 0x4550 -or
        [BitConverter]::ToUInt16($bytes, $pe + 4) -ne 0x8664) { throw "Not an x64 PE file: $Path" }
}
Assert-X64Pe $ProbePath
Assert-X64Pe $RuntimePath
$signature = Get-AuthenticodeSignature -LiteralPath $RuntimePath
if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch 'O=Microsoft Corporation(?:,|$)') {
    throw 'Runtime must have a valid Microsoft signature.'
}
$version = (Get-Item -LiteralPath $RuntimePath).VersionInfo
if ($version.OriginalFilename -ine 'vcruntime140.dll') { throw 'Expected the redistributable vcruntime140.dll.' }
$null = New-Item -ItemType Directory -Path $Destination
$Destination = (Resolve-Path -LiteralPath $Destination).Path
Copy-Item -LiteralPath $ProbePath -Destination (Join-Path $Destination 'dwm_probe.exe')
Copy-Item -LiteralPath $RuntimePath -Destination (Join-Path $Destination 'vcruntime140.dll')
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'test-dwm-probe.ps1'), (Join-Path $PSScriptRoot 'dwm-probe-target.ps1') -Destination $Destination
Copy-Item -LiteralPath (Join-Path $PSScriptRoot '..\docs\dwm-probe.md') -Destination (Join-Path $Destination 'README.md')
[ordered]@{
    utc = [DateTime]::UtcNow.ToString('o')
    executable_sha256 = (Get-FileHash -LiteralPath $ProbePath).Hash
    runtime_source = $RuntimeSource
    runtime_version = $version.FileVersion
    runtime_sha256 = (Get-FileHash -LiteralPath $RuntimePath).Hash
    runtime_signer = $signature.SignerCertificate.Subject
} | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $Destination 'package.json')
# Window enumeration can include private titles. Keep these logs OUTSIDE the package.
& (Join-Path $Destination 'test-dwm-probe.ps1') -PreflightOnly -OutputDirectory $preflightDirectory
Write-Output "Package staged and loader checks passed: $Destination"
