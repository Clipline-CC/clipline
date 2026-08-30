# Groups PR review follow-up

## Goal

Make group order and compilation identity durable, failure-atomic, and correct across Windows path
spellings and app restarts, while restoring a non-destructive exit from a group.

## Backend TDD

- [ ] Replace neighbor moves with one `reorder_group(name, ordered_paths)` command; add failing
      tests for verbatim/non-verbatim Windows path equivalence, duplicates, missing members, and
      exact ordered renumbering.
- [ ] Add a failing rollback test for a metadata write error, then restore every already-written
      sidecar before returning the original failure.
- [ ] Add `remove_from_group(path)` and a test that clears only group metadata.
- [ ] Persist a normalized ordered-member fingerprint on compilation metadata and expose it through
      `ClipInfo`; remove the format-version field that tried to stand in for content identity.
- [ ] Give every member's normalized video and audio the same declared endpoint before concat.
- [ ] Reuse the existing H.264 encoder discovery, one-deadline timeout, and cancellation state for
      group compilation fallback.

## Frontend TDD

- [ ] Replace the step loop with one ordered-path invoke for drag and Alt+Arrow.
- [ ] Make `groupCompilationClip` compare the persisted fingerprint with current ordered members;
      delete both compilation globals, invalidation helpers, and their call sites.
- [ ] Add **Remove from group** to the member context menu and keep playback on a surviving member.
- [ ] Route group chrome through the existing event/play/metadata policy functions and remove review
      rail rendering from the gallery renderer.
- [ ] Delete matching generated compilations with a group so same-name groups cannot inherit them.

## Verification

- [ ] Reproduce fresh Add-to-group → drag/Alt+Arrow without refreshing the Library.
- [ ] Verify reorder, restart, and membership changes reject stale compilations.
- [ ] Run workspace tests and warning-denied Clippy, update docs, push the PR, and watch CI.
