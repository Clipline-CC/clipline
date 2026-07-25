# Native Software H.264 MFT Implementation Plan

> **For agentic workers:** Execute this plan task-by-task with strict TDD. Steps use checkbox
> (`- [ ]`) syntax for tracking and remain unticked by repository convention.

**Goal:** Let Clipline instantiate the Microsoft synchronous software H.264 Media Foundation
transform that it already probes and ranks, so recording works without a hardware encoder or an
FFmpeg runtime.

**Architecture:** Keep the existing asynchronous, zero-copy hardware MFT encoder unchanged. Add a
separate synchronous software MFT encoder that reads WGC BGRA textures to CPU memory, uses the
existing neutral BGRA-to-NV12 converter, submits system-memory NV12 samples, and drains caller-owned
H.264 output samples synchronously. Route only `Mft/MfSoftware/H264` candidates to it; retain the
existing FFmpeg `h264_mf` CPU path as a later candidate.

**Tech Stack:** Rust, windows-rs, Media Foundation, D3D11 readback, existing neutral CPU NV12
conversion and MP4 pipeline.

## Global Constraints

- Do not change the hardware MFT event pump, D3D manager, or GPU conversion path.
- Keep all COM, Media Foundation, and D3D11 operations under `windows/` behind safe wrappers.
- Reuse the existing limited-range Rec.709 CPU conversion.
- Preserve capture crop/scale behavior, timestamps, fixed GOPs, disabled B-frames, SPS/PPS
  extraction, AVCC samples, and H.264 track metadata.
- Respect the transform's output allocation model from `GetOutputStreamInfo`.
- Bound all buffer sizes and return errors rather than panicking on malformed dimensions or COM
  results.
- Follow strict TDD: observe each new test fail for the intended reason before implementation.

---

### Task 1: Lock the advertised software-MFT contract

**Files:**
- Modify: `crates/clipline-capture/src/windows/mft.rs`
- Modify: `apps/clipline-app/src/service.rs`

- [ ] Add a Windows-only real encoder test that skips only when the synchronous H.264 MFT is not
      advertised, then requires construction and encoding to succeed.
- [ ] Assert the output contains keyframed AVCC H.264, SPS/PPS, monotonic timestamps, and the
      configured dimensions.
- [ ] Add an app routing-policy test that maps only native `MfSoftware` candidates to the software
      MFT constructor.
- [ ] Run the focused tests and verify RED because the native constructor/routing does not exist.

### Task 2: Implement synchronous software H.264 encoding

**Files:**
- Modify: `crates/clipline-capture/src/windows/mft.rs`

- [ ] Add a dedicated `SoftwareMftH264Encoder`; do not make hardware-encoder fields optional.
- [ ] Enumerate and activate the synchronous H.264 MFT with `MFT_ENUM_FLAG_SYNCMFT`.
- [ ] Configure output before input, including bitrate, frame size/rate, progressive scan, High
      profile, limited-range Rec.709 metadata, fixed GOP, and zero B-frames.
- [ ] Read BGRA textures back and convert them with `CpuVideoConverter`.
- [ ] Create timestamped system-memory NV12 input samples.
- [ ] Allocate output samples according to `GetOutputStreamInfo`, pump until
      `MF_E_TRANSFORM_NEED_MORE_INPUT`, handle stream changes, and drain synchronously at finish.
- [ ] Run the focused real encoder test and verify GREEN.

### Task 3: Wire service candidate construction

**Files:**
- Modify: `apps/clipline-app/src/service.rs`

- [ ] Replace the `software H.264 MFT is not yet wired` rejection with the native constructor.
- [ ] Preserve the ranked walk so failure still falls through to FFmpeg `h264_mf`.
- [ ] Run focused app and capture tests and verify GREEN.

### Task 4: Documentation and quality gates

**Files:**
- Modify: `ddoc.md`
- Modify: `handoff.md`

- [ ] Remove stale “not yet wired” caveats and document the synchronous CPU path.
- [ ] Run `cargo fmt --check`.
- [ ] Run `cargo test --workspace`.
- [ ] Run `cargo clean -p clipline-capture` and
      `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Review the diff for hardware-path regressions, unsafe-surface growth, buffer ownership,
      dependency changes, and license impact.
- [ ] Relaunch Clipline and verify the recorder reaches Running on the software encoder.
