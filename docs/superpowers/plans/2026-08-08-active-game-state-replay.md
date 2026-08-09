# Active Game State Replay Plan

**Goal:** Keep the captured-game icon and game status visible after Clipline recreates its UI while
the same game remains active.

## Root cause

Game detection emits only when the detected game changes. A recreated UI receives durable recorder
and quota state through `frontend_ready`, but not the already-known active game, so its sidebar
starts with no game state and no later event is emitted for an unchanged game.

## Plan-driven implementation

### Task 1: Reproduce the missing replay

- [ ] Add a runtime-state regression test proving the current detected game can be reconstructed as
      a frontend `GameDetectionEvent`.
- [ ] Confirm the test fails before the replay accessor exists.

### Task 2: Replay the existing state

- [ ] Add the smallest runtime accessor for the current game-detection event.
- [ ] Emit it from `frontend_ready` beside the existing durable recorder and quota state.

### Task 3: Verify and hand off

- [ ] Run the focused test, workspace tests, and warning-denied Clippy.
- [ ] Update `handoff.md`, commit, rebuild, and relaunch Clipline.
