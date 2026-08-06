# First-run Setup Wizard Plan

**Goal:** Show the approved four-page setup flow only when Clipline has never saved settings, and apply the choices through the existing settings transaction before recording begins.

## First-run lifecycle

- [ ] Classify startup as first-run only when neither `settings.json` nor its recovery copy exists; recovered or invalid prior settings must not reopen onboarding.
- [ ] Keep the recorder stopped during first-run setup while leaving existing-install startup unchanged.
- [ ] Expose first-run state to the frontend and clear it after any successful settings save so recreated WebViews cannot reopen the wizard.

## Production wizard

- [ ] Add the Basics page for replay hotkey, media folder, 10 GB quota, and launch-on-startup enabled by default.
- [ ] Add Capture + recording with the existing output/input device controls, mic test, capture target, games-only pause, 30-second replay, 1080p output, Balanced quality, and 60 FPS defaults.
- [ ] Add Games with enabled built-in profiles plus inline installed-game detection and selection.
- [ ] Add Review, save through the existing `save_settings` command, start recording only after success, and keep failures visible without dismissing the wizard.

## Regression coverage

- [ ] Add persistence tests for first-run, normal, recovered, and damaged-settings startup classification.
- [ ] Add UI contract coverage for wizard structure, required controls/defaults, inaccessible background state, save path, and recorder gating.
- [ ] Run focused tests, workspace tests, and warning-denied workspace Clippy.

## Handoff

- [ ] Update `handoff.md`, launch Clipline with isolated first-run app data, and verify the complete wizard manually without touching the user's saved settings.
