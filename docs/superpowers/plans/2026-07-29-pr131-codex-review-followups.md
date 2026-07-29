# PR #131 Codex Review Follow-ups Plan

**Goal:** Preserve in-flight review identity across Windows path aliases and ensure
post-upload deletion/error feedback is visible after a backgrounded window returns
to the foreground.

## Root causes

- `refreshClips` finds the active clip with Windows-aware path identity but replaces
  `currentClip` with the Library object's alternate path spelling. In-flight audio
  and rename work still holds the original spelling and includes strict guards.
- `refresh()` can return `false` when the window is backgrounded after only marking
  the Library dirty. Starting a transient deletion notice at that point lets it
  expire before the deferred foreground refresh closes the missing local review.

## Tasks

- [ ] Add a UI contract regression requiring refreshed metadata to retain the
  active `currentClip.path` spelling.
- [ ] Add lifecycle/UI contract coverage requiring post-refresh upload feedback to
  queue when refresh is deferred and flush only after a successful foreground
  refresh.
- [ ] Run the focused tests and confirm both requirements fail before implementation.
- [ ] Merge refreshed metadata into `currentClip` while preserving its active path.
- [ ] Add one bounded pending post-refresh feedback slot for upload errors and the
  deletion confirmation; publish it immediately after a completed refresh or flush
  it after the next completed foreground refresh.
- [ ] Run focused tests, `cargo test --workspace`, and fresh-cache
  `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Update `handoff.md`, commit, and push the follow-ups to PR #131.

## Manual verification

- [ ] Upload a freshly exported trim while an audio preview or rename flow is
  in-flight and confirm the flow remains current after Library reconciliation.
- [ ] Complete a delete-local upload while Clipline is hidden, wait longer than the
  normal notice duration, restore the app, and confirm the deletion notice appears
  after the Library transition.
