# Recording Toggle Hotkey Plan

**Goal:** Let users start or stop a saved full-session recording with a configurable system-wide
keybind, while making replay-buffer readiness a distinct UI state.

## Product decisions

- Add an optional `Start / Stop recording` keybind under Settings > Hotkeys.
- Support the same F-keys, modified keyboard keys, and mouse buttons as Save Replay.
- Leave the keybind unset by default so upgrades do not claim a new system-wide shortcut.
- Reject a recording keybind that duplicates either Save Replay keybind.
- The action controls the full-session writer, not the replay buffer. Starting records new footage
  from that point forward; stopping finalizes the session while the replay buffer stays available.
- If capture is off or waiting for a game, an explicit manual recording starts capture and bypasses
  the games-only wait for that recording.
- Existing per-game automatic full-session recording remains supported. The action always reflects
  and controls whether a session is actually being written.
- Rework the left rail into two honest controls: `Record` / `Rec` for a saved session and `Buffer`
  for replay-buffer Off, Waiting, or Ready state.
- Both manual session start and underlying capture remain subject to the existing storage-quota
  lock.
- Keep the control native-side so it works while the app window is closed to the tray.
- Do not add a second recorder state model or expose raw encoder controls.

## Minimal architecture

Persist one optional setting and extend the existing low-level hotkey dispatcher from one action
to two (`SaveReplay` and `ToggleRecording`). Add start/stop-full-session commands to the existing
recorder loop, reusing its one encoder, full-session finalization, metadata, and quota paths.

## Plan-driven implementation

### Task 1: Lock the contracts

- [ ] Add settings tests for the optional default, normalization, and cross-action conflicts.
- [ ] Add a hotkey dispatcher test proving each binding invokes only its own action.
- [ ] Add a UI contract for the field, persistence, capture, and optional-clear behavior.
- [ ] Add recorder-loop tests for starting and stopping a full-session sink independently of the
      replay service.

### Task 2: Implement settings and UI

- [ ] Add the optional persisted field with backward-compatible loading.
- [ ] Add the Hotkeys settings row and include it in shared shortcut capture/conflict handling.

### Task 3: Route the native action

- [ ] Generalize the existing hook dispatcher to distinguish Save Replay from recorder toggle.
- [ ] Start or finish the existing full-session sink without stopping the replay buffer.
- [ ] Start capture for an explicit manual session even when the buffer is off or games-only is
      waiting, and surface failures through existing events.
- [ ] Keep the first-run wizard from changing the optional recording keybind.

### Task 4: Make rail state truthful

- [ ] Make the red `Rec` state depend only on `full_session` from backend status.
- [ ] Give replay-buffer Off, Waiting, and Ready states their own `Buffer` control.
- [ ] Route the Record button and new hotkey through the same native command.

### Task 5: Verify and hand off

- [ ] Update `ddoc.md` and `handoff.md`.
- [ ] Run focused tests, JavaScript syntax checks, the workspace suite, and warning-denied Clippy.
- [ ] Commit and relaunch Clipline for manual testing.
