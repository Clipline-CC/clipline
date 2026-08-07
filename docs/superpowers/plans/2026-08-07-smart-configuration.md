# Smart First-Run Configuration Plan

**Goal:** Add a one-click `Set this up for me` path to the first-run wizard that inspects this PC, fills the existing wizard with conservative recommendations, explains the important choices, and saves nothing until the user confirms the existing Review page.

## Product decision

Build a small, deterministic recommendation engine, not an AI model or a GPU-name database.
Clipline already probes the facts that most directly predict a reliable setup: usable encoders,
displays, audio devices, and installed games. Add free space for the selected media volume, apply a
documented rule table, and keep every recommendation editable in the existing wizard.

Include a short, bounded calibration through Clipline's real video path. The existing encoder probe
only proves that an encoder can open and complete a 640x360 frame; calibration must instead run the
selected capture backend, scaling/color conversion, and actual Automatic H.264 encoder at candidate
resolution/FPS combinations. Encoded packets are discarded before the replay ring or muxer, audio
is never opened, and no test clip is written.

This is still a capacity test, not a promise about every game. Run it with a conservative headroom
threshold and tell users that rerunning it from Settings while their usual game is open produces the
most representative result.

## Research findings

- [OBS's Auto-Configuration Wizard](https://obsproject.com/kb/quick-start-guide) runs on first
  launch, can be rerun later, and considers the user's goal, hardware resources, and network. OBS
  still recommends making a real test recording. Its separate
  [hardware-encoding guidance](https://obsproject.com/kb/hardware-encoding) recommends a hardware
  encoder for low performance impact and ties bitrate to resolution and FPS. In the
  [wizard implementation](https://github.com/obsproject/obs-studio/blob/master/frontend/wizards/AutoConfigTestPage.cpp),
  OBS runs candidate software-encoding profiles for five seconds and accepts profiles with at most
  ten skipped frames; its hardware path relies more heavily on capability and data-rate heuristics.
- [AMD Software's recording setup](https://www.amd.com/en/resources/support-articles/faqs/DH3-023.html)
  puts common choices in a wizard, leaves advanced controls for later, uses understandable quality
  presets, and limits options according to GPU capability and resolution. It also makes storage
  cost visible and lets the wizard be rerun.
- [SteelSeries Moments](https://support.steelseries.com/hc/en-us/articles/360060558211-How-does-Moments-Capture-Work)
  scans installed launchers, begins capture for recognized games, and uses an automatic capture mode
  with a compatibility fallback. The useful pattern is capability-first fallback, not asking a new
  user to choose a backend.
- [NVIDIA App](https://www.nvidia.com/en-us/software/nvidia-app/) personalizes game settings using
  GPU, CPU, and display data backed by NVIDIA's cloud database, and uses 30 seconds as the simple
  Instant Replay starting point. Clipline should borrow the one-click and explainable-review UX,
  but not attempt to recreate NVIDIA's vendor-scale cloud database.

## Approved v1 recommendation rules

### Basics

| Setting | Recommendation | Reason |
|---|---|---|
| Replay hotkey | `Alt+F10` | Keep the established Clipline default; hardware cannot make this choice better. |
| Media folder | Existing default or current wizard choice | Never guess another drive or create an unexpected location. |
| Saved-clip quota | 10 GB when free space is at least 20 GiB; otherwise `max(1, floor(free GiB / 2))` GB | Preserve the approved default while leaving headroom on a constrained drive. If free space cannot be read, keep 10 GB and show a warning. |
| Launch on startup | On | Preserve the approved first-run behavior so replay capture is available without manual launch. |

### Capture and recording

Use these only as safe fallbacks when calibration is canceled or cannot start:

| Fact | Output size | Quality | FPS | Encoder |
|---|---:|---|---:|---|
| A verified hardware H.264 option exists | Up to 1080p | Balanced | 60 | Automatic |
| No verified hardware H.264 option exists | Up to 720p | Balanced | 30 | Automatic |

When calibration runs, test the real Automatic H.264 video path in this order and stop at the first
passing profile:

1. 1440p60 only when the primary display is at least 1440p and a hardware H.264 encoder was verified.
2. 1080p60.
3. 720p60.
4. 720p30 as the final conservative fallback.

Each candidate gets a one-second warm-up and four measured seconds, with a hard timeout and
cancellation. A candidate passes only when it processes at least 97% of its expected cadence and
the 95th-percentile encode-and-conversion time stays below 75% of the frame interval. The unused 25%
is intentional game-load headroom; validate these thresholds on the dev hardware matrix before
release rather than treating them as universal constants.

The ordering deliberately favors 60 FPS for game clips over 1080p30. Quality remains Balanced:
stress testing can measure throughput, but it cannot decide the user's preferred visual quality or
storage tradeoff.

Additional rules:

- Keep capture backend and video encoder on `Automatic`. The existing ranking already prefers
  compatible H.264 hardware and falls back safely; pinning a vendor backend would make recovery
  worse after driver or hardware changes.
- Do not automatically choose 90 FPS, 120 FPS, HEVC, or AV1. H.264 is the only codec Clipline can
  always play in WebView2, and the first calibration does not need a high-refresh branch.
- Capture the primary display with the automatic backend.
- Enable default output audio only when an output device exists. Keep app-track splitting off.
- Keep microphone recording off even when a microphone exists. Device presence is not consent.
- Use a 30-second replay in memory. At the planned Balanced presets this remains within Clipline's
  64 MiB minimum buffer estimate, so disk replay and continuous cache writes are unnecessary.
- Enable automatic game detection and pause when no enabled game is open.

### Games

- Keep the built-in game integrations enabled.
- Run the existing installed-game detector as part of the one-click analysis.
- Select and stage every detected game for addition. List them on Review and preserve Back so the
  user can remove any false positive before saving.
- If detection fails, keep the supported-game setup, show the failure, and still allow the user to
  accept the remaining recommendations or use the manual Games page.

## First-run UX

1. Add a prominent card near the top of Basics: `Want the easy setup?` with a
   `Set this up for me` button, an `about 10-20 seconds` expectation, and a local-only privacy note.
2. On click, show truthful stages from the backend: checking devices, testing a named profile such
   as `1080p at 60 FPS`, and detecting games. Include Cancel.
3. State before testing that Clipline briefly captures the selected display into memory, records no
   audio, saves no test video, and uploads nothing.
4. Analyze without mutating or saving settings. On success, fill the existing wizard controls, stage
   detected games, and open Review.
5. Add a compact `Recommended for this PC` summary above the existing review grid with the three or
   four material reasons: encoder/profile, replay memory, storage quota, and detected games.
6. Keep `Back` fully functional. A user can inspect or change every recommendation on Basics,
   Capture + recording, and Games.
7. Save only through the existing `Start Clipline` path. Cancellation or test failure uses the safe
   capability fallback, while a fatal analysis failure leaves the manual wizard
   usable and does not change the settings draft.

The button will also appear when the real wizard is replayed from Settings > Misc because that path
reuses the same UI. It must still produce a patch for wizard-owned fields rather than replacing the
whole `AppSettings`, so niche settings are not reset on an existing installation.

## Minimal architecture

- Add `apps/clipline-app/src/smart_config.rs` with serializable `SmartConfigFacts`,
  `CalibrationResult`, `SmartConfigPatch`, `RecommendationReason`, and one pure
  `recommend(facts, calibration)` function.
- The patch covers only fields owned by the first-run wizard. It is not a second settings model and
  never writes to disk.
- Add one asynchronous Tauri command, `recommend_first_run_settings`, accepting the selected media
  folder and existing custom games. It reuses display enumeration, audio enumeration, verified
  encoder options, game discovery, and `windows::available_space_bytes`, plus reports progress using
  the existing app-event pattern.
- Put the bounded calibration beside the recorder service so it reuses `open_screen_capture`,
  `CadencedCapture`, output-dimension calculation, and `build_encoder`. It stops before audio,
  replay storage, muxing, or filesystem writes.
- Return the patch, detected game candidates, reasons, and non-fatal warnings in one response. No
  hardware identifiers or facts leave the PC, and no network request is made.
- Keep rule decisions in Rust so table-driven tests run on Windows and CI without browser or device
  dependencies. JavaScript only applies the returned patch to the existing controls and renders the
  explanation.

Do not collect CPU model, GPU model, or RAM merely to make the feature appear smarter. Those values
do not currently map to a measured Clipline capability decision. The verified encoder result is a
stronger signal and avoids a brittle device-name allowlist.

## Plan-driven implementation

### Task 1: Pure recommendation rules

- [ ] Add failing table-driven tests for hardware H.264, software fallback, 4K display capping, no
  audio output, constrained disk, unknown disk space, each calibration result, and
  deterministic/idempotent output.
- [ ] Implement the smallest pure rule function that makes those fixtures pass.
- [ ] Assert that microphone, app-track splitting, disk replay, advanced recording, explicit codecs,
  and above-60-FPS presets are never enabled by v1 recommendations.

### Task 2: Bounded video calibration

- [ ] Add failing neutral tests for candidate ordering, warm-up exclusion, cadence percentage,
  percentile calculation, timeouts, and cancellation.
- [ ] Add injected capture/encoder tests proving packets are discarded and no audio, replay storage,
  muxer, or file writer is created.
- [ ] Implement the benchmark by reusing the production video path, return the actual encoder and
  capture backend labels, and guarantee cleanup on pass, failure, timeout, cancellation, and window
  close.
- [ ] Prevent simultaneous recorder and calibration ownership. First run is already stopped; when
  replayed from Settings, require explicit confirmation that the current replay buffer will be
  cleared, stop recording, run calibration, and restore the prior recording request afterward.

### Task 3: Local fact collection

- [ ] Add failing command-level tests with injected display/audio/encoder/storage/game results; do
  not make unit tests depend on real hardware.
- [ ] Add the asynchronous command and reuse the existing probes rather than adding a second device
  enumeration path.
- [ ] Preserve partial success: an unavailable audio device or game scan becomes a warning and a
  conservative patch, not an all-or-nothing failure.
- [ ] Register the command in the Tauri invoke handler and keep all new Windows calls behind the
  existing safe wrappers.

### Task 4: One-click wizard flow

- [ ] Add failing UI-contract assertions for the button, local-only capture copy, progress stages,
  Cancel, review explanation, and the absence of a direct `save_settings` call from analysis.
- [ ] Add the Basics recommendation card and accessible progress/`aria-live` status.
- [ ] Apply a successful patch through the existing first-run form controls, stage all detected
  games, refresh dependent labels, and navigate to Review.
- [ ] Leave the form untouched on a fatal command error and restore the button so manual setup still
  works.

### Task 5: Review and editing safety

- [ ] Add tests proving Back retains the recommended values and lets individual detected games be
  deselected before finish.
- [ ] Render concise reasons and warnings without duplicating every Review row.
- [ ] Verify replaying the wizard from Settings changes only wizard-owned fields and persists nothing
  until `Start Clipline` succeeds.

### Task 6: Verification and handoff

- [ ] Run focused smart-config tests and JavaScript syntax checks.
- [ ] Run `cargo test --workspace`.
- [ ] Run `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Update `handoff.md` with the rule table and known limitations.
- [ ] Launch with isolated app data and manually test a verified-hardware path, forced fallback
  fixture, benchmark pass/fail/cancel/timeout, no test file/audio, low-space warning, game-detection
  failure, Back/customize, Settings rerun recorder restoration, and final save/start.

## Deferred until measurements justify it

- GPU/CPU model databases, cloud recommendations, or machine learning.
- Automatic 4K, 90/120 FPS, HEVC, or AV1 selection.
- Battery-aware profiles and display-refresh-rate tuning.
- Silent runtime quality changes. A future runtime advisor may offer an explicit downgrade after
  measured dropped frames, but it must explain and confirm the change.
