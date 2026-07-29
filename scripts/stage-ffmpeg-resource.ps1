param(
  [string]$ArchivePath,
  [switch]$VerifyOnly
)

$ErrorActionPreference = "Stop"

$repoRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
$appRoot = Join-Path $repoRoot "apps\clipline-app"
$destination = [System.IO.Path]::GetFullPath((Join-Path $appRoot "ffmpeg"))
$expectedDestination = [System.IO.Path]::GetFullPath((Join-Path $repoRoot "apps\clipline-app\ffmpeg"))
if ($destination -ne $expectedDestination) {
  throw "Refusing to stage FFmpeg outside the app resource directory: $destination"
}

$manifestPath = Join-Path $appRoot "ffmpeg-runtime.json"
$manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
if ([int]$manifest.schema_version -ne 1) {
  throw "FFmpeg resource manifest schema_version must be 1"
}
if ([string]::IsNullOrWhiteSpace($ArchivePath)) {
  $ArchivePath = Join-Path (Join-Path $env:LOCALAPPDATA "Clipline\release-inputs") $manifest.archive_name
}
$archive = [System.IO.Path]::GetFullPath($ArchivePath)
$archiveInfo = Get-Item -LiteralPath $archive -Force -ErrorAction Stop
if ($archiveInfo.PSIsContainer -or $archiveInfo.LinkType) {
  throw "FFmpeg release input must be a regular archive file: $archive"
}
if ($archiveInfo.Name -cne $manifest.archive_name) {
  throw "FFmpeg archive name must be $($manifest.archive_name), got $($archiveInfo.Name)"
}

$archiveHash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
if ($archiveHash -cne $manifest.archive_sha256) {
  throw "FFmpeg archive SHA-256 mismatch: expected $($manifest.archive_sha256), got $archiveHash"
}

Add-Type -AssemblyName System.IO.Compression.FileSystem
$temporary = Join-Path $appRoot (".ffmpeg-stage-{0}-{1}" -f $PID, [guid]::NewGuid().ToString("N"))
$backup = Join-Path $appRoot (".ffmpeg-previous-{0}-{1}" -f $PID, [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $temporary -ErrorAction Stop | Out-Null
$published = $false

try {
  $verifiedFiles = @()
  $zip = [System.IO.Compression.ZipFile]::OpenRead($archive)
  try {
    foreach ($file in $manifest.allowed_files) {
      if (
        [string]::IsNullOrWhiteSpace($file.staged_name) -or
        $file.staged_name -cne [System.IO.Path]::GetFileName($file.staged_name) -or
        $file.archive_path.Contains("..") -or
        [System.IO.Path]::IsPathRooted($file.archive_path)
      ) {
        throw "Unsafe FFmpeg allowlist entry: $($file.archive_path) -> $($file.staged_name)"
      }
      $entryName = "$($manifest.archive_root)/$($file.archive_path)".Replace("\", "/")
      $entries = @($zip.Entries | Where-Object { $_.FullName -ceq $entryName })
      if ($entries.Count -ne 1) {
        throw "Expected one exact FFmpeg archive entry $entryName, found $($entries.Count)"
      }
      $entry = $entries[0]
      if ($entry.Length -ne [int64]$file.size) {
        throw "FFmpeg entry size mismatch for $entryName"
      }

      $outputPath = Join-Path $temporary $file.staged_name
      $inputStream = $entry.Open()
      $outputStream = [System.IO.File]::Open(
        $outputPath,
        [System.IO.FileMode]::CreateNew,
        [System.IO.FileAccess]::Write,
        [System.IO.FileShare]::None
      )
      try {
        $inputStream.CopyTo($outputStream)
      } finally {
        $outputStream.Dispose()
        $inputStream.Dispose()
      }

      $fileHash = (Get-FileHash -LiteralPath $outputPath -Algorithm SHA256).Hash.ToLowerInvariant()
      if ($fileHash -cne $file.sha256) {
        throw "FFmpeg staged-file SHA-256 mismatch for $($file.staged_name)"
      }
      $verifiedFiles += [ordered]@{
        name = $file.staged_name
        size = [int64]$file.size
        sha256 = $fileHash
      }
    }
  } finally {
    $zip.Dispose()
  }

  $trackedReadme = Join-Path $destination "README.md"
  if (-not (Test-Path -LiteralPath $trackedReadme -PathType Leaf)) {
    throw "Tracked FFmpeg release notice is missing: $trackedReadme"
  }
  Copy-Item -LiteralPath $trackedReadme -Destination (Join-Path $temporary "README.md")

  $ffmpegExe = Join-Path $temporary "ffmpeg.exe"
  $versionLines = @(& $ffmpegExe -version 2>&1 | ForEach-Object { $_.ToString() })
  if ($LASTEXITCODE -ne 0) {
    throw "Verified FFmpeg failed its version probe with exit code $LASTEXITCODE"
  }
  if ($versionLines.Count -eq 0 -or $versionLines[0] -cne $manifest.version_line) {
    throw "FFmpeg version line does not match the reviewed manifest"
  }
  $versionText = $versionLines -join "`n"
  foreach ($required in $manifest.required_configuration) {
    if (-not $versionText.Contains($required)) {
      throw "FFmpeg configuration is missing required flag $required"
    }
  }
  foreach ($forbidden in $manifest.forbidden_configuration) {
    if ($versionText.Contains($forbidden)) {
      throw "FFmpeg configuration contains forbidden flag $forbidden"
    }
  }

  $manifestHash = (Get-FileHash -LiteralPath $manifestPath -Algorithm SHA256).Hash.ToLowerInvariant()
  $provenance = [ordered]@{
    schema_version = 1
    provider = $manifest.provider
    release_tag = $manifest.release_tag
    published_at = $manifest.published_at
    archive_name = $manifest.archive_name
    archive_url = $manifest.archive_url
    archive_sha256 = $archiveHash
    manifest_sha256 = $manifestHash
    ffmpeg_version = $versionLines[0]
    configuration = ($versionLines | Where-Object { $_.StartsWith("configuration:") } | Select-Object -First 1)
    source_offer_url = $manifest.source_offer_url
    ffmpeg_source_url = $manifest.ffmpeg_source_url
    files = $verifiedFiles
  }
  $provenanceJson = $provenance | ConvertTo-Json -Depth 6
  [System.IO.File]::WriteAllText(
    (Join-Path $temporary "PROVENANCE.json"),
    ($provenanceJson + "`n"),
    [System.Text.UTF8Encoding]::new($false)
  )

  Write-Host ("Verified FFmpeg provenance: " + ($provenance | ConvertTo-Json -Depth 6 -Compress))
  if ($VerifyOnly) {
    Write-Host "FFmpeg archive verification completed without changing staged resources."
    return
  }

  if (Test-Path -LiteralPath $destination) {
    Move-Item -LiteralPath $destination -Destination $backup
  }
  try {
    Move-Item -LiteralPath $temporary -Destination $destination
    $published = $true
  } catch {
    if ((Test-Path -LiteralPath $backup) -and (Test-Path -LiteralPath $destination)) {
      Remove-Item -LiteralPath $destination -Recurse -Force
    }
    if (Test-Path -LiteralPath $backup) {
      Move-Item -LiteralPath $backup -Destination $destination
    }
    throw
  }
  if (Test-Path -LiteralPath $backup) {
    Remove-Item -LiteralPath $backup -Recurse -Force
  }
  Write-Host "Staged verified FFmpeg resource in $destination"
} finally {
  if (-not $published -and (Test-Path -LiteralPath $temporary)) {
    Remove-Item -LiteralPath $temporary -Recurse -Force
  }
  if (Test-Path -LiteralPath $backup) {
    if (-not (Test-Path -LiteralPath $destination)) {
      Move-Item -LiteralPath $backup -Destination $destination
    } else {
      Remove-Item -LiteralPath $backup -Recurse -Force
    }
  }
}
