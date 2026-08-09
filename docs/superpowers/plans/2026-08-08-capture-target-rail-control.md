# Capture Target Rail Control Plan

**Goal:** Replace the redundant replay-buffer button and separate game icon with one clickable
capture-target icon that communicates both the source and replay-buffer state.

## Interaction

- Show the active game's icon while a supported/custom game is captured.
- Otherwise show a compact monitor or selected-region glyph matching the saved capture target.
- Give the icon a blue ring/glow while the replay buffer is active, and darken it while off.
- Clicking the icon uses the existing replay-buffer start/stop action; quota blocking remains
  unchanged and accessible names/tooltips describe the current action.

## Plan-driven implementation

### Task 1: Lock the UI contract

- [ ] Add a failing UI-contract test for the unified button, fallback glyphs, active/off styling,
      and removal of the old buffer button.

### Task 2: Unify the control

- [ ] Make the existing game-icon host the replay-buffer button and remove the redundant rail row.
- [ ] Render monitor/region fallbacks when no game is active, retaining custom/plugin icons.
- [ ] Move buffer state, tooltip, accessibility, and click/disabled behavior onto the unified icon.

### Task 3: Verify and hand off

- [ ] Run the focused UI contract, JavaScript syntax checks, workspace tests, and warning-denied
      Clippy.
- [ ] Update `handoff.md`, commit, rebuild, and relaunch Clipline.
