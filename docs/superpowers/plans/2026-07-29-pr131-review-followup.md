# PR #131 Review Follow-up Plan

**Goal:** Preserve the actionable cloud cleanup error when the authoritative
post-upload Library refresh also reports a scan warning.

**Root cause:** `uploadClipToCloud` currently writes `result.record.error` to the
global error surface before `await refresh()`. `refreshClips` owns that same surface
for Library warnings, so a scan warning can overwrite the cleanup error.

## Tasks

- [ ] Strengthen the UI contract regression to require cleanup-error publication
  after the authoritative post-upload refresh.
- [ ] Run the focused test and confirm it fails on the ordering requirement.
- [ ] Capture the backend upload error, refresh the Library, then publish the upload
  error so it remains the final actionable message.
- [ ] Run the focused test, workspace tests, and warning-denied workspace Clippy.
- [ ] Update `handoff.md`, commit the review follow-up, and push PR #131.

## Manual verification

- [ ] With a forced local-cleanup failure and a simultaneous Library scan warning,
  confirm the cleanup failure remains visible after upload completion.
- [ ] Confirm uploads without a backend error still show Library scan warnings.
