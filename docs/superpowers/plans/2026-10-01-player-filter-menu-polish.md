# Player filter menu polish

Follow-up on `feat/player-metadata-marker-filters` / PR #219.

- [ ] Extend the existing control regression before implementation: the eye
  toggles all markers off/on, the bookmark button toggles bookmarks-only/all,
  their accessible pressed states follow selection, and both menus share
  dismissal and keyboard behavior. Cover audio checkbox focus and fresh callbacks.
- [ ] Replace the three marker shortcut labels with two native icon buttons.
  Reuse the bookmark glyph; show the eye's crossed-out state when nothing is visible.
  Keep accessible names, tooltips and visible pressed styling.
- [ ] Use the same anchored popup sizing/styling for Audio tracks and Markers.
  Preserve the narrow summary hitboxes, audio selection semantics and persistence.
  Reuse existing row rendering while retaining checkboxes across selection repaint.
- [ ] Check live popup layout and keyboard/mouse interactions, run workspace tests
  and fresh-cache warning-denied Clippy, update the handoff, push PR #219 and wait
  for Ubuntu/Windows CI. Reopen Clipline for manual acceptance.

Plan checkboxes remain unticked by repository convention.
