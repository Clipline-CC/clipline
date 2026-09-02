# Groups Player View TDD Plan

**Goal:** Make a group the only top-level Library item for its member clips, open it in the normal
review player as an ordered playlist, reorder members by drag-and-drop in the reused Match events
rail, and render a poster mosaic that reads as one compilation.

**Architecture:** Keep the existing per-clip group metadata and backend order command. The group
review is frontend playlist state over the existing `<video>` element: selecting a rail row loads
that member, and `ended` advances to the next member. No preview compilation or second player is
created. Export/upload continue to call the existing authoritative `export_group` path. The group
card owns up to four normal lazy poster requests in an asymmetric CSS mosaic.

## Task 1: Top-level Library ownership

- [ ] Add a failing UI contract that grouped clips are excluded from gallery clip buckets while
      remaining in `clipsCache` for group membership and playback.
- [ ] Keep group cards visible/searchable and update Library counts/empty states around top-level
      items rather than rendering member clips twice.
- [ ] After Add to group succeeds, make the deck action say **Open group** and open the resulting
      group instead of the hidden member clip.

## Task 2: Review-player group mode

- [ ] Add a failing contract for group playlist state, player reuse, automatic next-member
      playback, group header/summary, and group export/upload controls.
- [ ] Delete the standalone group-view dialog.
- [ ] Open the first group member in the existing review player while preserving group identity;
      selecting another member loads it in the same player and reaching `ended` advances once.
- [ ] Hide member-only rename/delete/share/trim actions while group mode is active and show the
      authoritative group export/upload actions in the deck.

## Task 3: Reused rail with drag ordering

- [ ] Render group members into `#game-event-rail` with numbered rows, poster/title/duration, and
      active-member state.
- [ ] Use native HTML drag events plus keyboard Up/Down buttons for accessible ordering. Drop calls
      the existing adjacent-move backend command repeatedly only as needed, then refreshes local
      group order and the rail.
- [ ] Keep normal game-event behavior unchanged outside group mode.

## Task 4: Group poster mosaic

- [ ] Replace the placeholder icon with one-to-four poster cells using the existing bounded poster
      loader/cache.
- [ ] Use a full-bleed first poster, two offset cutouts, a dark diagonal seam, and an overflow count
      for larger groups; retain stable gradients while posters load or are unavailable.
- [ ] Add CSS/contract coverage for the mosaic layers and member rail.

## Task 5: Verification

- [ ] Node syntax checks, focused Groups/UI tests, workspace tests, and warning-denied Clippy.
- [ ] Update `ddoc.md` and `handoff.md`, commit, stop the old app, and launch the rebuilt branch for
      manual group creation/playback/reorder/export/upload checks.
