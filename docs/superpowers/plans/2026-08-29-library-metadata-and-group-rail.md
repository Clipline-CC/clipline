# Library Metadata and Group Rail Layout

**Goal:** Render every local Library item's primary metadata as duration, size, modified time, and
keep group-rail text strictly to the right of its fixed thumbnail.

## TDD steps

- [ ] Add a failing UI contract for one `libraryItemMeta` formatter whose first three fields are
      duration, size, and relative modified time.
- [ ] Sum group member sizes alongside existing duration/modified aggregation and use the shared
      formatter for group cards and normal clip cards.
- [ ] Keep queue/marker context only after the three primary metadata fields.
- [ ] Add a failing CSS contract for a fixed 52px sidebar poster and a `min-width: 0` flex body.
- [ ] Remove the stray unmatched rail-rule brace and replace the row's competing generic grids with
      one explicit flex layout so title/meta cannot occupy the thumbnail area.
- [ ] Run Node syntax checks, focused UI tests, workspace tests, warning-denied Clippy, update
      handoff/design notes, commit, and relaunch.
