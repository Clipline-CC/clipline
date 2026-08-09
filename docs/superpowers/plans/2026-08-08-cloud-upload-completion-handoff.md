# Cloud Upload Completion Handoff Plan

**Goal:** Make a completed Cloud upload immediately useful when Clipline removes the local copy.

## Product decisions

- Automatically copy the canonical share URL for every completed public or unlisted upload.
- Do not invent a URL for private uploads; private records intentionally have no share link.
- When the backend confirms that the local copy was deleted, close Review and switch the Library
  to its Cloud tab.
- Never redirect merely because deletion was requested: a failed cleanup keeps the user and local
  clip in place.
- Clipboard failure must not turn a successful upload into a failed upload. Surface it separately
  while preserving the upload result.

## Minimal implementation

Reuse `CloudUploadResult.record.remote_url`, `CloudCore.shareUrl`, and `local_deleted` in the
existing `uploadClipToCloud` completion path. No backend command, new event, setting, or navigation
abstraction is needed.

## Plan-driven implementation

### Task 1: Lock the completion behavior

- [ ] Add a UI contract asserting that completed shareable uploads write the canonical URL.
- [ ] Assert that Cloud navigation is gated by confirmed local deletion and completed status.

### Task 2: Implement and verify

- [ ] Copy the share URL without disturbing private uploads.
- [ ] Include copy success or failure in the existing completion feedback.
- [ ] Close Review and select Cloud only when `local_deleted` is true.
- [ ] Run focused tests, the workspace suite, and warning-denied Clippy.
- [ ] Update `handoff.md`, commit, and relaunch Clipline.
