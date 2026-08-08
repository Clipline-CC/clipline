# PR #142 review fixes plan

**Goal:** Keep quota recovery usable and quota monitoring cheap and resilient,
while tightening the small clipboard/upload status issues found in review.

**Architecture:** A quota recheck receives an explicit `announce` flag. Manual
checks may reopen the dialog; refresh-driven checks update usage and resolve the
lock silently. The recorder reuses its one startup inventory total as a baseline
and checks only the active full-session file size on each status tick. Filesystem
inspection failures warn and skip that check instead of terminating capture.

## Scope

- [ ] Add failing UI contracts proving background rechecks are silent, manual
      rechecks announce, quota resolution restores the requested rail state, and
      expected clipboard cancellation does not surface as an error.
- [ ] Add failing service tests for deriving full-session quota usage from a
      cached baseline plus the active file size.
- [ ] Thread `announce` through the quota recheck command and update blocked
      usage text without reopening the modal during Library refreshes.
- [ ] Replace the once-per-second media-tree scan with cached baseline accounting
      plus one active-file metadata read.
- [ ] Make startup, replay-save, and post-save quota inspection failures
      warn-and-continue rather than stop the recorder.
- [ ] Clarify upload completion notices, swallow expected superseded clipboard
      cancellation, remove the disabled Save Replay click branch, and reuse the
      computed clipboard success message.
- [ ] Add prominent upgrade notes: existing quotas become recording locks, no
      saved clips are auto-deleted, and short osu! sessions are retained.
- [ ] Run focused tests, `cargo test --workspace`, and
      `cargo clippy --workspace --all-targets -- -D warnings`.

## Non-goals

- No background quota-index service or persistent usage database.
- No restoration of automatic cleanup, including the old osu! short-session
  discard; preservation is the product requirement.
- No PR restacking: PR #141 is already merged and its merge commit is the current
  `develop` base for PR #142.
