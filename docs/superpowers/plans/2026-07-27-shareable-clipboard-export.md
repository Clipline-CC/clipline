# Shareable Clipboard Export Implementation Plan

**Goal:** Make the review Copy button place a broadly compatible H.264/AAC-LC
MP4 on the clipboard, while Shift+click copies the untouched source MP4.

**Architecture:** Keep Clipline's recording and review files unchanged. Reuse
the existing native selected-track remux/mix to produce one Opus track, then
transcode only that audio to AAC-LC with the separately spawned bundled
FFmpeg process. Stream-copy H.264 video; when the source is HEVC or AV1,
select a proven FFmpeg H.264 encoder. Cache only the final compatible MP4
under a new cache namespace so old Opus share exports are never reused.

## Task 1: Pin the interaction and export contracts

- [ ] Add UI contract coverage for:
  - normal click forwarding `original: false` and the current selected audio;
  - Shift+click forwarding `original: true`;
  - progress and success text distinguishing shareable and original copies;
  - a tooltip that documents Shift+click.
- [ ] Add library tests proving:
  - an original request returns the exact source path without exporting;
  - normal share requests use a new cache namespace;
  - the FFmpeg argument builder emits one AAC-LC track and stream-copies H.264;
  - non-H.264 sources request H.264 encoding;
  - muted exports contain no audio map.

## Task 2: Implement share-compatible export

- [ ] Extend `CopyClipToClipboardRequest` with a default-false `original`
  field.
- [ ] Split original-path selection from normal share export selection.
- [ ] For a normal copy, remux/mix the selected audio into a unique
  intermediate MP4, invoke FFmpeg to emit AAC-LC, and atomically publish the
  final cached export.
- [ ] Add a bounded FFmpeg invocation with console suppression, captured
  diagnostics, and cleanup on every failure.
- [ ] Stream-copy H.264 video. For HEVC/AV1, probe FFmpeg's usable H.264
  encoders and try them in merit order.
- [ ] Change the cache namespace from `share-export-v1` to a compatibility
  version so cached Opus files cannot survive the behavior change.

## Task 3: Wire the modifier-aware UI

- [ ] Pass the click event into `copyClipToClipboard`.
- [ ] Normal click sends selected audio and `original: false`.
- [ ] Shift+click sends `audioTrackIds: null` and `original: true`.
- [ ] Show `preparing shareable clip...` while normal export runs and distinct
  transient success messages for both paths.
- [ ] Update the Copy button tooltip.

## Task 4: Verify and hand off

- [ ] Run focused library, MP4, and UI contract tests.
- [ ] Run `cargo test --workspace`.
- [ ] Run `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Update `handoff.md`.
- [ ] Launch `cargo run -p clipline-app` and manually verify:
  - normal copy pastes an H.264/AAC-LC MP4 into X;
  - selected output/microphone audio is audible;
  - repeat copy reuses the cache;
  - Shift+click pastes the byte-identical original multitrack MP4.
