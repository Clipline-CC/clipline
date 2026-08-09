param(
  [string]$ResourceDirectory
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version 2.0

$repoRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
$appRoot = Join-Path $repoRoot "apps\clipline-app"
$manifestPath = Join-Path $appRoot "ffmpeg-runtime.json"
if ([string]::IsNullOrWhiteSpace($ResourceDirectory)) {
  $ResourceDirectory = Join-Path $appRoot "ffmpeg"
}
$resourceRoot = [System.IO.Path]::GetFullPath($ResourceDirectory)

function Assert-ExactText {
  param(
    [AllowNull()][object]$Actual,
    [AllowNull()][object]$Expected,
    [string]$Label
  )

  if ([string]$Actual -cne [string]$Expected) {
    throw "FFmpeg resource $Label mismatch: expected '$Expected', got '$Actual'"
  }
}

function Get-RegularFile {
  param(
    [string]$Path,
    [string]$Label
  )

  $item = Get-Item -LiteralPath $Path -Force -ErrorAction Stop
  if (
    $item.PSIsContainer -or
    (($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0)
  ) {
    throw "FFmpeg resource $Label must be a regular file: $Path"
  }
  return $item
}

function Get-Sha256 {
  param([string]$Path)

  $stream = [System.IO.File]::OpenRead($Path)
  try {
    $sha256 = [System.Security.Cryptography.SHA256]::Create()
    try {
      $hash = $sha256.ComputeHash($stream)
    } finally {
      $sha256.Dispose()
    }
  } finally {
    $stream.Dispose()
  }
  return ([System.BitConverter]::ToString($hash)).Replace("-", "").ToLowerInvariant()
}

$resourceInfo = Get-Item -LiteralPath $resourceRoot -Force -ErrorAction Stop
if (
  -not $resourceInfo.PSIsContainer -or
  (($resourceInfo.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0)
) {
  throw "FFmpeg resource root must be a regular directory: $resourceRoot"
}

$manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
if ([int]$manifest.schema_version -ne 1) {
  throw "FFmpeg resource manifest schema_version must be 1"
}
$manifestFiles = @($manifest.allowed_files)
if ($manifestFiles.Count -eq 0) {
  throw "FFmpeg resource manifest has no allowed_files"
}

$seenNames = @{
  "README.md" = $true
  "PROVENANCE.json" = $true
}
$expectedNames = @("README.md", "PROVENANCE.json")
foreach ($file in $manifestFiles) {
  $name = [string]$file.staged_name
  if (
    [string]::IsNullOrWhiteSpace($name) -or
    $name -cne [System.IO.Path]::GetFileName($name) -or
    [System.IO.Path]::IsPathRooted($name)
  ) {
    throw "Unsafe staged FFmpeg resource name: '$name'"
  }
  if ($seenNames.ContainsKey($name)) {
    throw "Duplicate staged FFmpeg resource name: '$name'"
  }
  $seenNames[$name] = $true
  $expectedNames += $name
}

$resourceEntries = @(Get-ChildItem -LiteralPath $resourceRoot -Force)
$actualNames = @($resourceEntries | ForEach-Object { $_.Name })
$unexpected = @($actualNames | Where-Object { $expectedNames -cnotcontains $_ })
$missing = @($expectedNames | Where-Object { $actualNames -cnotcontains $_ })
if ($unexpected.Count -ne 0 -or $missing.Count -ne 0) {
  throw (
    "Unexpected FFmpeg resource entries. Missing: [{0}]. Unexpected: [{1}]." -f
    ($missing -join ", "),
    ($unexpected -join ", ")
  )
}
foreach ($entry in $resourceEntries) {
  Get-RegularFile -Path $entry.FullName -Label $entry.Name | Out-Null
}

foreach ($file in $manifestFiles) {
  $name = [string]$file.staged_name
  $path = Join-Path $resourceRoot $name
  $item = Get-RegularFile -Path $path -Label $name
  $expectedSize = [int64]$file.size
  if ($item.Length -ne $expectedSize) {
    throw "FFmpeg resource $name size mismatch: expected $expectedSize, got $($item.Length)"
  }
  $actualHash = Get-Sha256 -Path $path
  $expectedHash = ([string]$file.sha256).ToLowerInvariant()
  if ($actualHash -cne $expectedHash) {
    throw "FFmpeg resource $name SHA-256 mismatch: expected $expectedHash, got $actualHash"
  }
}

$provenancePath = Join-Path $resourceRoot "PROVENANCE.json"
$provenance = Get-Content -LiteralPath $provenancePath -Raw | ConvertFrom-Json
if ([int]$provenance.schema_version -ne 1) {
  throw "FFmpeg resource provenance schema_version must be 1"
}
foreach ($field in @(
  "provider",
  "release_tag",
  "published_at",
  "archive_name",
  "archive_url",
  "archive_sha256",
  "source_offer_url",
  "ffmpeg_source_url"
)) {
  Assert-ExactText $provenance.$field $manifest.$field "provenance $field"
}

$manifestHash = Get-Sha256 -Path $manifestPath
Assert-ExactText $provenance.manifest_sha256 $manifestHash "provenance manifest_sha256"
Assert-ExactText $provenance.ffmpeg_version $manifest.version_line "provenance ffmpeg_version"

$provenanceFiles = @($provenance.files)
if ($provenanceFiles.Count -ne $manifestFiles.Count) {
  throw (
    "FFmpeg resource provenance file count mismatch: expected {0}, got {1}" -f
    $manifestFiles.Count,
    $provenanceFiles.Count
  )
}
for ($index = 0; $index -lt $manifestFiles.Count; $index++) {
  $expected = $manifestFiles[$index]
  $actual = $provenanceFiles[$index]
  Assert-ExactText $actual.name $expected.staged_name "provenance files[$index].name"
  if ([int64]$actual.size -ne [int64]$expected.size) {
    throw "FFmpeg resource provenance files[$index].size mismatch"
  }
  Assert-ExactText (
    ([string]$actual.sha256).ToLowerInvariant()
  ) (
    ([string]$expected.sha256).ToLowerInvariant()
  ) "provenance files[$index].sha256"
}

$ffmpegExe = Join-Path $resourceRoot "ffmpeg.exe"
$versionLines = @(& $ffmpegExe -version 2>&1 | ForEach-Object { $_.ToString() })
if ($LASTEXITCODE -ne 0) {
  throw "Verified FFmpeg resource failed its version probe with exit code $LASTEXITCODE"
}
if ($versionLines.Count -eq 0) {
  throw "Verified FFmpeg resource returned no version output"
}
Assert-ExactText $versionLines[0] $manifest.version_line "version line"

$configurationLines = @($versionLines | Where-Object { $_.StartsWith("configuration:") })
if ($configurationLines.Count -ne 1) {
  throw "Verified FFmpeg resource must report exactly one configuration line"
}
$configuration = $configurationLines[0]
Assert-ExactText $configuration $provenance.configuration "provenance configuration"
foreach ($required in @($manifest.required_configuration)) {
  if (-not $configuration.Contains([string]$required)) {
    throw "FFmpeg resource configuration is missing required flag $required"
  }
}
foreach ($forbidden in @($manifest.forbidden_configuration)) {
  if ($configuration.Contains([string]$forbidden)) {
    throw "FFmpeg resource configuration contains forbidden flag $forbidden"
  }
}

Write-Host "Verified staged FFmpeg resource at $resourceRoot"
