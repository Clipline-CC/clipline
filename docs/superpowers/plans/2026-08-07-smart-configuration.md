# Smart First-Run Configuration Plan

**Goal:** Add a one-click `Set this up for me` path to the first-run wizard that inspects this PC, fills the existing wizard with conservative recommendations, explains the important choices, and saves nothing until the user confirms the existing Review page.

## Product decision

Build a small, deterministic recommendation engine, not an AI model or a GPU-name database.
Clipline already probes the facts that most directly predict a reliable setup: usable encoders,
displays, audio devices, and installed games. Add free space for the selected media volume, apply a
documented rule table, and keep every recommendation editable in the existing wizard.

The first version must not run an active capture benchmark. Clipline's encoder probe proves that an
encoder can open and complete a small test encode, but it does not reproduce a game consuming the
GPU. A synthetic first-run benchmark could therefore overpromise high-resolution or high-FPS
capture. Start conservatively and consider runtime calibration only after real measurements show
the capability rules are insufficient.

## Research findings

- [OBS's Auto-Configuration Wizard](https://obsproject.com/kb/quick-start-guide) runs on first
  launch, can be rerun later, and considers the user's goal, hardware resources, and network. OBS
  still recommends making a real test recording. Its separate
  [hardware-encoding guidance](https://obsproject.com/kb/hardware-encoding) recommends a hardware
  encoder for low performance impact and ties bitrate to resolution and FPS.
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

| Fact | Output size | Quality | FPS | Encoder |
|---|---:|---|---:|---|
| A verified hardware H.264 option exists | Up to 1080p | Balanced | 60 | Automatic |
| No verified hardware H.264 option exists | Up to 720p | Balanced | 30 | Automatic |

Additional rules:

- Keep capture backend and video encoder on `Automatic`. The existing ranking already prefers
  compatible H.264 hardware and falls back safely; pinning a vendor backend would make recovery
  worse after driver or hardware changes.
- Do not automatically choose 1440p, 90 FPS, 120 FPS, HEVC, or AV1. The current probe cannot prove
  game-load headroom, and H.264 is the only codec Clipline can always play in WebView2.
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
   `Set this up for me` button and a short local-only privacy note.
2. On click, disable the button and show one honest `Checking this PC...` state. Do not simulate
   per-device progress when the backend only returns one result.
3. Analyze without mutating or saving settings. On success, fill the existing wizard controls, stage
   detected games, and open Review.
4. Add a compact `Recommended for this PC` summary above the existing review grid with the three or
   four material reasons: encoder/profile, replay memory, storage quota, and detected games.
5. Keep `Back` fully functional. A user can inspect or change every recommendation on Basics,
   Capture + recording, and Games.
6. Save only through the existing `Start Clipline` path. Analysis failure leaves the manual wizard
   usable and does not change the settings draft.

The button will also appear when the real wizard is replayed from Settings > Misc because that path
reuses the same UI. It must still produce a patch for wizard-owned fields rather than replacing the
whole `AppSettings`, so niche settings are not reset on an existing installation.

## Minimal architecture

- Add `apps/clipline-app/src/smart_config.rs` with serializable `SmartConfigFacts`,
  `SmartConfigPatch`, `RecommendationReason`, and one pure `recommend(facts)` function.
- The patch covers only fields owned by the first-run wizard. It is not a second settings model and
  never writes to disk.
- Add one asynchronous Tauri command, `recommend_first_run_settings`, accepting the selected media
  folder and existing custom games. It reuses display enumeration, audio enumeration, verified
  encoder options, game discovery, and `windows::available_space_bytes`.
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
  audio output, constrained disk, unknown disk space, and deterministic/idempotent output.
- [ ] Implement the smallest pure rule function that makes those fixtures pass.
- [ ] Assert that microphone, app-track splitting, disk replay, advanced recording, explicit codecs,
  and above-60-FPS presets are never enabled by v1 recommendations.

### Task 2: Local fact collection

- [ ] Add failing command-level tests with injected display/audio/encoder/storage/game results; do
  not make unit tests depend on real hardware.
- [ ] Add the asynchronous command and reuse the existing probes rather than adding a second device
  enumeration path.
- [ ] Preserve partial success: an unavailable audio device or game scan becomes a warning and a
  conservative patch, not an all-or-nothing failure.
- [ ] Register the command in the Tauri invoke handler and keep all new Windows calls behind the
  existing safe wrappers.

### Task 3: One-click wizard flow

- [ ] Add failing UI-contract assertions for the button, local-only copy, loading state, review
  explanation, and the absence of a direct `save_settings` call from analysis.
- [ ] Add the Basics recommendation card and accessible `aria-live` status.
- [ ] Apply a successful patch through the existing first-run form controls, stage all detected
  games, refresh dependent labels, and navigate to Review.
- [ ] Leave the form untouched on a fatal command error and restore the button so manual setup still
  works.

### Task 4: Review and editing safety

- [ ] Add tests proving Back retains the recommended values and lets individual detected games be
  deselected before finish.
- [ ] Render concise reasons and warnings without duplicating every Review row.
- [ ] Verify replaying the wizard from Settings changes only wizard-owned fields and persists nothing
  until `Start Clipline` succeeds.

### Task 5: Verification and handoff

- [ ] Run focused smart-config tests and JavaScript syntax checks.
- [ ] Run `cargo test --workspace`.
- [ ] Run `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Update `handoff.md` with the rule table and known limitations.
- [ ] Launch with isolated app data and manually test a verified-hardware path, forced fallback
  fixture, low-space warning, game-detection failure, Back/customize, and final save/start.

## Deferred until measurements justify it

- Active capture/encode benchmarks during onboarding.
- GPU/CPU model databases, cloud recommendations, or machine learning.
- Automatic 1440p/4K, 90/120 FPS, HEVC, or AV1 selection.
- Battery-aware profiles and display-refresh-rate tuning.
- Silent runtime quality changes. A future runtime advisor may offer an explicit downgrade after
  measured dropped frames, but it must explain and confirm the change.
