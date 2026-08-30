# Groups Drag and Playback Follow-up

**Goal:** Make whole-row group drag ordering work on Windows, remove visible reorder controls, and
avoid a blank stage while sequential group members change source.

## Confirmed drag cause

Tauri's native Windows drag-drop handler is enabled by default. Clipline does not consume native
`DragDropEvent`s, and that handler intercepts the gesture before WebView2 dispatches the HTML
`dragstart` used by group rows. `groupDragSourcePath` therefore stays empty and `move_group_clip`
is never invoked.

## TDD steps

- [ ] Add a failing UI contract that the main window sets `dragDropEnabled: false`, group rows keep
      native HTML `draggable`, and the rail contains no arrow buttons or drag glyph.
- [ ] Disable the unused Tauri handler, keep the entire row as the drag target, and retain hidden
      Alt+Arrow keyboard ordering on the row's single play button.
- [ ] Add a failing contract for a reusable muted preload video, next-member priming, boundary
      bridge start, and cleanup on loaded data/group exit.
- [ ] Layer the preload video behind existing transport chrome. Prime the next member while the
      current one plays; at `ended`, show/play that prepared element (or its poster) until the main
      player has decoded the next first frame.
- [ ] Keep audio authority on the existing main player/sidecar path; the bridge is visual-only.
- [ ] Run Node syntax checks, focused UI tests, workspace tests, warning-denied Clippy, update the
      design/handoff, commit, and relaunch.
