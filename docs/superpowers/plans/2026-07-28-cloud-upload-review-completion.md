# Cloud Upload Review Completion Plan

**Goal:** Keep the active review open after a cloud upload when the local clip still
exists under an equivalent Windows path, and clearly explain the library transition
when `Delete local after upload` intentionally removes it.

**Approach:** Treat Windows clip paths as identities rather than raw strings during
library reconciliation. Extend the cloud upload command result with an explicit
`local_deleted` outcome so the frontend can distinguish intentional cleanup from a
missing clip or path-spelling mismatch. Reuse the global notice surface for the
post-delete confirmation because the review deck is hidden after the viewer closes.

## Tasks

- [ ] Add a UI contract test requiring `refreshClips` to reconcile the active clip
  with `PlayerCore.sameClipPath`.
- [ ] Add a UI/backend contract test requiring cloud upload results to report
  whether local cleanup succeeded and requiring the frontend to show a global
  post-delete notice.
- [ ] Make the new tests fail before implementation.
- [ ] Replace strict active-clip path equality in `refreshClips` with the existing
  Windows-aware comparator.
- [ ] Add `local_deleted` to `CloudUploadResult`, default it to `false`, and set it
  only after `delete_uploaded_local_files` succeeds.
- [ ] After the authoritative post-upload refresh, show a transient global notice
  when the reviewed clip was intentionally deleted locally.
- [ ] Run focused tests, `cargo test --workspace`, and
  `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Update `handoff.md`, rebuild, and relaunch Clipline for manual verification.

## Manual verification

- [ ] Export a trim, open it immediately, upload it with local deletion disabled,
  and confirm the review stays open.
- [ ] Upload the active clip with local deletion enabled and confirm the app returns
  to the Library with a visible “local copy deleted” confirmation.
- [ ] Confirm failed cloud processing or failed local cleanup preserves the local
  clip and does not claim it was deleted.
