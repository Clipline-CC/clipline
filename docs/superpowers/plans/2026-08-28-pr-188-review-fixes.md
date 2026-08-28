# PR #188 review fixes

**Goal:** Close the ownership, concurrency, Unicode, startup-sweep, and avoidable quota-work gaps found during review of PR #188 without changing the visible favorites or cleanup behavior.

**Architecture:** Keep `<clip>.clipline.json` backward-compatible as ownership proof, but let favorite-only metadata on imported MP4s opt out with `owned: false`. Serialize quota GC with clip metadata mutations through storage's existing process-wide mutex. Serialize emptied-session cleanup with session metadata writes through one process-wide session mutex, and create new clip ownership markers before writing session metadata. Keep the remaining fixes local: checked UTF-8 slicing, a per-root sweep before one-shot recovery, and an early quota return.

## Task 1: ownership remains explicit

**Files:** `crates/clipline-storage/Cargo.toml`, `crates/clipline-storage/src/lib.rs`, `apps/clipline-app/src/library.rs`

- [ ] Add failing tests proving `owned: false` metadata does not make an imported MP4 quota-managed and favorite/unfavorite never adopts it.
- [ ] Parse the optional ownership override while treating existing metadata without the field as owned.
- [ ] Preserve `owned: false` for favorite-only metadata; title/file rename clears it because those existing operations explicitly adopt imported clips.

## Task 2: serialize clip metadata mutations with GC

**Files:** `crates/clipline-storage/src/lib.rs`, `apps/clipline-app/src/library.rs`, `apps/clipline-app/src/gc.rs`

- [ ] Add a deterministic failing race test where GC passes its last favorite check while a favorite command starts.
- [ ] Expose the existing clip-mutation guard and hold it across favorite/title/file-rename read-modify-write operations.
- [ ] Revalidate that the MP4 still exists after taking the guard so a command cannot recreate metadata for a GC-deleted clip.

## Task 3: make session cleanup and metadata writes mutually exclusive

**Files:** `crates/clipline-storage/src/lib.rs`, `crates/clipline-storage/src/empty_sessions.rs`, `apps/clipline-app/src/service.rs`

- [ ] Add a failing test proving cleanup holds the session-mutation guard across metadata read, unlink, removal, and restore.
- [ ] Share one process-wide session guard between cleanup and `write_session_game_meta`.
- [ ] Reserve the replay/full-session ownership marker before writing session metadata so cleanup cannot remove a newly prepared folder.

## Task 4: local correctness and efficiency fixes

**Files:** `crates/clipline-storage/src/empty_sessions.rs`, `crates/clipline-storage/src/lib.rs`, `apps/clipline-app/src/service.rs`

- [ ] Add a Unicode sidecar-name regression and switch suffix matching to checked string slicing.
- [ ] Add a two-root recovery regression and sweep the current root before the process-global recovery guard.
- [ ] Add a callback-count regression and return before priority/protection work when inventory is already under quota.

## Task 5: docs and verification

**Files:** `handoff.md`

- [ ] Update the handoff checkpoint with the hardened ownership and concurrency behavior.
- [ ] Run `cargo test --workspace`.
- [ ] Run fresh-cache clippy for changed crates, then `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Stop an existing app process and launch `cargo run -p clipline-app` for manual testing.
