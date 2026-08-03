# PR #133 Review Follow-up

## Goal

Address the actionable Codex review finding without changing the valid plan-first commit history or
broadening the staggered-audio mixer.

## Test-Driven Steps

1. Extend the decoded-audio regression to prove a 960-tick Opus packet paired with a 480-tick MP4
   sample duration is rejected instead of truncated.
2. Accept only the existing ±1-tick MP4 duration quantization before normalizing decoded PCM.
3. Run the focused regression, complete `clipline-mp4` suite, full workspace tests, formatting, and
   warning-denied workspace Clippy.
4. Update `handoff.md`, commit, and push the follow-up to PR #133.

## Review Disposition

- The duration-mismatch thread is actionable and receives the validation above.
- The commit-history thread needs explanation only: each plan already precedes its implementation
  in a separate commit, preserving the repository's review, bisect, and rollback boundaries.
