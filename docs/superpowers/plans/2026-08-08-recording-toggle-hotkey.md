# Recording Toggle Hotkey Plan

**Goal:** Let users start or stop Clipline's recorder with a configurable system-wide keybind.

## Product decisions

- Add an optional `Start / Stop recording` keybind under Settings > Hotkeys.
- Support the same F-keys, modified keyboard keys, and mouse buttons as Save Replay.
- Leave the keybind unset by default so upgrades do not claim a new system-wide shortcut.
- Reject a recording keybind that duplicates either Save Replay keybind.
- Toggle the recorder's existing desired state: an active or waiting recorder stops; a stopped
  recorder starts through the existing game-detection and storage-quota gates.
- Keep the control native-side so it works while the app window is closed to the tray.
- Do not add a second recorder state model or expose raw encoder controls.

## Minimal architecture

Persist one optional setting and extend the existing low-level hotkey dispatcher from one action
to two (`SaveReplay` and `ToggleRecording`). Route the new action through `RuntimeState`'s existing
start/stop methods so UI, tray, quota, and game-waiting behavior remain authoritative.

## Plan-driven implementation

### Task 1: Lock the contracts

- [ ] Add settings tests for the optional default, normalization, and cross-action conflicts.
- [ ] Add a hotkey dispatcher test proving each binding invokes only its own action.
- [ ] Add a UI contract for the field, persistence, capture, and optional-clear behavior.

### Task 2: Implement settings and UI

- [ ] Add the optional persisted field with backward-compatible loading.
- [ ] Add the Hotkeys settings row and include it in shared shortcut capture/conflict handling.

### Task 3: Route the native action

- [ ] Generalize the existing hook dispatcher to distinguish Save Replay from recorder toggle.
- [ ] Toggle through the existing recorder state and surface failures through the existing error
      event.
- [ ] Keep the first-run wizard from changing the optional recording keybind.

### Task 4: Verify and hand off

- [ ] Update `ddoc.md` and `handoff.md`.
- [ ] Run focused tests, JavaScript syntax checks, the workspace suite, and warning-denied Clippy.
- [ ] Commit and relaunch Clipline for manual testing.
