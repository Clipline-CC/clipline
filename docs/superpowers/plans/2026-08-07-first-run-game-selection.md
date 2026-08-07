# First-Run Game Selection Plan

**Goal:** Make detected-game selection a single-step part of continuing through setup.

## UI contract

- [ ] Replace `Add selected games` with an accessible `Select all` checkbox and selected count.
- [ ] Keep Select all synchronized with individual selections, including its mixed state.

## Behavior

- [ ] Add checked detected games automatically when Continue leaves the Games page.
- [ ] Preserve the existing duplicate filtering and custom-game normalization.

## Verification

- [ ] Run the focused UI contract, workspace tests, and clippy.
- [ ] Relaunch Clipline for manual verification.
