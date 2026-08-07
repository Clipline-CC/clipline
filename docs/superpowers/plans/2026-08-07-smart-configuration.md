# One-Click First-Run Configuration Plan

**Goal:** Add a `Set this up for me` action that applies the approved Clipline preset, detects and
adds every game it finds, and opens Review without saving anything early.

## Product decision

V1 is a fixed, conservative preset rather than a hardware scorer or stress test. Supported Clipline
systems are expected to have a usable hardware encoder, and the existing `Automatic` encoder and
capture-backend paths already select and fall back between usable implementations.

This keeps the one-click path fast and predictable. Hardware-specific scoring, active calibration,
and passive first-game tuning are out of scope until real usage shows that the fixed preset is not
reliable enough.

## Approved preset

### Basics

- Replay hotkey: `F6`.
- Media folder: the normal default on first launch; preserve the current wizard value when replayed
  from Settings.
- Saved-clip quota: 10 GB.
- Launch on startup: on.

### Capture and recording

- Capture target: primary display.
- Capture backend and video encoder: `Automatic` through the existing defaults.
- Output audio: on, using the default output device at 100%.
- App-track splitting: off.
- Microphone: on when an input device exists, using the default microphone at 100% in mono. If no
  input device exists, leave it off and explain that on Review.
- Pause recorder when no game is open: on.
- Replay length: 30 seconds.
- Replay storage: memory.
- Output size: 720p.
- Video quality: Balanced, which maps to the existing 5 Mbps 720p preset.
- FPS: 60.
- Advanced recording: off.

Microphone recording must be prominent on Review. The one-click action is not permission to hide
that audio is being captured.

### Games

- Enable automatic game detection.
- Keep every built-in supported-game integration enabled.
- Run the existing installed-game detector.
- Select and stage every detected game for addition, using the existing duplicate filtering and
  normalization.
- If game detection fails, keep the rest of the preset, show the warning on Review, and leave Games
  available through Back for a manual retry.

## UX

1. Add a prominent `Want the easy setup?` card near the top of Basics with a
   `Set this up for me` button and copy stating that Clipline will enable the default microphone.
2. On click, disable the button and show `Setting up Clipline...` while the existing audio-device
   and installed-game checks finish.
3. Apply the preset to the existing first-run controls. Do not create a second settings model.
4. Add all detected games and open Review.
5. Show a compact `Set up for you` summary: 720p60 Balanced, 30-second memory replay, default
   microphone state, and the number of games added.
6. Keep Back functional so every choice remains editable.
7. Save only when the user clicks the existing `Start Clipline` button.

The same action appears when the wizard is replayed from Settings > Misc. It changes only settings
represented by the wizard and must not reset niche settings that the wizard does not expose.

## Minimal implementation

No new Rust recommendation module or Tauri command is needed. Add one frontend helper in
`first-run.js` that:

- waits for the existing display and audio-device loaders;
- fills the existing wizard controls with the approved preset;
- invokes the existing `detect_installed_games` command;
- selects and adds every returned candidate with the existing helpers;
- records a small in-memory summary/warning for Review; and
- navigates to Review without calling `save_settings`.

## Plan-driven implementation

### Task 1: UI contract first

- [ ] Add failing contract assertions for the `Set this up for me` action, microphone disclosure,
  loading status, recommendation summary, and the absence of a save call in the setup helper.
- [ ] Add failing assertions for 720p, Balanced, 60 FPS, 30-second memory replay, automatic game
  detection, all detected games, and microphone-on-when-present behavior.

### Task 2: Apply the fixed preset

- [ ] Add the Basics card and accessible live loading status.
- [ ] Implement the smallest helper that writes the approved values into the existing wizard
  controls and refreshes their dependent labels.
- [ ] Reuse the existing device selections; enable the default microphone only when an input device
  is available.
- [ ] Keep the form unchanged until device loading has completed, and restore the button after an
  unexpected fatal error.

### Task 3: Detect and add games

- [ ] Refactor the existing first-run detector only as much as needed to return success/failure to
  the one-click flow.
- [ ] Select and add every detected candidate using the existing duplicate filtering and custom-game
  conversion.
- [ ] Continue to Review with a visible warning when detection fails.

### Task 4: Review and persistence safety

- [ ] Render the one-click summary and microphone state without duplicating the existing review
  grid.
- [ ] Prove Back retains the recommended values and lets the user disable the microphone or remove
  games before finishing.
- [ ] Prove the action does not call `save_settings`; persistence remains exclusively in the existing
  `Start Clipline` flow.
- [ ] Verify replaying the wizard from Settings changes only wizard-owned fields.

### Task 5: Verification and handoff

- [ ] Run the focused UI contract and JavaScript syntax checks.
- [ ] Run `cargo test --workspace`.
- [ ] Run `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Update `handoff.md`.
- [ ] Launch with isolated app data and manually test microphone present/absent, games found/none,
  game-detection failure, Back/customize, and final save/start.

## Deferred

- Encoder stress testing or first-game performance calibration.
- GPU/CPU model databases or cloud recommendations.
- Automatic 1080p/1440p/4K, 90/120 FPS, HEVC, or AV1 selection.
- Silent runtime quality changes.
