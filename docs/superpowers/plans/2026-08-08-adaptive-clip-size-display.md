# Adaptive Clip Size Display Plan

**Goal:** Show large clip sizes in GB instead of four-digit MB values.

## Product decision

- Keep MB for clips smaller than 1 GB.
- Switch to GB at 1 GB and round to the nearest tenth, so `1559.7 MB` displays as `1.5 GB`.
- Apply the same formatting to Library cards and Review metadata.

## Minimal implementation

Reuse the existing `PlayerCore.fmtBytes` formatter through a small megabytes adapter. Do not add a
new unit system or dependency.

## Plan-driven implementation

### Task 1: Lock the reported behavior

- [ ] Add a formatter test for a `1559.7 MB` clip.

### Task 2: Apply and verify

- [ ] Replace forced-MB Library and Review labels with the shared formatter.
- [ ] Run focused tests, the workspace suite, and warning-denied Clippy.
- [ ] Update `handoff.md`, commit, and relaunch Clipline.
