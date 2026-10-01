# Trim-handle edge scrolling

Base: `develop` at `1ff7abba`. While Clip mode is active, dragging either trim
handle near/beyond a zoomed timeline edge should scroll in that direction,
including while the pointer stays still. Release stops at the selected position.

- [ ] Add failing pure edge-pan and actual drag-loop regressions. Cover left/right,
  stationary holds, re-entering the center, clip/minimum-gap bounds, snapping/Alt,
  refresh-rate independence and stopping on release/cancel/lost capture/teardown.
- [ ] Put edge-zone/rate math in DOM-free `player-core.js`, reusing `panView` and
  existing clamp helpers. Preserve zoom; cap stalled-frame elapsed time.
- [ ] Use one requestAnimationFrame loop only during a trim-handle drag. Reuse the
  pointer-to-trim and seek path after every view shift. Keep scrub/selection-slide
  behavior intact, and cancel work when the clip/mode/focus/lifecycle changes.
- [ ] Verify real mouse dragging in both directions, stationary holds, release,
  snapping/Alt and zoomed-out behavior. Keep test selections in memory.
- [ ] Run workspace tests and fresh-cache warning-denied Clippy, update the
  handoff, reopen Clipline, and create a PR into develop with green OS checks.

Plan checkboxes remain unticked by repository convention.
