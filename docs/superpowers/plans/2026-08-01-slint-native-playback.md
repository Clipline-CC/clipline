# Slint Milestone 3: Headless Native Playback

> **For agentic workers:** Execute each task in order. Write the failing contract test before the
> implementation it protects. Checkboxes remain unticked by repository convention.

**Goal:** Prove a bounded, generation-cancelable Windows playback engine for Clipline-authored
H.264 High plus selectable/mixed Opus tracks before Slint is allowed to own any shipping UI.

**Scope:** Add `clipline-playback`, a headless diagnostic runner, deterministic neutral tests,
Windows Media Foundation/D3D11/WASAPI safe wrappers, and measured media evidence. Do not add Slint,
switch the Tauri application, link FFmpeg, add GStreamer, or promise HEVC/AV1 decode.

**Baseline:** branch `agent/slint-frontend-replacement-plan` at `94f8de2`. Milestone 2 provides the
bounded `IndexedMovie` API and the production-writer oracle. Milestone 1's matched Tauri measurements
remain pending while the user's installed Clipline stays open.

**Hard invariants:** Neutral reducer, scheduling, conversion, and mixing code must test on Ubuntu and
Windows. Every first-party `unsafe` and every COM object stays under `src/windows/` behind safe
wrappers. Encoded samples, decoded audio, and presentable video surfaces are all explicitly bounded.
Only a separately spawned reviewed LGPL FFmpeg executable may remain in existing encode/export paths;
the playback crate has no FFmpeg dependency.

## Task 1: Freeze the neutral command, event, and generation contract

**Files:**

- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Create: `crates/clipline-playback/Cargo.toml`
- Create: `crates/clipline-playback/src/lib.rs`
- Create: `crates/clipline-playback/src/command.rs`
- Create: `crates/clipline-playback/src/state.rs`
- Create: `crates/clipline-playback/tests/command_state.rs`

- [ ] Add the crate to the workspace with `clipline-mp4`, pinned `shiguredo_opus`, and `thiserror`
      as neutral dependencies. Match `clipline-capture` on `windows`/`windows-core` 0.62 behind
      `cfg(windows)`, with only reviewed Media Foundation, Audio, D3D11, DXGI, COM, Foundation,
      Performance, and Threading features.
- [ ] Add failing tests for public `PlaybackCommand::{Open, Play, Pause, Seek, Step, SetTracks,
      SetVolume, SetRate, Close}` and typed events/snapshots. Paths remain owned values; no UI or
      Tauri types enter the crate.
- [ ] Define monotonic open and seek generations. A newer `Open` or `Close` fences every older
      command/completion; a newer `Seek` fences every older seek/decode/frame completion within the
      active open.
- [ ] Define coalescing before threads exist: retain only the newest pending seek, transport intent
      (`Play`/`Pause`), volume, rate, and track selection; preserve ordered resource fences. Fix the
      inbox at 64 entries with one slot reserved for `Close`; a full inbox rejects any other new
      non-coalescable command with a typed `QueueFull` error and never drops `Close`.
- [ ] Gate initial playback to exactly 1x. Every other finite positive rate returns an explicit
      unsupported result; invalid volume/rate/time/track input fails without mutating live state.
- [ ] Confirm the integration test fails before implementing the reducer and bounded inbox.
- [ ] Implement the reducer and inbox to green, including deterministic capacity, overflow,
      coalescing, fence ordering, and reserved-`Close` tests.

## Task 2: Add bounded H.264 sample transport

**Files:**

- Create: `crates/clipline-playback/src/annexb.rs`
- Create: `crates/clipline-playback/src/sample_buffer.rs`
- Create: `crates/clipline-playback/tests/annexb.rs`

- [ ] Add failing vectors for 1-, 2-, and 4-byte MP4 NAL lengths, multiple NALs, empty/truncated
      samples, zero-length NALs, declared-length overflow, trailing bytes, and output-cap overflow.
- [ ] Convert one caller-buffered indexed H.264 sample to Annex B without whole-track copies. Reuse
      one bounded encoded buffer and one bounded converted buffer; reject a video sample above the
      reviewed cap before allocation.
- [ ] Emit SPS/PPS before the first sync sample after open/seek without duplicating them on ordinary
      inter frames. Preserve all parameter sets exposed by `PlaybackTrackConfig::H264`.
- [ ] Add a regression that walks every video sample in the production-writer fixture through the
      converter and proves bounds and sample ownership remain constant.
- [ ] Keep HEVC/AV1 typed as explicit unsupported capabilities in this milestone.

## Task 3: Extract bounded Opus decoding and mixing

**Files:**

- Create: `crates/clipline-playback/src/audio.rs`
- Create: `crates/clipline-playback/src/ring.rs`
- Create: `crates/clipline-playback/tests/audio_mix.rs`
- Modify: `crates/clipline-mp4/src/trim.rs` only if a genuinely neutral helper can replace existing
  duplicated mix behavior without broadening the public API unnecessarily

- [ ] Add failing tests for mono-to-stereo conversion, stereo preservation, `dOps` pre-skip,
      leading/internal edit gaps, absent packets as silence, two-track averaging, clipping bounds,
      corrupt packets, incompatible track formats, track switching, decoder reset, and EOF tails.
- [ ] Reuse the pinned `shiguredo_opus` decoder and preserve the existing average-active-track mix
      semantics. Do not introduce a new codec runtime.
- [ ] Bound compressed packet reads to the seek plan's sample ranges and the indexed timeline end;
      bound the mixed stereo queue by timeline duration, with no more than 500 ms of decoded audio
      retained. Reject hostile packet sizes before allocation.
- [ ] Keep the queue caller-drained and allocation-stable after warm-up. Expose counters for decoded,
      silent, corrupt, dropped, and mixed frames so the live gate can prove its behavior.
- [ ] Reset only affected decoder generations on seek/track change; file-start pre-skip must not be
      applied twice after a mid-file seek.

## Task 4: Implement the neutral scheduler and backend seam

**Files:**

- Create: `crates/clipline-playback/src/backend.rs`
- Create: `crates/clipline-playback/src/scheduler.rs`
- Create: `crates/clipline-playback/src/worker.rs`
- Create: `crates/clipline-playback/tests/scheduler.rs`
- Create: `crates/clipline-playback/tests/lifecycle.rs`
- Create: `docs/slint/native-playback-protocol.md`

- [ ] Define safe backend traits for video decode, audio rendering/clock, and frame publication so
      fake backends exercise every state transition on both CI operating systems.
- [ ] Add failing fake-clock tests for open/play/pause/resume, step, EOF, short clips, end-of-audio
      video tails, zero selected audio, delayed/gapped tracks, selected-track changes, volume, close
      during open/seek, seek clock re-anchoring, device loss, decoder error, and recovery.
- [ ] Define live metrics before implementing the scheduler: in headless mode, a frame is presented
      when the backend accepts one playback-owned surface at the scheduler publication edge; A/V
      error is the absolute difference between that frame PTS and the rebased audio clock sampled at
      that edge; a frame is late when publication occurs more than its indexed duration after PTS;
      a scheduler drop is a decoded eligible frame superseded by a newer due frame; seek latency runs
      from accepted command to publication of the final generation's target-correct frame. The
      late/drop ratio is the union of late or scheduler-dropped eligible frames divided by decoded
      eligible frames. Pin these definitions in the protocol and fake-clock tests before Task 8
      consumes the counters.
- [ ] Use the audio renderer's monotonically rebased position as the playback clock. When audio has
      ended or no audio track is selected, render bounded silence and advance an explicitly rebased
      monotonic tail clock rather than freezing video.
- [ ] Keep at most one presentable video frame. At each tick publish the newest frame due, release
      every superseded frame immediately, and count late/dropped frames.
- [ ] Make cancellation observable between index reads, packet conversion, decoder calls, audio
      production, and publication. Rapid alternating seeks must publish only the final generation.
- [ ] Seek from `IndexedMovie::seek_plan`: flush/reset backends, restart at the prior sync sample,
      decode preroll, discard until the clamped target, and report settled only after the correct
      frame is eligible for publication.

## Task 5: Add a safe Windows H.264/D3D11 decoder backend

**Files:**

- Create: `crates/clipline-playback/src/windows/mod.rs`
- Create: `crates/clipline-playback/src/windows/com.rs`
- Create: `crates/clipline-playback/src/windows/d3d11.rs`
- Create: `crates/clipline-playback/src/windows/mft_decode.rs`
- Create: `crates/clipline-playback/tests/windows_decoder.rs`
- Modify: `apps/clipline-app/tests/repository_security.rs`

- [ ] Add safe RAII guards for one COM apartment and process-wide balanced Media Foundation startup.
      `MFStartup` is ref-counted, so this playback-owned reference may balance with `MFShutdown`
      without changing `mft_probe.rs`'s deliberate process-lifetime reference. Do not copy the
      existing repeatedly-started encoder call pattern into every reopenable player instance.
- [ ] Create one hardware D3D11 device with `ID3D10Multithread::SetMultithreadProtected(TRUE)`, one
      `IMFDXGIDeviceManager`, and explicit hardware/software decoder capability reporting.
- [ ] Enumerate `MFT_CATEGORY_VIDEO_DECODER` for H.264, prefer a D3D11-aware hardware transform,
      configure Annex-B H.264 input and NV12 output, and label software fallback in every snapshot.
- [ ] Handle sync and async transforms, `MF_E_TRANSFORM_NEED_MORE_INPUT`,
      `MF_E_TRANSFORM_STREAM_CHANGE`, MFT-provided/caller-provided output samples, flush/drain, and
      device-manager reset behind one safe decoder interface.
- [ ] Never retain or alias an MFT-owned sample in the presentation mailbox. Copy with
      `CopySubresourceRegion` or an equivalent bounded operation into one playback-owned surface,
      release the MFT sample before the frame becomes presentable, and assert that ordering in a
      device test so the transform surface pool cannot be exhausted.
- [ ] Extend the repository security contract to scan the new crate and fail if first-party
      `unsafe`, Media Foundation, WASAPI, or D3D11 calls escape `clipline-playback/src/windows/`.
- [ ] Add Windows device tests for capability probe, production-fixture decode, dimensions/subtype,
      flush/reopen, corrupt access unit, software fallback reporting, and D3D device loss. Device
      tests self-skip under CI/no-hardware while neutral tests remain authoritative everywhere.

## Task 6: Add a safe WASAPI renderer and monotonic clock

**Files:**

- Create: `crates/clipline-playback/src/windows/wasapi_render.rs`
- Create: `crates/clipline-playback/tests/windows_audio.rs`

- [ ] Open the default render endpoint in shared mode with `IAudioClient3`/`IAudioRenderClient`,
      negotiate 48 kHz stereo float or perform one bounded explicit conversion, and report the exact
      device/format/buffer duration.
- [ ] Derive position from `IAudioClock`, rebase it monotonically across pause/resume and endpoint
      recreation, and re-anchor device position to the settled target after every seek/flush. Expose
      raw/rebased clock telemetry for A/V error measurement.
- [ ] Render silence on underrun without blocking the worker; count underruns and bound recovery.
      Never let the audio endpoint pull from an unbounded producer queue.
- [ ] Classify invalidated/resource-invalidated/service-stopped failures, release the endpoint, and
      either recover within the active generation or publish a terminal typed error.
- [ ] Add real-device tests for play/pause clock stability, drain, endpoint reopen, volume, buffer
      bounds, and clean close. Keep equivalent fake-device loss vectors in neutral tests.

## Task 7: Prove end-to-end headless playback and seek cancellation

**Files:**

- Create: `crates/clipline-playback/examples/headless_playback.rs`
- Create: `crates/clipline-playback/tests/fixture_playback.rs`
- Create: `scripts/measure-headless-playback.ps1`
- Modify: `scripts/lib/Clipline.ProcessMetrics.psm1` only if a frontend-neutral field is missing

- [ ] Open the checked-in production-writer fixture through `IndexedMovie<File>`, select both Opus
      tracks, play to EOF, and emit bounded JSON telemetry for clocks, A/V error, decoded/presented/
      dropped frames, audio underruns, queue high-water marks, decoder path, memory, handles, and
      resource release.
- [ ] Add deterministic worker tests with fake backends for rapid distant seek storms, close during
      seek, reopen after error, track switching during playback, and stale completion injection.
- [ ] Add a Windows live correctness run that decodes all H.264 frames, plays mixed audio, performs
      repeated seeks, and proves the input file can be renamed immediately after `Close`.
- [ ] Extend the existing process-tree sampler rather than using Task Manager. The headless harness
      records raw CSV, environment/provenance JSON, and p50/p95 summaries. Scope strictly to the
      spawned headless process tree with creation-time verification; exclude rather than abort on an
      unrelated installed Clipline, record every concurrent PID in provenance, and reject the run
      only when concurrent activity violates the protocol's system-idle/noise requirements.
- [ ] Keep the shipping Tauri application unchanged throughout this milestone.

## Task 8: Run the 1080p60 media gates

**Files:**

- Modify: `docs/slint/native-playback-protocol.md`
- Create: `artifacts/slint-playback/.gitkeep` only if the repository's artifact policy requires the
  directory; do not commit large performance media or machine-specific raw results

- [ ] Record or generate a local 1080p60 H.264 High plus two-Opus-track Clipline-authored file. Its
      final mux must come from the real recorder or the public `HybridMp4Writer`, never a foreign
      FFmpeg mux. Record its SHA-256, duration, encoder/mux provenance, GPU/driver, audio endpoint,
      and whether hardware or software decode was selected.
- [ ] Run at least three five-minute playback samples and matched seek-storm samples using the same
      environment rules as `docs/slint/baseline-protocol.md`.
- [ ] Gate p95 A/V error at 40 ms or lower, late/dropped presented frames below 0.5%, seek settle p95
      at 150 ms or lower and within one source frame, and no handle/thread/queue growth across 100
      open/play/seek/close cycles.
- [ ] Record private working set at 190 MiB or lower. The relative 80%-of-WebView memory and
      one-percentage-point CPU gates remain pending until the matched Milestone 1 Tauri run exists;
      do not invent or silently waive that comparison.
- [ ] If correctness, queue bounds, or absolute memory fail, stop before Milestone 4 and record a
      no-go. If only the matched comparison is unavailable, keep the implementation shippable but
      explicitly mark the media gate pending rather than claiming success.

## Task 9: Verify, review, commit, and hand off

**Files:**

- Modify: `handoff.md`
- Modify: `docs/slint/parity-ledger.md` only for behaviors actually verified

- [ ] Run `cargo test -p clipline-playback`, its Windows device tests, and fresh-cache
      `cargo clippy -p clipline-playback --all-targets -- -D warnings`.
- [ ] Run `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` on
      Windows; open or refresh the draft PR and require the repository's Ubuntu and Windows CI
      checks because ordinary branch pushes do not trigger this repository's workflow.
- [ ] Audit `cargo tree` and source for no linked FFmpeg/GStreamer/GPL runtime and no first-party
      `unsafe` outside `src/windows/`.
- [ ] Have an independent reviewer audit generations/cancellation, bounds, COM lifetimes, MFT
      ownership/flush, WASAPI clock rebasing, device loss, and the live evidence.
- [ ] Update the handoff with the stable API, decoder/fallback behavior, queue caps, fixture hashes,
      measured results, remaining matched gates, the explicit pending pitch-preserving non-1x rate
      stage/product decision, and the exact Milestone 4 entry point. Keep the same deferral visible
      in the parity ledger rather than treating 1x-only playback as implicit parity.
- [ ] Commit this plan before implementation, then commit implementation in logical conventional
      slices and push the branch after every green slice.
