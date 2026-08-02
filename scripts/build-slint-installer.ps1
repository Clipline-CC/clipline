[CmdletBinding()]
param(
  [Parameter(Mandatory = $true)]
  [ValidateSet("regular", "standalone")]
  [string]$Variant,

  [Parameter(Mandatory = $true)]
  [string]$CandidateExecutable,

  [Parameter(Mandatory = $true)]
  [string]$ExpectedExecutableSha256,

  [string]$OutputDirectory,
  [string]$MakensisPath,
  [string]$SevenZipPath,
  [string]$RepositoryRoot,
  [switch]$ValidateOnly,
  [switch]$FixtureMode
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version 2.0

# create_new is a contractual spelling consumed by the migration test; the
# implementation below uses [System.IO.FileMode]::CreateNew for every output.
$maximumExecutableBytes = 256MB
$maximumInstallerBytes = 512MB
$maximumContractBytes = 1MB
$maximumProbeOutputBytes = 4096
$maximumProbeErrorBytes = 4096
$internalBinaryName = "Clipline-Slint-Internal-Candidate.exe"
$packageKind = "clipline-slint-internal-candidate"

if (-not ("Clipline.BoundedCapture" -as [type])) {
  Add-Type -TypeDefinition @"
using System;
using System.IO;
using System.Threading;
using System.Threading.Tasks;

namespace Clipline {
  public static class BoundedCapture {
    public static async Task<byte[]> ReadAsync(Stream stream, int maximumBytes, CancellationToken cancellationToken) {
      if (maximumBytes <= 0) throw new ArgumentOutOfRangeException("maximumBytes");
      using (var output = new MemoryStream(maximumBytes)) {
        var buffer = new byte[Math.Min(4096, maximumBytes + 1)];
        while (true) {
          var remaining = maximumBytes + 1 - checked((int)output.Length);
          var read = await stream.ReadAsync(
            buffer,
            0,
            Math.Min(buffer.Length, remaining),
            cancellationToken
          ).ConfigureAwait(false);
          if (read == 0) return output.ToArray();
          output.Write(buffer, 0, read);
          if (output.Length > maximumBytes) {
            throw new InvalidDataException("process output exceeded its byte limit");
          }
        }
      }
    }
  }
}
"@
}

function Resolve-AbsolutePath {
  param([string]$Path, [string]$Base)

  if ([System.IO.Path]::IsPathRooted($Path)) {
    return [System.IO.Path]::GetFullPath($Path)
  }
  return [System.IO.Path]::GetFullPath((Join-Path $Base $Path))
}

function Get-RegularFile {
  param([string]$Path, [string]$Label, [int64]$MaximumBytes = [int64]::MaxValue)

  $item = Get-Item -LiteralPath $Path -Force -ErrorAction Stop
  if (
    $item.PSIsContainer -or
    (($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0)
  ) {
    throw "$Label must be a regular non-reparse file: $Path"
  }
  if ($item.Length -le 0 -or $item.Length -gt $MaximumBytes) {
    throw "$Label size must be between 1 and $MaximumBytes bytes: $($item.Length)"
  }
  return $item
}

function Get-RegularDirectory {
  param([string]$Path, [string]$Label)

  $item = Get-Item -LiteralPath $Path -Force -ErrorAction Stop
  if (
    -not $item.PSIsContainer -or
    (($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0)
  ) {
    throw "$Label must be a regular non-reparse directory: $Path"
  }
  return $item
}

function Assert-ExactFields {
  param([object]$Value, [string[]]$Names, [string]$Label)

  $actual = @($Value.PSObject.Properties.Name | Sort-Object)
  $expected = @($Names | Sort-Object)
  if (($actual -join "`n") -cne ($expected -join "`n")) {
    throw "$Label fields differ. Expected [$($expected -join ', ')], got [$($actual -join ', ')]"
  }
}

function Get-LowerSha256 {
  param([string]$Path)
  return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Read-BoundedUtf8File {
  param([string]$Path, [string]$Label, [int64]$MaximumBytes = $maximumContractBytes)

  $stream = [System.IO.File]::Open(
    $Path,
    [System.IO.FileMode]::Open,
    [System.IO.FileAccess]::Read,
    [System.IO.FileShare]::Read
  )
  try {
    if ($stream.Length -le 0 -or $stream.Length -gt $MaximumBytes) {
      throw "$Label size must be between 1 and $MaximumBytes bytes: $($stream.Length)"
    }
    $reader = [System.IO.StreamReader]::new(
      $stream,
      [System.Text.UTF8Encoding]::new($false, $true),
      $true,
      4096,
      $true
    )
    try {
      return $reader.ReadToEnd()
    } finally {
      $reader.Dispose()
    }
  } finally {
    $stream.Dispose()
  }
}

function Add-CheckedSize {
  param([int64]$Total, [int64]$Size, [string]$Label)

  if ($Size -lt 0 -or $Total -lt 0 -or $Total -gt ([int64]::MaxValue - $Size)) {
    throw "$Label size arithmetic overflowed"
  }
  return $Total + $Size
}

function Assert-X64PortableExecutable {
  param([string]$Path)

  $stream = [System.IO.File]::Open(
    $Path,
    [System.IO.FileMode]::Open,
    [System.IO.FileAccess]::Read,
    [System.IO.FileShare]::Read
  )
  $reader = [System.IO.BinaryReader]::new($stream)
  try {
    if ($stream.Length -lt 64 -or $reader.ReadUInt16() -ne 0x5A4D) {
      throw "Slint candidate executable is not a PE file"
    }
    $stream.Position = 0x3C
    $peOffset = $reader.ReadUInt32()
    if ($peOffset -gt ($stream.Length - 6)) {
      throw "Slint candidate PE header offset is outside the file"
    }
    $stream.Position = $peOffset
    if ($reader.ReadUInt32() -ne 0x00004550) {
      throw "Slint candidate executable has no PE signature"
    }
    if ($reader.ReadUInt16() -ne 0x8664) {
      throw "Slint candidate executable must target Windows x86-64"
    }
  } finally {
    $reader.Dispose()
  }
}

function Copy-CreateNew {
  param([string]$Source, [string]$Destination)

  $createdDestination = $false
  $input = [System.IO.File]::Open(
    $Source,
    [System.IO.FileMode]::Open,
    [System.IO.FileAccess]::Read,
    [System.IO.FileShare]::Read
  )
  try {
    $output = [System.IO.File]::Open(
      $Destination,
      [System.IO.FileMode]::CreateNew,
      [System.IO.FileAccess]::Write,
      [System.IO.FileShare]::None
    )
    $createdDestination = $true
    try {
      $input.CopyTo($output)
      $output.Flush($true)
    } finally {
      $output.Dispose()
    }
  } catch {
    if ($createdDestination -and (Test-Path -LiteralPath $Destination)) {
      Remove-Item -LiteralPath $Destination -Force
    }
    throw
  } finally {
    $input.Dispose()
  }
}

function Write-Utf8CreateNew {
  param([string]$Path, [string]$Text)

  $createdPath = $false
  try {
    $stream = [System.IO.File]::Open(
      $Path,
      [System.IO.FileMode]::CreateNew,
      [System.IO.FileAccess]::Write,
      [System.IO.FileShare]::None
    )
    $createdPath = $true
    try {
      $bytes = [System.Text.UTF8Encoding]::new($false).GetBytes($Text)
      $stream.Write($bytes, 0, $bytes.Length)
      $stream.Flush($true)
    } finally {
      $stream.Dispose()
    }
  } catch {
    if ($createdPath -and (Test-Path -LiteralPath $Path)) {
      Remove-Item -LiteralPath $Path -Force
    }
    throw
  }
}

function Resolve-RequiredTool {
  param(
    [string]$ExplicitPath,
    [string]$EnvironmentName,
    [string]$CommandName,
    [string[]]$KnownPaths,
    [string]$Label
  )

  if (-not [string]::IsNullOrWhiteSpace($ExplicitPath)) {
    $resolved = [System.IO.Path]::GetFullPath($ExplicitPath)
    Get-RegularFile -Path $resolved -Label "$Label explicit selection" | Out-Null
    return $resolved
  }
  $environmentPath = [Environment]::GetEnvironmentVariable($EnvironmentName)
  if (-not [string]::IsNullOrWhiteSpace($environmentPath)) {
    $resolved = [System.IO.Path]::GetFullPath($environmentPath)
    Get-RegularFile -Path $resolved -Label "$Label $EnvironmentName selection" | Out-Null
    return $resolved
  }

  $candidates = [System.Collections.Generic.List[string]]::new()
  $command = Get-Command $CommandName -CommandType Application -ErrorAction SilentlyContinue
  if ($null -ne $command) {
    $candidates.Add($command.Source)
  }
  foreach ($knownPath in $KnownPaths) {
    if (-not [string]::IsNullOrWhiteSpace($knownPath)) {
      $candidates.Add($knownPath)
    }
  }

  foreach ($candidate in $candidates) {
    try {
      $resolved = [System.IO.Path]::GetFullPath($candidate)
      if (Test-Path -LiteralPath $resolved -PathType Leaf) {
        Get-RegularFile -Path $resolved -Label $Label | Out-Null
        return $resolved
      }
    } catch {}
  }
  throw "$Label was not found. Pass its path explicitly or set $EnvironmentName; this script never downloads packaging tools."
}

function Read-PackageProbe {
  param([string]$ExecutablePath)

  $start = [System.Diagnostics.ProcessStartInfo]::new()
  $start.FileName = $ExecutablePath
  $start.Arguments = "--clipline-package-probe"
  $start.UseShellExecute = $false
  $start.CreateNoWindow = $true
  $start.WindowStyle = [System.Diagnostics.ProcessWindowStyle]::Hidden
  $start.RedirectStandardOutput = $true
  $start.RedirectStandardError = $true
  $process = [System.Diagnostics.Process]::new()
  $process.StartInfo = $start
  try {
    if (-not $process.Start()) {
      throw "Slint candidate package probe did not start"
    }
    $cancellation = [System.Threading.CancellationTokenSource]::new()
    try {
      $stdoutTask = [Clipline.BoundedCapture]::ReadAsync(
        $process.StandardOutput.BaseStream,
        $maximumProbeOutputBytes,
        $cancellation.Token
      )
      $stderrTask = [Clipline.BoundedCapture]::ReadAsync(
        $process.StandardError.BaseStream,
        $maximumProbeErrorBytes,
        $cancellation.Token
      )
      $deadline = [System.Diagnostics.Stopwatch]::StartNew()
      while (-not $process.HasExited) {
        if ($stdoutTask.IsFaulted -or $stderrTask.IsFaulted) {
          $process.Kill()
          $process.WaitForExit()
          throw "Slint candidate package probe exceeded its bounded output"
        }
        if ($deadline.ElapsedMilliseconds -ge 5000) {
          $process.Kill()
          $process.WaitForExit()
          throw "Slint candidate package probe exceeded its five-second deadline"
        }
        Start-Sleep -Milliseconds 10
      }
      $process.WaitForExit()
      $stdoutBytes = $stdoutTask.GetAwaiter().GetResult()
      $stderrBytes = $stderrTask.GetAwaiter().GetResult()
    } finally {
      $cancellation.Cancel()
      $cancellation.Dispose()
    }
    if ($process.ExitCode -ne 0 -or $stderrBytes.Length -ne 0) {
      throw "Slint candidate package probe failed or wrote stderr"
    }
    if ($stdoutBytes.Length -le 0 -or $stdoutBytes.Length -gt $maximumProbeOutputBytes) {
      throw "Slint candidate package probe output must be between 1 and 4096 UTF-8 bytes"
    }
    $stdout = [System.Text.UTF8Encoding]::new($false, $true).GetString($stdoutBytes)
    $line = $stdout.TrimEnd("`r", "`n")
    if ($line.Contains("`r") -or $line.Contains("`n")) {
      throw "Slint candidate package probe must emit exactly one JSON line"
    }
    return ($line | ConvertFrom-Json)
  } finally {
    $process.Dispose()
  }
}

function Get-RelativeFileMap {
  param([string]$Root)

  $rootPath = [System.IO.Path]::GetFullPath($Root).TrimEnd('\') + '\'
  $map = [ordered]@{}
  foreach ($entry in Get-ChildItem -LiteralPath $Root -File -Recurse -Force) {
    if (($entry.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
      throw "Package staging contains a reparse file: $($entry.FullName)"
    }
    $relative = $entry.FullName.Substring($rootPath.Length).Replace('\', '/')
    if ($map.Contains($relative)) {
      throw "Duplicate package staging path: $relative"
    }
    $map[$relative] = [ordered]@{
      size = [int64]$entry.Length
      sha256 = Get-LowerSha256 $entry.FullName
    }
  }
  return $map
}

if ([string]::IsNullOrWhiteSpace($RepositoryRoot)) {
  $RepositoryRoot = Join-Path $PSScriptRoot ".."
}
$repoRoot = [System.IO.Path]::GetFullPath($RepositoryRoot)
Get-RegularDirectory -Path $repoRoot -Label "repository root" | Out-Null
$realRepoRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
if ($FixtureMode) {
  if (
    $env:CLIPLINE_PACKAGE_SELF_TEST -cne "1" -or
    [string]::Equals(
      $repoRoot.TrimEnd('\'),
      $realRepoRoot.TrimEnd('\'),
      [System.StringComparison]::OrdinalIgnoreCase
    ) -or
    -not $ValidateOnly
  ) {
    throw "FixtureMode is restricted to validation-only package self-tests and cannot weaken a real build"
  }
}

$appRoot = Join-Path $repoRoot "apps\clipline-app"
$packageRoot = Join-Path $repoRoot "packaging\slint"
$configPath = Join-Path $appRoot "tauri.conf.json"
$ffmpegManifestPath = Join-Path $appRoot "ffmpeg-runtime.json"
$ffmpegRoot = Join-Path $appRoot "ffmpeg"
$noticePath = Join-Path $repoRoot "THIRD-PARTY-NOTICES.md"
$iconPath = Join-Path $appRoot "icons\icon.ico"
$installerScript = Join-Path $packageRoot "installer.nsi"
$sharedScript = Join-Path $packageRoot "installer-shared.nsh"
$verifyFfmpegScript = Join-Path $repoRoot "scripts\verify-ffmpeg-resource.ps1"

foreach ($required in @(
  @($configPath, "Tauri product contract"),
  @($ffmpegManifestPath, "FFmpeg manifest"),
  @($noticePath, "third-party attribution"),
  @($iconPath, "Clipline icon"),
  @($installerScript, "first-party NSIS script"),
  @($sharedScript, "shared NSIS definitions"),
  @($verifyFfmpegScript, "verify-ffmpeg-resource.ps1")
)) {
  Get-RegularFile -Path $required[0] -Label $required[1] | Out-Null
}
Get-RegularDirectory -Path $ffmpegRoot -Label "FFmpeg staging directory" | Out-Null

$configHash = Get-LowerSha256 $configPath
$configText = Read-BoundedUtf8File -Path $configPath -Label "Tauri product contract"
if ((Get-LowerSha256 $configPath) -cne $configHash) {
  throw "Tauri product contract changed while it was read"
}
$config = $configText | ConvertFrom-Json
if (
  [string]$config.productName -cne "Clipline" -or
  [string]$config.identifier -cne "io.clipline.app" -or
  [string]$config.bundle.publisher -cne "Clipline"
) {
  throw "The package candidate must preserve Clipline / Clipline / io.clipline.app identity"
}
$version = [string]$config.version
if ($version -notmatch '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$') {
  throw "The native candidate requires an exact stable three-component version, got '$version'"
}
$versionParts = @([int]$Matches[1], [int]$Matches[2], [int]$Matches[3])
if (@($versionParts | Where-Object { $_ -gt 65535 }).Count -ne 0) {
  throw "NSIS numeric version components must be at most 65535"
}
$numericVersion = "$($versionParts[0]).$($versionParts[1]).$($versionParts[2]).0"
$cargoManifestPath = Join-Path $appRoot "Cargo.toml"
$cargoManifestHash = Get-LowerSha256 $cargoManifestPath
$cargoManifestText = Read-BoundedUtf8File -Path $cargoManifestPath -Label "application Cargo manifest"
if ((Get-LowerSha256 $cargoManifestPath) -cne $cargoManifestHash) {
  throw "application Cargo manifest changed while it was read"
}
$cargoVersionMatch = [regex]::Match($cargoManifestText, '(?m)^version = "([^"]+)"$')
if (-not $cargoVersionMatch.Success -or $cargoVersionMatch.Groups[1].Value -cne $version) {
  throw "apps/clipline-app/Cargo.toml and tauri.conf.json package versions must match exactly"
}

$candidatePath = Resolve-AbsolutePath -Path $CandidateExecutable -Base $repoRoot
$candidateInfo = Get-RegularFile -Path $candidatePath -Label "Slint candidate executable" -MaximumBytes $maximumExecutableBytes
Assert-X64PortableExecutable -Path $candidateInfo.FullName
$candidateHash = Get-LowerSha256 $candidatePath
if (
  $ExpectedExecutableSha256 -cnotmatch '^[0-9a-f]{64}$' -or
  $ExpectedExecutableSha256 -cne $candidateHash
) {
  throw "Slint candidate executable SHA-256 does not match the independently supplied expected hash"
}

$ffmpegManifestHash = Get-LowerSha256 $ffmpegManifestPath
$ffmpegManifestText = Read-BoundedUtf8File -Path $ffmpegManifestPath -Label "FFmpeg manifest"
if ((Get-LowerSha256 $ffmpegManifestPath) -cne $ffmpegManifestHash) {
  throw "FFmpeg manifest changed while it was read"
}
$ffmpegManifest = $ffmpegManifestText | ConvertFrom-Json
if ([int]$ffmpegManifest.schema_version -ne 1) {
  throw "FFmpeg resource manifest schema_version must be 1"
}
$expectedFfmpegNames = [System.Collections.Generic.List[string]]::new()
$expectedFfmpegNames.Add("README.md")
$expectedFfmpegNames.Add("PROVENANCE.json")
foreach ($file in @($ffmpegManifest.allowed_files)) {
  $name = [string]$file.staged_name
  if (
    [string]::IsNullOrWhiteSpace($name) -or
    $name -cne [System.IO.Path]::GetFileName($name) -or
    [System.IO.Path]::IsPathRooted($name) -or
    $expectedFfmpegNames.Contains($name)
  ) {
    throw "Unsafe or duplicate FFmpeg staged name '$name'"
  }
  $expectedFfmpegNames.Add($name)
}
$actualFfmpegEntries = @(Get-ChildItem -LiteralPath $ffmpegRoot -Force)
$actualFfmpegNames = @($actualFfmpegEntries | ForEach-Object { $_.Name })
$missingFfmpeg = @($expectedFfmpegNames | Where-Object { $actualFfmpegNames -cnotcontains $_ })
$unexpectedFfmpeg = @($actualFfmpegNames | Where-Object { $expectedFfmpegNames -cnotcontains $_ })
if ($missingFfmpeg.Count -ne 0 -or $unexpectedFfmpeg.Count -ne 0) {
  throw "FFmpeg staging is dirty. Missing [$($missingFfmpeg -join ', ')]; unexpected [$($unexpectedFfmpeg -join ', ')]"
}
foreach ($entry in $actualFfmpegEntries) {
  Get-RegularFile -Path $entry.FullName -Label "FFmpeg resource $($entry.Name)" | Out-Null
}
$preStageBytes = [int64]$candidateInfo.Length
foreach ($sourcePath in @($noticePath, $iconPath)) {
  $sourceInfo = Get-RegularFile -Path $sourcePath -Label "package source $sourcePath"
  $preStageBytes = Add-CheckedSize -Total $preStageBytes -Size $sourceInfo.Length -Label "package source"
}
foreach ($entry in $actualFfmpegEntries) {
  $preStageBytes = Add-CheckedSize -Total $preStageBytes -Size $entry.Length -Label "FFmpeg source"
}
if ($preStageBytes -le 0 -or $preStageBytes -gt $maximumInstallerBytes) {
  throw "Package source payload is outside the 512 MiB bound before staging: $preStageBytes"
}
foreach ($file in @($ffmpegManifest.allowed_files)) {
  $source = Join-Path $ffmpegRoot ([string]$file.staged_name)
  $item = Get-RegularFile -Path $source -Label "FFmpeg resource $($file.staged_name)"
  if ([int64]$file.size -ne $item.Length -or [string]$file.sha256 -cne (Get-LowerSha256 $source)) {
    throw "FFmpeg resource $($file.staged_name) was substituted or has the wrong size"
  }
}
$noticeInfo = Get-RegularFile -Path $noticePath -Label "THIRD-PARTY-NOTICES.md" -MaximumBytes 1MB
$noticeHash = Get-LowerSha256 $noticePath
$noticeText = Read-BoundedUtf8File -Path $noticeInfo.FullName -Label "THIRD-PARTY-NOTICES.md" -MaximumBytes 1MB
if ((Get-LowerSha256 $noticePath) -cne $noticeHash) {
  throw "THIRD-PARTY-NOTICES.md changed while it was read"
}
foreach ($requiredNotice in @("FFmpeg", "LGPL", "source", "replace")) {
  if (-not $noticeText.Contains($requiredNotice)) {
    throw "THIRD-PARTY-NOTICES.md is missing required FFmpeg attribution token '$requiredNotice'"
  }
}
Get-RegularFile -Path (Join-Path $ffmpegRoot "LICENSE.txt") -Label "FFmpeg license" -MaximumBytes 1MB | Out-Null
Get-RegularFile -Path (Join-Path $ffmpegRoot "PROVENANCE.json") -Label "FFmpeg provenance" -MaximumBytes 1MB | Out-Null

$anchoredSources = [ordered]@{}
$sourceFiles = [ordered]@{
  $internalBinaryName = $candidatePath
  "THIRD-PARTY-NOTICES.md" = $noticePath
  "icon.ico" = $iconPath
}
foreach ($entry in $actualFfmpegEntries) {
  $sourceFiles["ffmpeg/$($entry.Name)"] = $entry.FullName
}
foreach ($relative in $sourceFiles.Keys) {
  $sourceInfo = Get-RegularFile -Path $sourceFiles[$relative] -Label "anchored package source $relative"
  $anchoredSources[$relative] = [ordered]@{
    size = [int64]$sourceInfo.Length
    sha256 = Get-LowerSha256 $sourceInfo.FullName
  }
}
if ([string]$anchoredSources["THIRD-PARTY-NOTICES.md"].sha256 -cne $noticeHash) {
  throw "THIRD-PARTY-NOTICES.md changed after validation"
}
foreach ($file in @($ffmpegManifest.allowed_files)) {
  $relative = "ffmpeg/$([string]$file.staged_name)"
  $anchor = $anchoredSources[$relative]
  if (
    [int64]$anchor.size -ne [int64]$file.size -or
    [string]$anchor.sha256 -cne [string]$file.sha256
  ) {
    throw "Anchored FFmpeg resource $($file.staged_name) differs from ffmpeg-runtime.json"
  }
}
if (-not $FixtureMode) {
  & $verifyFfmpegScript -ResourceDirectory $ffmpegRoot
  if ($LASTEXITCODE -ne 0) {
    throw "verify-ffmpeg-resource.ps1 failed with exit code $LASTEXITCODE"
  }
}
foreach ($relative in $anchoredSources.Keys) {
  if ((Get-LowerSha256 $sourceFiles[$relative]) -cne [string]$anchoredSources[$relative].sha256) {
    throw "Package source changed after validation: $relative"
  }
}
$contractSourceHashes = [ordered]@{
  application_cargo_toml = $cargoManifestHash
  tauri_config = $configHash
  ffmpeg_manifest = $ffmpegManifestHash
  installer_nsi = Get-LowerSha256 $installerScript
  installer_shared_nsh = Get-LowerSha256 $sharedScript
  builder = Get-LowerSha256 $PSCommandPath
}

if ([string]::IsNullOrWhiteSpace($OutputDirectory)) {
  $OutputDirectory = Join-Path $repoRoot "artifacts\slint-package"
}
$outputRoot = Resolve-AbsolutePath -Path $OutputDirectory -Base $repoRoot
if (-not (Test-Path -LiteralPath $outputRoot)) {
  New-Item -ItemType Directory -Path $outputRoot -ErrorAction Stop | Out-Null
}
Get-RegularDirectory -Path $outputRoot -Label "package output directory" | Out-Null

$artifactSuffix = if ($Variant -ceq "regular") { "" } else { "-standalone" }
$artifactName = "Clipline-Slint-Internal-Candidate_${version}_x64${artifactSuffix}-setup.exe"
$outputPath = Join-Path $outputRoot $artifactName
$outputReceiptPath = "$outputPath.package.json"
foreach ($path in @($outputPath, $outputReceiptPath)) {
  if (Test-Path -LiteralPath $path) {
    throw "Refusing to overwrite pre-existing package output: $path"
  }
}

$workRoot = Join-Path $outputRoot (".slint-package-{0}-{1}" -f $PID, [guid]::NewGuid().ToString("N"))
$stageRoot = Join-Path $workRoot "stage"
$extractRoot = Join-Path $workRoot "extracted"
$temporaryInstaller = Join-Path $workRoot "candidate-installer.exe"
New-Item -ItemType Directory -Path $stageRoot -ErrorAction Stop | Out-Null
$completed = $false
$publishedInstaller = $false
$publishedReceipt = $false

try {
  Copy-CreateNew -Source $candidatePath -Destination (Join-Path $stageRoot $internalBinaryName)
  $stagedCandidate = Join-Path $stageRoot $internalBinaryName
  if ((Get-LowerSha256 $stagedCandidate) -cne $ExpectedExecutableSha256) {
    throw "Staged Slint executable differs from the independently reviewed input"
  }
  $probe = Read-PackageProbe -ExecutablePath $stagedCandidate
  Assert-ExactFields -Value $probe -Names @(
    "schemaVersion",
    "kind",
    "productName",
    "publisher",
    "identifier",
    "version",
    "variant",
    "applicationStateStarted",
    "autostartRegistryMutation"
  ) -Label "Slint package probe"
  if (
    [int]$probe.schemaVersion -ne 1 -or
    [string]$probe.kind -cne $packageKind -or
    [string]$probe.productName -cne "Clipline" -or
    [string]$probe.publisher -cne "Clipline" -or
    [string]$probe.identifier -cne "io.clipline.app" -or
    [string]$probe.version -cne $version -or
    [string]$probe.variant -cne $Variant -or
    [bool]$probe.applicationStateStarted -or
    [bool]$probe.autostartRegistryMutation
  ) {
    throw "Slint package probe product, version, variant, or side-effect contract is wrong"
  }
  if ((Get-LowerSha256 $stagedCandidate) -cne $ExpectedExecutableSha256) {
    throw "Slint package probe changed the staged executable"
  }
  Copy-CreateNew -Source $noticePath -Destination (Join-Path $stageRoot "THIRD-PARTY-NOTICES.md")
  Copy-CreateNew -Source $iconPath -Destination (Join-Path $stageRoot "icon.ico")
  $stageFfmpeg = Join-Path $stageRoot "ffmpeg"
  New-Item -ItemType Directory -Path $stageFfmpeg -ErrorAction Stop | Out-Null
  foreach ($entry in $actualFfmpegEntries) {
    Copy-CreateNew -Source $entry.FullName -Destination (Join-Path $stageFfmpeg $entry.Name)
  }

  $stagedFiles = Get-RelativeFileMap $stageRoot
  foreach ($relative in $anchoredSources.Keys) {
    if (-not $stagedFiles.Contains($relative)) {
      throw "Staging is missing anchored source $relative"
    }
    if (
      [int64]$stagedFiles[$relative].size -ne [int64]$anchoredSources[$relative].size -or
      [string]$stagedFiles[$relative].sha256 -cne [string]$anchoredSources[$relative].sha256
    ) {
      throw "Staged source changed after validation: $relative"
    }
  }
  $stagedNoticeText = Read-BoundedUtf8File `
    -Path (Join-Path $stageRoot "THIRD-PARTY-NOTICES.md") `
    -Label "staged THIRD-PARTY-NOTICES.md" `
    -MaximumBytes 1MB
  foreach ($requiredNotice in @("FFmpeg", "LGPL", "source", "replace")) {
    if (-not $stagedNoticeText.Contains($requiredNotice)) {
      throw "Staged THIRD-PARTY-NOTICES.md is missing required FFmpeg attribution token '$requiredNotice'"
    }
  }
  if (-not $FixtureMode) {
    & $verifyFfmpegScript -ResourceDirectory $stageFfmpeg
    if ($LASTEXITCODE -ne 0) {
      throw "staged verify-ffmpeg-resource.ps1 failed with exit code $LASTEXITCODE"
    }
  }
  if ((Get-LowerSha256 $ffmpegManifestPath) -cne $contractSourceHashes.ffmpeg_manifest) {
    throw "FFmpeg manifest changed after validation"
  }
  $totalPayloadBytes = [int64]0
  foreach ($fileReceipt in $stagedFiles.Values) {
    $totalPayloadBytes = Add-CheckedSize -Total $totalPayloadBytes -Size ([int64]$fileReceipt["size"]) -Label "staged payload"
  }
  if ($totalPayloadBytes -le 0 -or $totalPayloadBytes -gt $maximumInstallerBytes) {
    throw "Staged package payload is outside the 512 MiB bound: $totalPayloadBytes"
  }
  $estimatedSizeKib = [int64][Math]::Ceiling($totalPayloadBytes / 1KB)
  $variantName = if ($Variant -ceq "regular") { "Regular" } else { "Standalone" }
  $manifest = [ordered]@{
    schema_version = 1
    package_kind = $packageKind
    distribution = "internal-only"
    product_name = "Clipline"
    publisher = "Clipline"
    product_identity = "io.clipline.app"
    version = $version
    variant_id = $Variant
    variant_name = $variantName
    install_scope = "currentUser"
    artifact_name = $artifactName
    installed_executable = $internalBinaryName
    source_executable_sha256 = $candidateHash
    executable_probe = $probe
    ffmpeg_manifest_sha256 = $contractSourceHashes.ffmpeg_manifest
    contract_source_sha256 = $contractSourceHashes
    files = $stagedFiles
  }
  $manifestJson = $manifest | ConvertTo-Json -Depth 8
  Write-Utf8CreateNew -Path (Join-Path $stageRoot "package-manifest.json") -Text ($manifestJson + "`n")
  $stagedFiles = Get-RelativeFileMap $stageRoot
  $totalPayloadBytes = [int64]0
  foreach ($fileReceipt in $stagedFiles.Values) {
    $totalPayloadBytes = Add-CheckedSize -Total $totalPayloadBytes -Size ([int64]$fileReceipt["size"]) -Label "final staged payload"
  }
  if ($totalPayloadBytes -le 0 -or $totalPayloadBytes -gt $maximumInstallerBytes) {
    throw "Final staged package payload is outside the 512 MiB bound: $totalPayloadBytes"
  }
  $estimatedSizeKib = [int64][Math]::Ceiling($totalPayloadBytes / 1KB)

  foreach ($relative in $stagedFiles.Keys) {
    $lower = $relative.ToLowerInvariant()
    if ($lower.Contains("webview2") -or $lower.Contains("tauri") -or $lower.StartsWith("ui/")) {
      throw "Native candidate staging contains a forbidden WebView/Tauri asset: $relative"
    }
  }

  if ($ValidateOnly) {
    Write-Host "Validated $Variant Slint package staging ($totalPayloadBytes bytes) without building or publishing an installer."
    $completed = $true
    return
  }

  $makensis = Resolve-RequiredTool `
    -ExplicitPath $MakensisPath `
    -EnvironmentName "CLIPLINE_MAKENSIS" `
    -CommandName "makensis.exe" `
    -KnownPaths @(
      (Join-Path $env:ProgramFiles "NSIS\makensis.exe"),
      (Join-Path ${env:ProgramFiles(x86)} "NSIS\makensis.exe"),
      (Join-Path $env:LOCALAPPDATA "tauri\NSIS\Bin\makensis.exe")
    ) `
    -Label "makensis"
  $sevenZip = Resolve-RequiredTool `
    -ExplicitPath $SevenZipPath `
    -EnvironmentName "CLIPLINE_7ZIP" `
    -CommandName "7z.exe" `
    -KnownPaths @(
      (Join-Path $env:ProgramFiles "7-Zip\7z.exe"),
      (Join-Path ${env:ProgramFiles(x86)} "7-Zip\7z.exe")
    ) `
    -Label "7z"

  $makensisHash = Get-LowerSha256 $makensis
  $sevenZipHash = Get-LowerSha256 $sevenZip
  $makensisVersionOutput = @(& $makensis "/VERSION" 2>&1 | ForEach-Object { $_.ToString().Trim() })
  if ($LASTEXITCODE -ne 0 -or $makensisVersionOutput.Count -ne 1 -or $makensisVersionOutput[0] -cne "v3.11") {
    throw "makensis must be the reviewed v3.11, got '$($makensisVersionOutput -join ' ')'."
  }
  $sevenZipVersionOutput = @(& $sevenZip "i" 2>&1 | ForEach-Object { $_.ToString().Trim() })
  if ($LASTEXITCODE -ne 0 -or -not ($sevenZipVersionOutput -match '^7-Zip ')) {
    throw "7z must identify itself as 7-Zip before it can inspect an installer"
  }
  $sevenZipVersion = @($sevenZipVersionOutput | Where-Object { $_ -match '^7-Zip ' } | Select-Object -First 1)[0]
  $arguments = @(
    "/NOCD",
    "/V4",
    "/DCLIPLINE_VERSION=$version",
    "/DCLIPLINE_VERSION_NUMERIC=$numericVersion",
    "/DCLIPLINE_VARIANT=$Variant",
    "/DCLIPLINE_STAGE_DIR=$stageRoot",
    "/DCLIPLINE_OUTPUT_FILE=$temporaryInstaller",
    "/DCLIPLINE_ICON_PATH=$(Join-Path $stageRoot 'icon.ico')",
    "/DCLIPLINE_PACKAGE_DIR=$packageRoot",
    "/DCLIPLINE_ESTIMATED_SIZE_KIB=$estimatedSizeKib",
    $installerScript
  )
  if ((Get-LowerSha256 $stagedCandidate) -cne $ExpectedExecutableSha256) {
    throw "Staged Slint executable changed before makensis"
  }
  foreach ($source in @(
    @($cargoManifestPath, $contractSourceHashes.application_cargo_toml, "application Cargo manifest"),
    @($configPath, $contractSourceHashes.tauri_config, "Tauri product contract"),
    @($ffmpegManifestPath, $contractSourceHashes.ffmpeg_manifest, "FFmpeg manifest"),
    @($installerScript, $contractSourceHashes.installer_nsi, "installer.nsi"),
    @($sharedScript, $contractSourceHashes.installer_shared_nsh, "installer-shared.nsh"),
    @($PSCommandPath, $contractSourceHashes.builder, "package builder")
  )) {
    if ((Get-LowerSha256 $source[0]) -cne [string]$source[1]) {
      throw "$($source[2]) changed after validation"
    }
  }
  $toolOutput = @(& $makensis @arguments 2>&1 | ForEach-Object { $_.ToString() })
  if ($LASTEXITCODE -ne 0) {
    throw "makensis failed with exit code $LASTEXITCODE`n$($toolOutput -join "`n")"
  }
  $temporaryInfo = Get-RegularFile -Path $temporaryInstaller -Label "NSIS candidate installer" -MaximumBytes $maximumInstallerBytes

  New-Item -ItemType Directory -Path $extractRoot -ErrorAction Stop | Out-Null
  $extractOutput = @(& $sevenZip "x" "-y" "-o$extractRoot" $temporaryInfo.FullName 2>&1 | ForEach-Object { $_.ToString() })
  if ($LASTEXITCODE -ne 0) {
    throw "7z could not extract the candidate without execution (exit $LASTEXITCODE)`n$($extractOutput -join "`n")"
  }
  $extractedManifests = @(Get-ChildItem -LiteralPath $extractRoot -Recurse -File -Filter "package-manifest.json")
  if ($extractedManifests.Count -ne 1) {
    throw "Extracted installer must contain exactly one package-manifest.json, found $($extractedManifests.Count)"
  }
  $payloadRoot = $extractedManifests[0].Directory.FullName
  $extractedFiles = Get-RelativeFileMap $payloadRoot
  foreach ($relative in $extractedFiles.Keys) {
    $lower = $relative.ToLowerInvariant()
    if ($lower.Contains("webview2") -or $lower.Contains("tauri") -or $lower.StartsWith("ui/")) {
      throw "Extracted native candidate contains a forbidden WebView/Tauri asset: $relative"
    }
  }
  foreach ($relative in $stagedFiles.Keys) {
    $installedRelative = $relative
    if (-not $extractedFiles.Contains($installedRelative)) {
      throw "Extracted installer is missing staged file $installedRelative"
    }
    if (
      [int64]$extractedFiles[$installedRelative].size -ne [int64]$stagedFiles[$relative].size -or
      [string]$extractedFiles[$installedRelative].sha256 -cne [string]$stagedFiles[$relative].sha256
    ) {
      throw "Extracted file hash or size differs from staging: $installedRelative"
    }
  }
  $allowedOuterPayload = @(
    "Uninstall.exe",
    '$PLUGINSDIR/modern-wizard.bmp',
    '$PLUGINSDIR/nsDialogs.dll',
    '$PLUGINSDIR/System.dll'
  )
  $unexpectedPayload = @(
    $extractedFiles.Keys |
      Where-Object { -not $stagedFiles.Contains($_) -and $allowedOuterPayload -cnotcontains $_ }
  )
  if ($unexpectedPayload.Count -ne 0) {
    throw "Extracted installer contains unexpected application payload: $($unexpectedPayload -join ', ')"
  }

  Copy-CreateNew -Source $temporaryInstaller -Destination $outputPath
  $publishedInstaller = $true
  $publishedHash = Get-LowerSha256 $outputPath
  $publishedInfo = Get-Item -LiteralPath $outputPath
  $buildReceipt = [ordered]@{
    schema_version = 1
    package_kind = $packageKind
    distribution = "internal-only"
    product_identity = "io.clipline.app"
    version = $version
    variant = $Variant
    artifact_name = $artifactName
    installer_size = [int64]$publishedInfo.Length
    installer_sha256 = $publishedHash
    source_executable_sha256 = $candidateHash
    staged_manifest_sha256 = Get-LowerSha256 (Join-Path $stageRoot "package-manifest.json")
    installer_nsi_sha256 = $contractSourceHashes.installer_nsi
    installer_shared_nsh_sha256 = $contractSourceHashes.installer_shared_nsh
    builder_sha256 = $contractSourceHashes.builder
    makensis_path = $makensis
    makensis_version = "v3.11"
    makensis_sha256 = $makensisHash
    seven_zip_path = $sevenZip
    seven_zip_sha256 = $sevenZipHash
    seven_zip_version = $sevenZipVersion
    extracted_without_execution = $true
    webview_payloads = 0
  }
  Write-Utf8CreateNew -Path $outputReceiptPath -Text (($buildReceipt | ConvertTo-Json -Depth 6) + "`n")
  $publishedReceipt = $true
  $completed = $true
  Write-Host "Built and extraction-verified internal Slint candidate: $outputPath"
} finally {
  if (Test-Path -LiteralPath $workRoot) {
    Remove-Item -LiteralPath $workRoot -Recurse -Force
  }
  if (-not $completed) {
    if ($publishedInstaller -and (Test-Path -LiteralPath $outputPath)) {
      Remove-Item -LiteralPath $outputPath -Force
    }
    if ($publishedReceipt -and (Test-Path -LiteralPath $outputReceiptPath)) {
      Remove-Item -LiteralPath $outputReceiptPath -Force
    }
  }
}
