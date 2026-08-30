# PR #188 merge-blocker fixes

**Goal:** Close the remaining recorder/save races, abandoned-upload husks, post-delete false failures, startup recovery ordering, ownership/favorite coupling, and repeated GC policy reads found in the final review of PR #188.

**Architecture:** Keep filesystem ownership and user favorites as separate positive sidecars. Let storage own canonical sidecar derivation and session-mutation operations instead of exporting raw mutex guards. Keep clip mutation serialization in the app, where upload leases and metadata mutations can share one lock, and evaluate each clip's GC policy once. Treat session-folder cleanup after a successful MP4 deletion as best-effort while preserving a diagnostic channel for quota GC.

## Task 1: make new session files visible atomically

**Files:** `crates/clipline-storage/src/lib.rs`, `crates/clipline-storage/src/empty_sessions.rs`, `apps/clipline-app/src/service.rs`

- [ ] Add failing lock regressions for replay ownership and full-session file reservation.
- [ ] Add storage-owned operations that hold the session lock across directory creation and the first cleanup-blocking file.
- [ ] Replace the exported session mutex guard with a storage-owned session metadata writer.
- [ ] Preserve one-shot recording recovery, then sweep the recovered root so zero-byte recovery husks disappear in the same run.

## Task 2: keep upload payloads out of session folders

**Files:** `apps/clipline-app/src/cloud.rs`

- [ ] Add a regression proving a remuxed upload payload is not staged beside its source clip.
- [ ] Reserve and prune upload payloads in Clipline's process temp area so a crash cannot keep a session folder alive.

## Task 3: make successful clip deletion stay successful

**Files:** `crates/clipline-storage/src/lib.rs`, `apps/clipline-app/src/library.rs`, `apps/clipline-app/src/cloud.rs`

- [ ] Add a GC regression that simulates session cleanup failure after MP4 deletion.
- [ ] Count and report the deleted clip and freed bytes even when emptied-session cleanup fails.
- [ ] Keep single and bulk deletion successful after the MP4 is gone, logging cleanup diagnostics with the affected path.

## Task 4: separate favorites from ownership and centralize sidecars

**Files:** `crates/clipline-storage/Cargo.toml`, `crates/clipline-storage/src/lib.rs`, `apps/clipline-app/src/library.rs`, `apps/clipline-app/src/poster.rs`, `apps/clipline-app/src/osu_enrichment.rs`

- [ ] Replace `owned: false` metadata with a dedicated favorite marker sidecar so imported favorites never become managed clips.
- [ ] Preserve favorite markers across file rename and delete them with the clip.
- [ ] Remove storage's JSON dependency and ownership-schema parsing.
- [ ] Export one canonical sidecar path helper and keep the sidecar table compile-time sized.

## Task 5: evaluate GC policy once under app-owned synchronization

**Files:** `crates/clipline-storage/src/lib.rs`, `apps/clipline-app/src/gc.rs`, `apps/clipline-app/src/cloud_upload.rs`, `apps/clipline-app/src/library.rs`

- [ ] Add a callback-count regression proving one policy evaluation per inventoried clip.
- [ ] Replace the two storage callbacks with one `ClipGcPolicy` result.
- [ ] Move the clip mutation lock into the app and serialize upload lease acquisition, GC, favorite/title/file rename, and deletion through it.

## Task 6: tighten conservative cleanup and verification

**Files:** `crates/clipline-storage/src/empty_sessions.rs`, `apps/clipline-app/tests/ui_contract.rs`, `handoff.md`

- [ ] Keep evidence-free empty session-shaped directories and handle session metadata names case-insensitively.
- [ ] Remove branch-history and internal-call-shape tests that do not guard user behavior.
- [ ] Run `cargo fmt --all`, `cargo test --workspace`, and fresh-cache clippy for changed crates.
- [ ] Update `handoff.md`, stop an existing app process, and launch `cargo run -p clipline-app` for manual testing.
