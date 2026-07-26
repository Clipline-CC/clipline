# PR #107 review follow-ups

**Goal:** Close the lifecycle correctness and large-library performance gaps found in review,
address the unresolved inline threads, and keep the stacked PR's memory bounds intact.

**Base:** PR #107 (`pr-memory-gaps`) stacked on PR #106 (`memory-optimization`).

## Task 1: Make foreground boot and restore failure-tolerant

**Files:**
- Modify: `apps/clipline-app/src/app.rs`
- Modify: directly related lifecycle tests

- [ ] Add failing regressions for foreground publication when native reveal operations fail.
- [ ] Publish an authoritative foreground lifecycle snapshot independently of fallible
      `show`/`unminimize` calls while preserving controller-memory ordering.
- [ ] Add event coverage for minimize and restore paths that do not deliver focus transitions.
- [ ] Verify a visible native window cannot remain paired with a hidden WebView controller.

## Task 2: Keep large-library rendering bounded

**Files:**
- Modify: `apps/clipline-app/ui/library.js`
- Modify: `apps/clipline-app/ui/gallery-window-core.js`
- Modify: `apps/clipline-app/ui/gallery-window-core.mjs`
- Modify: related DOM-free and UI contract tests

- [ ] Add a regression that poster pruning builds one normalized path set instead of rescanning the
      complete library for every cache entry.
- [ ] Remove the remaining per-poster full-library scans.
- [ ] Replace full-library gallery hashing with bounded invalidation inputs while preserving page
      reset behavior for meaningful result changes.
- [ ] Give negative poster entries a bounded retry/expiry path.
- [ ] Export `GalleryWindowCore` explicitly through `globalThis`.

## Task 3: Make backend caches and diagnostics recoverable

**Files:**
- Modify: `apps/clipline-app/src/poster.rs`
- Modify: related tests

- [ ] Cache successful FFmpeg discovery without permanently caching absence.
- [ ] Verify a later successful discovery remains possible after an initial miss.
- [ ] Confirm upload-source mutation attempts produce an intentional busy/uploading message rather
      than an opaque sharing-violation error.

## Task 4: Reconcile Media Foundation feedback with the platform contract

**Files:**
- Modify if required: `crates/clipline-capture/src/windows/mft.rs`
- Modify: related tests

- [ ] Check the current Microsoft contract and real inbox transform behavior for
      `MFT_MESSAGE_COMMAND_DRAIN`; do not replace the stream ID with zero unless primary evidence
      supports it.
- [ ] Reuse caller-owned output samples across synchronous `ProcessOutput` calls where stream-info
      changes permit it.
- [ ] Document that software-MFT frame-duration assignment intentionally matches the hardware path.

## Task 5: Clean up safe mechanical review findings

**Files:**
- Modify: `crates/clipline-buffer/src/planning.rs`
- Modify: `scripts/measure-save-replay-memory.ps1`

- [ ] Remove the unreachable eviction branch.
- [ ] Make unsupported process-memory counter collection fail explicitly instead of reporting zero.
- [ ] Run the PowerShell parser and focused regressions.

## Task 6: Validate and publish

- [ ] Run focused lifecycle, UI, buffer, poster, and MFT tests.
- [ ] Run `cargo test --workspace`.
- [ ] Run fresh-cache `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Run `cargo fmt --all -- --check`, JavaScript syntax checks, PowerShell parser, and
      `git diff --check`.
- [ ] Commit the review fixes and push `pr-memory-gaps`.
- [ ] Report addressed and intentionally-open threads; do not reply to or resolve GitHub threads
      without explicit authorization.
