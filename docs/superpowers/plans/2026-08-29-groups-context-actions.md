# Groups Context Actions and Drag Polish

**Goal:** Let users right-click group-rail clips to delete them, make native row dragging read
clearly, and replace the bottom compilation toolbar with the review header's normal action icons.

## Task 1: Sidebar clip context

- [ ] Add a failing UI contract for a group-row `contextmenu` handler and the existing app-owned
      context menu restricted to Delete.
- [ ] Reuse `clipContextTarget`, `positionContextMenu`, `clip-menu-delete`, and `deleteClip` rather
      than creating another menu.
- [ ] If the current member is deleted, continue with the next/previous surviving group member;
      close review only when the group is empty.

## Task 2: Group review actions

- [ ] Add a failing contract that the bottom `group-review-actions`/Export/Upload controls are gone.
- [ ] In group mode, keep the normal header Open folder, Copy, Upload, and Delete icons visible;
      keep Rename hidden because group rename is still out of scope.
- [ ] Copy and Upload lazily create the authoritative group compilation, then reuse
      `copyClipToClipboard` and `openUploadDialog`. Delete confirms once and bulk-deletes members.

## Task 3: Drag feedback

- [ ] Replace the two-pixel target nudge with source scale/fade, an animated insertion gap, and a
      clear accent insertion line above/below the hovered row.
- [ ] Keep the existing native HTML drag pipeline and persisted backend ordering unchanged.

## Task 4: Verification

- [ ] Node syntax checks, focused UI contract, workspace tests, and warning-denied Clippy.
- [ ] Update `ddoc.md`/`handoff.md`, commit, and relaunch the rebuilt app for manual testing.
