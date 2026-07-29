# PR #105 Greptile Review Follow-up

## Goal

Prevent a successful public or unlisted visibility update from becoming a terminal
`uploaded_public` record without the canonical server-issued share URL when the follow-up clip
detail refresh fails.

## Constraints

- Preserve the remote clip identity so the existing status-sync path can reconcile the upload.
- Never synthesize an owner-page URL.
- Keep a successful visibility response usable when it already contains a canonical `public_url`.
- Keep private uploads URL-less and terminal as designed.

## Plan

- [ ] Add a failing HTTP regression proving that a URL-less public visibility response plus a
      failed detail refresh is reported as recoverable instead of returned as ready.
- [ ] Require a canonical `public_url` before falling back to the visibility response after a
      failed refresh; retain the existing fallback when that response already includes the URL.
- [ ] Run the focused Cloud tests, workspace tests, and fresh-cache warning-denied Clippy.
- [ ] Record the review follow-up in `handoff.md`.
