# Clipline playback fixtures

This directory contains a small, frozen decoder corpus for the native review
player. The MP4 files are generated from FFmpeg `lavfi` sources; no recorded,
downloaded, or third-party media is used.

The committed files are:

- `manifest.json`: fixture recipes, validation expectations, and materialization
  provenance, including FFmpeg identity, exact arguments, sizes, and SHA-256.
- `h264-two-opus-markers-5s.markers.json`: a Clipline-compatible first-party
  timeline sidecar for the two-track fixture.
- `../../scripts/generate-playback-fixtures.ps1`: generator and validator.
- Four checked-in H.264 High + Opus MP4s: reviewed oracle bytes that keep tests
  and local experiments independent of the encoder installed on a machine.

These MP4s are deliberately labeled `production_mux_oracle: false`. FFmpeg
muxed them, so they prove decode, track-selection, seeking, and lifecycle paths,
but they do not prove Clipline's `HybridMp4Writer` layout. A production-authored
H.264 + Opus fixture remains a blocking input to the Milestone 3 media gate.

## Requirements

- Windows with an H.264 encoder exposed to FFmpeg. `h264_mf` is preferred.
  The script can use an explicitly selected non-GPL hardware encoder when the
  Media Foundation encoder is unavailable.
- A separate FFmpeg process with `libopus`, plus a matching `ffprobe` binary.
  The script rejects GPL/non-free FFmpeg builds and GPL H.264 encoders.
- PowerShell 5.1 or newer.

The reviewed per-user Clipline FFmpeg and release-staged
`apps/clipline-app/ffmpeg/ffmpeg.exe` are considered before `PATH`. `ffprobe` is
optional; the mandatory path uses FFmpeg itself for full decode, stream, frame,
keyframe, and visual-variation checks.

## Use

Check the committed definition without requiring FFmpeg:

```powershell
./scripts/generate-playback-fixtures.ps1 -Mode SelfTest
```

Generate all gating fixtures, fully decode them, and update the manifest:

```powershell
./scripts/generate-playback-fixtures.ps1 -Mode Generate
```

Validate an existing local materialization, including every recorded hash:

```powershell
./scripts/generate-playback-fixtures.ps1 -Mode Validate
```

To choose a fallback encoder explicitly:

```powershell
./scripts/generate-playback-fixtures.ps1 -Mode Generate -H264Encoder h264_nvenc
```

HEVC and AV1 fixtures are capability probes, never migration gates. Generate
them only on a machine that exposes an approved encoder:

```powershell
./scripts/generate-playback-fixtures.ps1 -Mode Generate -IncludeOptionalCodecs
```

## Reproducibility boundary

The procedural inputs, stream layout, metadata, encoder settings, and muxer
settings are pinned. Re-running reproduces the same required media semantics:
dimensions, duration, stream count, profile, sample rate, GOP shape, and
changing visual frames.

It is intentionally **not** a promise of byte-identical MP4 output. Media
Foundation and hardware encoders can change output across driver, OS, adapter,
and firmware revisions, and may even vary on the same host. SHA-256 values in
`manifest.json` identify one validated local materialization; they are updated
when fixtures are regenerated. Validation always includes ffprobe structure,
a full FFmpeg decode with `-xerror`, frame-hash content checks where required,
and comparison with the recorded SHA-256 values. Regeneration changes the
reviewed oracle materialization and therefore requires an intentional diff and
fresh validation; it is never an automatic CI step.

The committed marker sidecar is stable first-party JSON and has its own hash in
the manifest. Normal CI validates committed bytes and never needs an encoder.
