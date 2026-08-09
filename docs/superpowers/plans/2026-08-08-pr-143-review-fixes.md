# PR #143 Review Fixes Plan

## Goal

Close the user-visible correctness gaps found in review without expanding the pull request into a broad architecture rewrite.

## Decisions

- Keep the existing storage-quota policy that stops recording and replay capture together; this is the previously approved non-destructive quota behavior.
- Preserve automatic Cloud-link copying, but perform it through the native clipboard so background uploads do not become false failures.
- Allow a disabled built-in game to be captured by an intentional custom rule. Prevent only exact duplicate custom rules.
- Defer generalized facets, hotkey normalization, and service-state refactors unless a focused fix directly removes duplicated work.

## Tasks

1. Add failing runtime and UI-contract coverage for quota-blocked manual starts, stopping a manual session back into games-only waiting, optimistic Record state, and a non-glowing waiting capture target.
2. Fix the manual recording transitions and make the UI reflect the accepted start immediately.
3. Memoize stable rail/game-type rendering and remove dead indicator styles.
4. Add failing settings/detection coverage for distinct same-executable custom rules and disabled-plugin custom takeover, then narrow dedupe to the full match tuple.
5. Add failing League tests for negative queue IDs and retryable/non-blocking queue lookup, then detach bounded LCU retries from the event poll loop.
6. Add failing Cloud UI/native clipboard coverage, then scope Review/Cloud navigation to the uploaded clip and copy links through the native clipboard.
7. Run focused tests, JavaScript syntax checks, the workspace suite, and warning-denied Clippy; update `handoff.md`, push the branch, and relaunch Clipline.
