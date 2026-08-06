# Settings Misc Wizard Replay Plan

**Goal:** Let users reopen the real first-time setup wizard from a new Misc settings tab.

## UI contract

- [ ] Add an accessible `Misc` settings tab and panel.
- [ ] Add a `Play first-time wizard` action without treating it as a persisted setting.
- [ ] Protect unsaved settings edits before leaving Settings.

## Implementation

- [ ] Close Settings and open the existing first-run wizard from the new action.
- [ ] Reuse the same first-run defaults and save flow as an actual first launch.

## Verification

- [ ] Run the focused UI contract test, workspace tests, and clippy.
- [ ] Launch Clipline for manual verification and update `handoff.md`.
