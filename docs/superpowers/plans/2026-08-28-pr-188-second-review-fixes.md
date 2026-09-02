# PR #188 second-review fixes

**Goal:** Close the remaining stale-cleanup, rename-adoption, IPC-blocking, and quota-explanation gaps found in the second review of PR #188.

**Architecture:** Keep cleanup conservative for arbitrary user files, but recognize Clipline's own interrupted sidecar writes and expire orphan ownership markers after a fixed grace period. Preserve the existing cleanup race re-check and restore protocol while surfacing restore failures. Move favorite mutation work off Tauri's IPC thread, clear the ownership override after file rename reads the moved metadata, and explain that favorites are quota-protected instead of deleting other clips when protected bytes alone exceed quota.

## Task 1: retry stale Clipline cleanup safely

**Files:** `crates/clipline-storage/src/empty_sessions.rs`

- [ ] Add failing regressions for old orphan ownership markers and interrupted sidecar temp/backup names.
- [ ] Keep fresh markers and arbitrary unrecognized files, but classify stale Clipline-owned debris as disposable.
- [ ] Add a failing restore-write regression and propagate that write error.

## Task 2: preserve adoption and IPC behavior

**Files:** `apps/clipline-app/src/library.rs`

- [ ] Add a failing regression proving file rename adopts a favorite-only imported MP4.
- [ ] Clear `owned: false` after reading the moved target metadata.
- [ ] Run favorite metadata mutation through `spawn_blocking` so a quota GC lock cannot stall Tauri's IPC thread.

## Task 3: make quota blocking explicit

**Files:** `apps/clipline-app/src/gc.rs`, `apps/clipline-app/ui/index.html`, `apps/clipline-app/tests/ui_contract.rs`

- [ ] Pin the intentional no-deletion result when protected favorites alone exceed quota.
- [ ] Tell users that favorites are protected and may need to be unfavorited or paired with a larger quota.

## Task 4: docs and verification

**Files:** `crates/clipline-storage/src/empty_sessions.rs`, `apps/clipline-app/src/service.rs`, `handoff.md`

- [ ] Correct the cleanup protocol description and document the ownership-before-session-metadata invariant.
- [ ] Update the PR description and handoff checkpoint.
- [ ] Run `cargo test --workspace` and fresh-cache clippy for changed crates.
- [ ] Push the fixes, verify CI, and reopen `clipline-app` for manual testing.
