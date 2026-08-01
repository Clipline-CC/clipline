[CmdletBinding()]
param(
    [ValidateSet("Generate", "Validate", "SelfTest")]
    [string]$Mode = "Generate",
    [string]$FfmpegPath,
    [string]$FfprobePath,
    [ValidateSet("h264_mf", "h264_nvenc", "h264_qsv", "h264_amf")]
    [string]$H264Encoder,
    [switch]$IncludeOptionalCodecs,
    [string]$FixtureDirectory
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
if ([string]::IsNullOrWhiteSpace($FixtureDirectory)) {
    $FixtureDirectory = Join-Path $PSScriptRoot "../fixtures/playback"
}
$FixtureDirectory = [System.IO.Path]::GetFullPath($FixtureDirectory)
$ManifestPath = Join-Path $FixtureDirectory "manifest.json"

function Assert-Condition {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw $Message }
}

function Get-OptionalProperty {
    param($Object, [string]$Name)
    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property) { return $null }
    return $property.Value
}

function Get-Sha256 {
    param([string]$Path)
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Test-Sha256 {
    param([AllowNull()][string]$Value)
    return $null -ne $Value -and $Value -match "^[0-9a-f]{64}$"
}

function Read-Manifest {
    Assert-Condition (Test-Path -LiteralPath $ManifestPath -PathType Leaf) "Missing fixture manifest: $ManifestPath"
    try { return [System.IO.File]::ReadAllText($ManifestPath) | ConvertFrom-Json }
    catch { throw "Invalid fixture manifest JSON: $($_.Exception.Message)" }
}

function Assert-ManifestDefinition {
    param($Manifest)
    Assert-Condition ([int]$Manifest.schema_version -eq 1) "manifest schema_version must be 1"
    Assert-Condition ([string]$Manifest.suite -eq "clipline-native-playback-v1") "Unexpected fixture suite id"
    Assert-Condition ([string]$Manifest.source.kind -eq "first-party-procedural") "Fixture sources must be first-party procedural"
    Assert-Condition (-not [bool]$Manifest.reproducibility.binary_identical) "MFT output must not claim byte reproducibility"

    $requiredIds = @("h264-one-opus-3s", "h264-two-opus-markers-5s", "h264-long-gop-6s", "h264-variable-content-2s")
    $fixtures = @($Manifest.fixtures)
    $ids = @($fixtures | ForEach-Object { [string]$_.id })
    $files = @($fixtures | ForEach-Object { [string]$_.file })
    Assert-Condition (($ids | Select-Object -Unique).Count -eq $ids.Count) "Fixture ids must be unique"
    Assert-Condition (($files | Select-Object -Unique).Count -eq $files.Count) "Fixture file names must be unique"

    foreach ($requiredId in $requiredIds) {
        $matches = @($fixtures | Where-Object { [string]$_.id -eq $requiredId })
        Assert-Condition ($matches.Count -eq 1) "Required fixture is missing or duplicated: $requiredId"
        Assert-Condition ([bool]$matches[0].gating) "Required decoder fixture must be gating: $requiredId"
        Assert-Condition (-not [bool]$matches[0].production_mux_oracle) "FFmpeg-muxed fixture cannot claim production mux provenance: $requiredId"
    }

    foreach ($fixture in $fixtures) {
        $id = [string]$fixture.id
        $file = [string]$fixture.file
        Assert-Condition ($file -eq [System.IO.Path]::GetFileName($file)) "Fixture '$id' must use a basename"
        Assert-Condition ($file.EndsWith(".mp4", [System.StringComparison]::OrdinalIgnoreCase)) "Fixture '$id' must be an MP4"
        Assert-Condition ([double]$fixture.recipe.duration_seconds -gt 0) "Fixture '$id' has no positive duration"
        Assert-Condition ([int]$fixture.recipe.frame_rate -eq 30) "Fixture '$id' must use 30 fps"
        Assert-Condition ([int]$fixture.recipe.gop_frames -gt 0) "Fixture '$id' has no GOP size"
        Assert-Condition (@($fixture.recipe.audio_frequencies_hz).Count -eq [int]$fixture.expect.audio_track_count) "Fixture '$id' audio recipe mismatch"
        Assert-Condition ([int]$fixture.expect.width -eq 640 -and [int]$fixture.expect.height -eq 360) "Fixture '$id' must be 640x360"
        Assert-Condition ([string]$fixture.expect.audio_codec -eq "opus") "Fixture '$id' must use Opus"
        if ([bool]$fixture.gating) {
            Assert-Condition ([string]$fixture.expect.video_codec -eq "h264") "Gating fixture '$id' must use H.264"
            Assert-Condition ([string]$fixture.expect.video_profile -eq "High") "Gating fixture '$id' must use High profile"
            Assert-Condition (Test-Sha256 ([string]$fixture.artifact.sha256)) "Frozen fixture '$id' needs a SHA-256"
            Assert-Condition ([int64]$fixture.artifact.bytes -gt 0) "Frozen fixture '$id' needs a byte count"
            Assert-Condition (@($fixture.artifact.ffmpeg_arguments).Count -gt 0) "Frozen fixture '$id' needs FFmpeg arguments"
        }
        else {
            Assert-Condition ([bool]$fixture.capability_only) "Non-gating fixture '$id' must be capability_only"
        }
    }

    $markerFixture = @($fixtures | Where-Object { [string]$_.id -eq "h264-two-opus-markers-5s" })[0]
    $sidecarPath = Join-Path $FixtureDirectory ([string]$markerFixture.sidecar.file)
    Assert-Condition (Test-Path -LiteralPath $sidecarPath -PathType Leaf) "Marker sidecar is missing"
    Assert-Condition ((Get-Sha256 $sidecarPath) -eq [string]$markerFixture.sidecar.sha256) "Marker sidecar hash mismatch"
    $sidecar = [System.IO.File]::ReadAllText($sidecarPath) | ConvertFrom-Json
    Assert-Condition ([double]$sidecar.duration_s -eq 5.0) "Marker sidecar duration mismatch"
    Assert-Condition (@($sidecar.audio_tracks).Count -eq 2) "Marker sidecar must map two audio tracks"
    Assert-Condition (@($sidecar.markers).Count -ge 2) "Marker sidecar must include markers"
}

function Resolve-Ffmpeg {
    if (-not [string]::IsNullOrWhiteSpace($FfmpegPath)) {
        Assert-Condition (Test-Path -LiteralPath $FfmpegPath -PathType Leaf) "FFmpeg not found: $FfmpegPath"
        return (Resolve-Path -LiteralPath $FfmpegPath).Path
    }
    $candidates = @()
    if (-not [string]::IsNullOrWhiteSpace($env:LOCALAPPDATA)) {
        $candidates += Join-Path $env:LOCALAPPDATA "Clipline/ffmpeg/ffmpeg.exe"
    }
    $candidates += Join-Path $PSScriptRoot "../apps/clipline-app/ffmpeg/ffmpeg.exe"
    foreach ($candidate in $candidates) {
        if (Test-Path -LiteralPath $candidate -PathType Leaf) { return (Resolve-Path -LiteralPath $candidate).Path }
    }
    $command = Get-Command ffmpeg -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($null -ne $command) { return $command.Source }
    throw "FFmpeg not found. Pass -FfmpegPath or stage the reviewed LGPL binary."
}

function Resolve-Ffprobe {
    if (-not [string]::IsNullOrWhiteSpace($FfprobePath)) {
        Assert-Condition (Test-Path -LiteralPath $FfprobePath -PathType Leaf) "ffprobe not found: $FfprobePath"
        return (Resolve-Path -LiteralPath $FfprobePath).Path
    }
    $command = Get-Command ffprobe -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($null -ne $command) { return $command.Source }
    return $null
}

function Invoke-Media {
    param([string]$Executable, [object[]]$Arguments, [string]$Description)
    $previousPreference = $ErrorActionPreference
    try {
        # Windows PowerShell promotes native stderr to ErrorRecord objects when
        # Stop is active, even when the native process succeeds.
        $ErrorActionPreference = "Continue"
        $output = @(& $Executable @Arguments 2>&1)
        $exitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previousPreference
    }
    if ($exitCode -ne 0) {
        $details = ($output | ForEach-Object { [string]$_ }) -join [Environment]::NewLine
        throw "$Description failed.$([Environment]::NewLine)$details"
    }
    return @($output | ForEach-Object { [string]$_ })
}

function Get-ToolInfo {
    param([string]$Executable)
    $lines = @(Invoke-Media $Executable @("-hide_banner", "-version") "Reading tool version")
    return [ordered]@{
        file_name = [System.IO.Path]::GetFileName($Executable)
        sha256 = Get-Sha256 $Executable
        version = [string]$lines[0]
        configuration = [string](@($lines | Where-Object { $_ -match "^configuration:" }) -join " ")
    }
}

function Assert-ReviewedFfmpeg {
    param([string]$Ffmpeg)
    $info = Get-ToolInfo $Ffmpeg
    Assert-Condition ([string]$info.configuration -notmatch "--enable-gpl") "GPL-enabled FFmpeg builds are not allowed"
    Assert-Condition ([string]$info.configuration -notmatch "--enable-nonfree") "Non-free FFmpeg builds are not allowed"
    Assert-Condition ([string]$info.configuration -match "--disable-libx264") "Fixture FFmpeg must explicitly disable libx264"
    $encoders = @(Invoke-Media $Ffmpeg @("-hide_banner", "-encoders") "Enumerating encoders")
    Assert-Condition (@($encoders | Where-Object { $_ -match "\sh264_mf\s" }).Count -gt 0) "Reviewed FFmpeg does not expose h264_mf"
    Assert-Condition (@($encoders | Where-Object { $_ -match "\slibopus\s" }).Count -gt 0) "Reviewed FFmpeg does not expose libopus"
    if (-not [string]::IsNullOrWhiteSpace($H264Encoder)) {
        Assert-Condition ($H264Encoder -eq "h264_mf") "Frozen oracle generation is pinned to h264_mf"
    }
    return $info
}

function New-Arguments {
    param($Fixture, [string]$Output)
    $recipe = $Fixture.recipe
    $duration = ([double]$recipe.duration_seconds).ToString("0.###", [System.Globalization.CultureInfo]::InvariantCulture)
    $arguments = @("-hide_banner", "-nostdin", "-y", "-f", "lavfi", "-i", [string]$recipe.visual_filter)
    foreach ($frequency in @($recipe.audio_frequencies_hz)) {
        $arguments += @("-f", "lavfi", "-i", "sine=frequency=$([int]$frequency):sample_rate=48000:duration=$duration")
    }
    $arguments += @("-map_metadata", "-1", "-map", "0:v:0")
    for ($index = 0; $index -lt @($recipe.audio_frequencies_hz).Count; $index++) {
        $arguments += @("-map", "$($index + 1):a:0")
    }
    $arguments += @(
        "-vf", "format=nv12", "-c:v", "h264_mf", "-profile:v", "100", "-level:v", "40",
        "-rate_control", "cbr", "-scenario", "archive", "-r", "30", "-g", [string][int]$recipe.gop_frames,
        "-bf", "0", "-b:v", "450k", "-maxrate", "450k", "-bufsize", "900k",
        "-c:a", "libopus", "-b:a", "64k", "-vbr", "off", "-compression_level", "10",
        "-application", "audio", "-frame_duration", "20", "-ar", "48000", "-ac", "2"
    )
    $trackIds = @(Get-OptionalProperty $recipe "audio_track_ids")
    for ($index = 0; $index -lt @($recipe.audio_frequencies_hz).Count; $index++) {
        $id = if ($trackIds.Count -gt $index) { [string]$trackIds[$index] } else { "output" }
        $label = if ($id -eq "microphone") { "Microphone" } else { "Output Audio" }
        $disposition = if ($index -eq 0) { "default" } else { "0" }
        $arguments += @("-metadata:s:a:$index", "title=$label", "-metadata:s:a:$index", "handler_name=$label", "-metadata:s:a:$index", "language=und", "-disposition:a:$index", $disposition)
    }
    $arguments += @(
        "-metadata:s:v:0", "language=und", "-metadata", "title=Clipline procedural playback fixture",
        "-metadata", "creation_time=2000-01-01T00:00:00Z", "-t", $duration, "-shortest",
        "-movflags", "+faststart+disable_chpl", "-f", "mp4", $Output
    )
    return $arguments
}

function Assert-OptionalFfprobe {
    param($Fixture, [string]$Path, [AllowNull()][string]$Ffprobe)
    if ([string]::IsNullOrWhiteSpace($Ffprobe)) { return }
    $json = @(Invoke-Media $Ffprobe @("-v", "error", "-show_entries", "stream=codec_name,codec_type,profile,width,height,sample_rate:format=duration", "-of", "json", $Path) "ffprobe validation") -join [Environment]::NewLine
    $probe = $json | ConvertFrom-Json
    $video = @($probe.streams | Where-Object { $_.codec_type -eq "video" })
    $audio = @($probe.streams | Where-Object { $_.codec_type -eq "audio" })
    Assert-Condition ($video.Count -eq 1 -and $video[0].codec_name -eq "h264" -and $video[0].profile -eq "High") "ffprobe H.264 High validation failed"
    Assert-Condition ($audio.Count -eq [int]$Fixture.expect.audio_track_count) "ffprobe audio track count failed"
}

function Assert-Media {
    param($Fixture, [string]$Path, [string]$Ffmpeg, [AllowNull()][string]$Ffprobe, [bool]$CheckHash)
    $id = [string]$Fixture.id
    Assert-Condition (Test-Path -LiteralPath $Path -PathType Leaf) "Missing media: $Path"
    $decode = @(Invoke-Media $Ffmpeg @("-hide_banner", "-v", "info", "-xerror", "-i", $Path, "-map", "0", "-vf", "showinfo", "-f", "null", "-") "Full decode of $id")
    Assert-Condition (@($decode | Where-Object { $_ -match "^  Stream #0:0.*Video: h264 \(High\).*640x360.*30 fps" }).Count -eq 1) "$id is not 640x360 H.264 High at 30 fps"
    $audioLines = @($decode | Where-Object { $_ -match "^  Stream #0:\d+.*Audio: opus.*48000 Hz" })
    Assert-Condition ($audioLines.Count -eq [int]$Fixture.expect.audio_track_count) "$id has the wrong Opus track count"
    $frameLines = @($decode | Where-Object { $_ -match "Parsed_showinfo.* n:\s*\d+" })
    $expectedFrames = [int]([double]$Fixture.expect.duration_seconds * [double]$Fixture.expect.frame_rate)
    Assert-Condition ($frameLines.Count -eq $expectedFrames) "$id decoded $($frameLines.Count) frames; expected $expectedFrames"
    $keyframes = @()
    foreach ($line in $frameLines) {
        if ($line -match "pts_time:([0-9.]+).*iskey:1") { $keyframes += [double]$Matches[1] }
    }
    Assert-Condition ($keyframes.Count -gt 0 -and $keyframes[0] -le 0.1) "$id has no initial keyframe"
    $timeline = @($keyframes) + @([double]$Fixture.expect.duration_seconds)
    $gaps = @()
    for ($index = 1; $index -lt $timeline.Count; $index++) { $gaps += $timeline[$index] - $timeline[$index - 1] }
    $largest = [double](($gaps | Measure-Object -Maximum).Maximum)
    $maximum = Get-OptionalProperty $Fixture.expect "maximum_keyframe_gap_seconds"
    if ($null -ne $maximum) { Assert-Condition ($largest -le [double]$maximum) "$id GOP gap is too large: $largest" }
    $minimum = Get-OptionalProperty $Fixture.expect "minimum_keyframe_gap_seconds"
    if ($null -ne $minimum) { Assert-Condition ($largest -ge [double]$minimum) "$id GOP gap is too short: $largest" }
    $minimumDistinct = Get-OptionalProperty $Fixture.expect "minimum_distinct_frame_hashes"
    if ($null -ne $minimumDistinct) {
        $hashOutput = @(Invoke-Media $Ffmpeg @("-v", "error", "-i", $Path, "-map", "0:v:0", "-an", "-f", "framemd5", "-") "Frame hash validation")
        $hashes = @($hashOutput | Where-Object { $_ -notmatch "^#" -and $_ -match ",\s*([0-9a-f]{32})$" } | ForEach-Object { $Matches[1] } | Select-Object -Unique)
        Assert-Condition ($hashes.Count -ge [int]$minimumDistinct) "$id does not contain enough distinct decoded frames"
    }
    Assert-OptionalFfprobe $Fixture $Path $Ffprobe
    if ($CheckHash) {
        Assert-Condition ((Get-Sha256 $Path) -eq [string]$Fixture.artifact.sha256) "$id SHA-256 mismatch"
        Assert-Condition ((Get-Item -LiteralPath $Path).Length -eq [int64]$Fixture.artifact.bytes) "$id byte-count mismatch"
    }
}

function Write-Manifest {
    param($Manifest)
    $json = $Manifest | ConvertTo-Json -Depth 20
    $encoding = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($ManifestPath, $json + [Environment]::NewLine, $encoding)
}

function Invoke-Generate {
    param($Manifest)
    $ffmpeg = Resolve-Ffmpeg
    $ffprobe = Resolve-Ffprobe
    $toolInfo = Assert-ReviewedFfmpeg $ffmpeg
    foreach ($fixture in @($Manifest.fixtures | Where-Object { [bool]$_.gating })) {
        $path = Join-Path $FixtureDirectory ([string]$fixture.file)
        $arguments = @(New-Arguments $fixture $path)
        [void](Invoke-Media $ffmpeg $arguments "Generating $($fixture.id)")
        Assert-Media $fixture $path $ffmpeg $ffprobe $false
        $fixture.artifact.sha256 = Get-Sha256 $path
        $fixture.artifact.bytes = (Get-Item -LiteralPath $path).Length
        $recorded = @($arguments)
        $recorded[$recorded.Count - 1] = [string]$fixture.file
        $fixture.artifact.ffmpeg_arguments = $recorded
    }
    $Manifest.materialization.state = "generated"
    $Manifest.materialization.generated_at_utc = [DateTime]::UtcNow.ToString("o")
    $Manifest.materialization.host = [ordered]@{ os = [Environment]::OSVersion.VersionString }
    $Manifest.materialization.ffmpeg = $toolInfo
    $Manifest.materialization.ffprobe = if ([string]::IsNullOrWhiteSpace($ffprobe)) { $null } else { Get-ToolInfo $ffprobe }
    $Manifest.materialization.h264_encoder = "h264_mf"
    Write-Manifest $Manifest
    Assert-ManifestDefinition $Manifest
    if ($IncludeOptionalCodecs) { Write-Warning "HEVC/AV1 remain non-gating local capability fixtures and are not materialized by this corpus generator." }
}

function Invoke-Validate {
    param($Manifest)
    $ffmpeg = Resolve-Ffmpeg
    $ffprobe = Resolve-Ffprobe
    [void](Assert-ReviewedFfmpeg $ffmpeg)
    foreach ($fixture in @($Manifest.fixtures | Where-Object { [bool]$_.gating })) {
        Assert-Media $fixture (Join-Path $FixtureDirectory ([string]$fixture.file)) $ffmpeg $ffprobe $true
        Write-Host "Validated $($fixture.file)"
    }
}

$manifest = Read-Manifest
if ($Mode -eq "SelfTest") {
    Assert-ManifestDefinition $manifest
    Write-Host "Playback fixture definition is valid; FFmpeg was not required."
}
elseif ($Mode -eq "Generate") {
    Invoke-Generate $manifest
}
else {
    Assert-ManifestDefinition $manifest
    Invoke-Validate $manifest
}
