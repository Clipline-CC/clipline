# PR 130 Review Follow-ups

**Goal:** Address all three unresolved review threads on PR #130 without
changing the normal-copy or Shift+click product behavior.

## Task 1: Predictable H.264 transcode output

- [ ] Add failing tests proving every supported FFmpeg H.264 backend receives
  explicit, known-good rate-control settings.
- [ ] Carry `EncoderBackend` through the share-video export mode.
- [ ] Reuse the recorder's per-backend FFmpeg rate-control argument builder at
  an 8 Mbps target and 16 Mbps buffer.
- [ ] Bump the share cache namespace so any locally generated
  default-settings transcodes are not reused.

## Task 2: One timeout across fallback attempts

- [ ] Add a failing pure deadline-budget regression.
- [ ] Compute one export deadline before entering the encoder fallback loop.
- [ ] Pass only the remaining duration to each attempt and stop once the
  shared budget is exhausted.

## Task 3: Prune abandoned unique intermediates

- [ ] Add a failing regression using the actual nested
  `sibling_tmp_path` naming shape.
- [ ] Recognize only valid `share-export-*.mp4.<pid>.<counter>.tmp` chains,
  retaining malformed or unrelated files.
- [ ] Keep age-gated deletion and successful-path cleanup unchanged.

## Task 4: Verify and publish

- [ ] Run focused library and UI contract tests.
- [ ] Run `cargo test --workspace`.
- [ ] Run `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Update `handoff.md`, commit, push the PR branch, and relaunch Clipline.
