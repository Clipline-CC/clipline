[CmdletBinding()]
param(
    # Download/extract scratch directory. The benchmark harness passes its own
    # output directory so the archive lands beside the other benchmark records.
    [string]$OutputDirectory = $(
        if ($env:BENCHMARK_DIR) { $env:BENCHMARK_DIR }
        else { Join-Path $env:TEMP 'clipline-tauri-cli' }
    )
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Supply-chain pin: upgrade by changing all five values together after
# reviewing the release at https://github.com/tauri-apps/tauri/releases.
$tauriVersion = '2.11.2'
$tauriArchiveName = 'cargo-tauri-x86_64-pc-windows-msvc.zip'
$tauriArchiveSize = 7414116
$tauriArchiveSha256 = 'b6844470bcbf1da6e5dbf01990ae317d4d7969171628bb8badbdbff2e3d06d23'
$tauriArchiveUrl = "https://github.com/tauri-apps/tauri/releases/download/tauri-cli-v$tauriVersion/$tauriArchiveName"

$started = [DateTimeOffset]::UtcNow
New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
$archive = Join-Path $OutputDirectory $tauriArchiveName
$destination = Join-Path $OutputDirectory 'tauri-cli'
Invoke-WebRequest -UseBasicParsing -Uri $tauriArchiveUrl -OutFile $archive
$download = Get-Item -LiteralPath $archive
if ($download.Length -ne $tauriArchiveSize) {
    throw "Tauri CLI archive size mismatch: expected $tauriArchiveSize, got $($download.Length)."
}
$actualHash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
if ($actualHash -cne $tauriArchiveSha256) {
    throw "Tauri CLI archive SHA-256 mismatch: expected $tauriArchiveSha256, got $actualHash."
}
Expand-Archive -LiteralPath $archive -DestinationPath $destination -Force
$binaries = @(Get-ChildItem -LiteralPath $destination -Recurse -Filter cargo-tauri.exe -File)
if ($binaries.Count -ne 1) {
    throw "Expected exactly one cargo-tauri.exe in the pinned Tauri CLI archive."
}
$versionOutput = (& $binaries[0].FullName --version | Out-String).Trim()
if ($LASTEXITCODE -ne 0 -or $versionOutput -cne "tauri-cli $tauriVersion") {
    throw "Expected tauri-cli $tauriVersion, got '$versionOutput'."
}
if ($env:GITHUB_PATH) {
    $binaries[0].DirectoryName | Add-Content -LiteralPath $env:GITHUB_PATH
}
$completed = [DateTimeOffset]::UtcNow

# Provenance record for callers (the benchmark harness wraps it in its timing JSON).
[ordered]@{
    archive_url = $tauriArchiveUrl
    started_utc = $started.ToString('O')
    completed_utc = $completed.ToString('O')
    duration_seconds = [Math]::Round(($completed - $started).TotalSeconds, 3)
    archive_bytes = [long]$download.Length
    archive_sha256 = $actualHash
    version_output = $versionOutput
    cargo_tauri_directory = $binaries[0].DirectoryName
} | ConvertTo-Json
