# Slint Milestone 2: Bounded MP4 Playback Index

> **For agentic workers:** Execute each task in order. Write the failing contract test before the
> implementation it protects. Checkboxes remain unticked by repository convention.

**Goal:** Give the future native player a public, file-backed, bounded index over finalized
Clipline MP4 tracks and samples, including seek planning and caller-buffered reads, without adding a
second MP4 parser or loading media payloads into memory.

**Scope:** `clipline-mp4`, a Clipline-writer-authored playback fixture, tests, and documentation.
Do not add a decoder, Slint dependency, linked media runtime, or shipping application behavior.

**Baseline:** branch `agent/slint-frontend-replacement-plan` at `895bd59`. Milestone 1's matched-run
tooling is present; its measurements remain pending while the user's installed Clipline is open.

## Task 1: Freeze the public playback-index contract

**Files:**

- Create: `crates/clipline-mp4/tests/playback_index.rs`
- Create: `crates/clipline-mp4/src/playback_index.rs`
- Modify: `crates/clipline-mp4/src/lib.rs`

- [ ] Add failing integration tests for opening a finalized file without reading `mdat`, exposing
      ordered track metadata and sample offsets/sizes, and reporting separate DTS/PTS/duration/sync
      fields. Clipline forbids B-frames, so composition PTS equals DTS within the media timeline;
      track edit lists may still map those samples to a different presentation timestamp.
- [ ] Define public typed H.264, HEVC, AV1, and Opus configuration. Preserve H.264 SPS/PPS and the
      MP4 NAL-length field; preserve equivalent HEVC arrays/length size and AV1 config bytes without
      promising decoder availability.
- [ ] Add a generic reader constructor for deterministic tests and `open(Path)` for the production
      file-backed path. The index owns its reader but exposes only immutable metadata.
- [ ] Confirm the new integration test fails because the API is absent before implementation.

## Task 2: Reuse and harden the existing finalized-movie parser

**Files:**

- Modify: `crates/clipline-mp4/src/trim.rs`
- Modify: `crates/clipline-mp4/src/playback_index.rs`

- [ ] Make the existing bounded `trim.rs` parsed-movie representation available crate-internally;
      do not duplicate `moov`, `stsd`, edit-list, or sample-table parsing.
- [ ] Carry the H.264/HEVC NAL-length size out of `avcC`/`hvcC` parsing and validate it. Keep all
      existing 64 MiB `moov`, four-million-sample, integer-overflow, and source-range limits.
- [ ] Explicitly reject `ctts` until composition offsets are deliberately supported. This prevents
      foreign B-frame files from being mislabeled as PTS=DTS while preserving Clipline-authored
      no-B-frame playback.
- [ ] Reject zero-sized samples, zero-duration samples, overlapping sample byte ranges, duplicate
      track selections, invalid track kinds, and all out-of-file ranges before playback begins.
- [ ] Move parsed sample metadata into the public index without copying `mdat` payload bytes.

## Task 3: Add bounded sample reads and seek planning

**Files:**

- Modify: `crates/clipline-mp4/tests/playback_index.rs`
- Modify: `crates/clipline-mp4/src/playback_index.rs`

- [ ] Add failing tests for exact caller-buffered reads, undersized buffers, invalid track/sample
      indices, short underlying reads, and proof that index construction reads only bounded metadata.
- [ ] Implement `read_sample_into` using one caller-owned slice and exact `seek`/`read_exact`; never
      allocate a whole-file, whole-track, or sample-sized internal buffer.
- [ ] Add failing seek tests for target zero, mid-GOP, exact sync boundaries, after-end clamping,
      missing sync samples, selected audio tracks with different edit-list starts, and duplicate or
      non-audio selections.
- [ ] Return the video sync sample at-or-before the clamped target plus, for each selected Opus
      track, the bounded decode range from the video restart point through the packet covering the
      target. Use integer timescale conversion, not floating-point sample comparisons.
- [ ] Ensure every reported range is ordered, in-bounds, and sufficient for a generation-cancelable
      decoder to discard forward to the requested presentation time.

## Task 4: Add a production-writer playback oracle

**Files:**

- Create: `crates/clipline-mp4/tests/production_playback_fixture.rs`
- Create: `crates/clipline-mp4/examples/generate_production_playback_fixture.rs`
- Create: `fixtures/playback/hybrid-writer-h264-two-opus-5s.mp4`
- Modify: `fixtures/playback/README.md`
- Modify: `fixtures/playback/manifest.json`
- Modify: `.gitignore`

- [ ] Remux the existing first-party H.264 High + two-track Opus decoder oracle through the public
      `HybridMp4Writer` path. FFmpeg may remain the source encoder but cannot be the final muxer.
- [ ] Check in the exact output and a regression that regenerates it byte-for-byte, reports one
      H.264 video plus two Opus audio tracks, and opens it through the playback index.
- [ ] Record SHA-256, size, source hash, writer provenance, and `production_mux_oracle: true` in the
      fixture manifest. Keep the generator independent of FFmpeg and GPL components.
- [ ] Validate the checked-in artifact with both the native parser and the existing external
      full-decode validation when the reviewed FFmpeg executable is available.

## Task 5: Verify, review, and hand off

**Files:**

- Modify: `handoff.md`
- Modify: `docs/slint/parity-ledger.md` if Milestone 2 rows advance

- [ ] Run `cargo test -p clipline-mp4` and a fresh-cache
      `cargo clippy -p clipline-mp4 --all-targets -- -D warnings`.
- [ ] Run `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Have an independent reviewer audit parser reuse, integer conversions, seek semantics,
      bounded allocation/read behavior, and hostile-file rejection.
- [ ] Update the handoff with the stable API, artifact hash, tests, deliberate `ctts` limitation,
      and the Milestone 3 headless-playback entry point.
- [ ] Commit the plan before implementation, then commit implementation in logical conventional
      slices and push the branch.
