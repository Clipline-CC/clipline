# First Stable Release

> **For agentic workers:** Execute this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking and remain unticked by repository convention.

**Goal:** Enable Settings → Updates → Stable, make `main` identical to `develop`, and publish
Clipline's first non-prerelease GitHub release through a tag-triggered Stable workflow.

## Product decisions

- Stable is already modeled (`UpdateChannel::Stable`, `/releases/latest/download/latest.json`) and
  only gated off. Flip the gate; do not invent a second updater.
- Keep **Nightly as the default** channel. Existing `settings.json` values stay as they are. New
  installs keep checking Nightly until the user picks Stable.
- Do not reuse version **0.1.56**. That Nightly already shipped different bits. The first Stable
  binary is **0.1.57**.
- Stay on the 0.1.x line (do not jump to 1.0.0). Nightly and Stable share one version sequence;
  Stable is a less-frequent promotion of the same numbers.
- Stable tags are immutable `v<version>` on `main`. GitHub `/releases/latest` is the updater
  endpoint; there is no rolling `stable` tag.
- The pipeline clones Nightly: tests, Clippy, both NSIS variants, Tauri updater signatures, seven
  assets, draft-then-publish, public byte verification. Do **not** activate SignPath /
  `docs/release.workflow.yml` in this milestone.
- After `main` matches `develop`, the same 0.1.57 commit is tagged `v0.1.57` (Stable) and
  `nightly-v0.1.57` (Nightly) so current Nightly users receive the channel picker.

## Task 1: Enable the Stable option

- [ ] Flip `STABLE_CHANNEL_ENABLED` and rewrite the tests that currently require Stable to be
      disabled, repaired back to Nightly, or marked `disabled` / "coming soon" in the Settings UI.
- [ ] Re-enable the Stable `<option>` in General settings.
- [ ] Confirm `normalize_channel(Stable)` now keeps Stable, load/save round-trips `"stable"`, and
      update checks are allowed on that channel.

## Task 2: Stable release workflow

- [ ] Parameterize `scripts/prepare-nightly-assets.ps1` with `-Channel Nightly|Stable`. Stable
      requires tag `v<version>` and writes installer URLs under `/releases/download/v<version>/`.
- [ ] Add `.github/workflows/stable.yml` on `v*` tags. Require the commit to be contained in
      `main`, reject version regressions against GitHub's latest non-prerelease, and publish a
      non-prerelease `--latest` release (no rolling-tag delete).
- [ ] Add `repository_security.rs` contracts for the Stable workflow mirroring the Nightly ones.

## Task 3: Docs

- [ ] Replace the three-line "when stable is ready" stub in `docs/release-updates.md` with the
      Stable runbook. Point AGENTS.md, ddoc.md, and a handoff checkpoint at it.

## Task 4: Land on `develop`

- [ ] Open a PR, wait for green Ubuntu and Windows CI, merge to `develop`.

## Task 5: First Stable 0.1.57

- [ ] Bump Cargo, Cargo.lock, and Tauri to 0.1.57. Re-review WebView2 Fixed Version dates.
- [ ] Fast-forward `main` to that `develop` commit so the two branches are identical.
- [ ] Push `v0.1.57` and `nightly-v0.1.57` at that commit. Watch both release actions.
- [ ] Confirm `gh release view v0.1.57` is the GitHub latest non-prerelease with seven assets, and
      that `/releases/latest/download/latest.json` matches the staged manifest.
- [ ] Record publication in `handoff.md`.
