# First-Start Walkthrough Plan

**Goal:** Give genuine first-time users a short, privacy-forward setup walkthrough before Clipline begins capturing, while leaving existing installs and settings-recovery startups unchanged.

## First-run lifecycle

- [ ] Extend the recovery-aware settings startup result with an explicit fresh-install signal that is true only when both `settings.json` and `settings.json.bak` are missing.
- [ ] Keep corrupt/unreadable settings recovery separate from first run so recovery warnings never open onboarding.
- [ ] Manage the pending first-run state for the lifetime of the native process and expose it through `frontend_ready`.
- [ ] Do not start the recorder during native setup while first-run setup is pending.
- [ ] Add a completion command that starts recording only after settings have been saved and clears the in-memory pending state so a rebuilt WebView does not reopen the walkthrough.

## Walkthrough UI

- [ ] Add an accessible modal walkthrough that cannot be dismissed accidentally with Escape or a backdrop click.
- [ ] Step 1 explains that Clipline records locally, uploads nothing by default, and remains paused until setup finishes.
- [ ] Step 2 lets the user choose games-only capture versus the primary-display fallback and whether supported games save full sessions automatically.
- [ ] Step 3 lets the user enable/disable desktop audio and the default microphone, teaches the `Alt+F10` Save Replay shortcut, and previews the selected behavior.
- [ ] `Finish setup` saves the selected settings transactionally, starts recording, and closes the walkthrough. `Use defaults` follows the same path without changing defaults.
- [ ] Keep smart hardware recommendations, device selection, Cloud, folders, themes, and advanced recording outside this milestone.

## Tests and verification

- [ ] Add failing settings tests for fresh install, primary load, backup recovery, and invalid-settings recovery classification.
- [ ] Add backend tests for the pending-state transition and native startup guard.
- [ ] Add UI contract coverage for the modal structure, accessible step state, persistence/start ordering, and non-dismissible behavior.
- [ ] Run `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Update `handoff.md`, rebuild Clipline, open it, and manually verify both a fresh settings directory and an existing settings file.
