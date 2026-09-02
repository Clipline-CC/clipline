# PR #188 final-review fixes

**Goal:** Close the upload-temp pruning defect, quota-dialog wording, replay attribution crash window, and meaningful concurrency-test gaps found in the final review of PR #188.

**Architecture:** Share the upload payload prefix between its writer and reaper. Write session attribution immediately after the locked replay ownership reservation and before MP4 creation. Keep quota copy accurate in both manual and auto-delete modes. Exercise the production GC lock wrapper and both session reservation operations with deterministic lock tests.

## Task 1: reap relocated upload payloads

**Files:** `apps/clipline-app/src/cloud.rs`

- [ ] Add a failing regression using the relocated `clipline-upload-*` filename.
- [ ] Share the filename prefix between reservation and pruning.
- [ ] Preserve the 24-hour age gate and unrelated-file protection.

## Task 2: close replay attribution and copy gaps

**Files:** `apps/clipline-app/src/service.rs`, `apps/clipline-app/ui/index.html`

- [ ] Write session game metadata after ownership reservation but before replay MP4 creation.
- [ ] Keep failed-save cleanup and late League queue updates intact.
- [ ] Make the favorites hint conditional in meaning so it is accurate when auto-delete is disabled.

## Task 3: strengthen race regressions

**Files:** `crates/clipline-storage/src/lib.rs`, `apps/clipline-app/src/gc.rs`

- [ ] Prove both replay and full-session reservations wait for the session cleanup lock before creating their folder.
- [ ] Pause the production GC wrapper after its favorite check and prove a concurrent favorite cannot succeed and then be deleted.
- [ ] Keep delete-helper coverage behavioral rather than restoring source-string assertions.

## Task 4: docs and verification

**Files:** `handoff.md`, PR #188 body and review thread

- [ ] Update the PR body to describe the dedicated favorite marker and current GC policy API.
- [ ] Reply to the Greptile cleanup-race thread with the lock/reservation invariant.
- [ ] Run workspace tests and warning-denied workspace Clippy, push, and verify CI.
