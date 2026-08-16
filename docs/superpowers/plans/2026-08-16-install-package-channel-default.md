# Install Package Decides the Default Update Channel

> **For agentic workers:** Execute this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking and remain unticked by repository convention.

**Goal:** A Stable download starts on Stable updates and a Nightly download on Nightly. Fresh
installs only; a saved `update_channel` always wins.

**Problem:** The channel default was hardcoded Nightly in three places (derive default,
`AppSettings::default()`, serde fallback), so every install — including Stable packages downloaded
from clipline.cc — started tracking the Nightly prerelease feed.

## Task 1: Bake the channel into each package

- [ ] Validate and re-export `CLIPLINE_DEFAULT_UPDATE_CHANNEL` in `build.rs` following the
      `CLIPLINE_BUG_REPORT_ENDPOINT` pattern; reject values other than `nightly`/`stable`, and
      keep Nightly for local dev builds.
- [ ] Resolve the default through `UpdateChannel::install_default()`; `UpdateChannel::default()`
      and `AppSettings::default()` follow it.
- [ ] Set the matching env on both installer build steps (regular + standalone) in `nightly.yml`
      and `stable.yml`, and on the future SignPath Stable template `docs/release.workflow.yml`.

## Task 2: Keep existing installs on their channel

- [ ] Legacy settings files without `update_channel` (written before the channel picker existed)
      load as Nightly, not the package default, so a Stable package cannot silently flip a
      Nightly-era install.
- [ ] A missing settings file (fresh install) still adopts the package default.

## Task 3: Verify and guard

- [ ] Prove the knob end to end: with `CLIPLINE_DEFAULT_UPDATE_CHANNEL=stable` baked, the
      compiled default is Stable; without it, Nightly (matching dev and CI).
- [ ] `repository_security.rs` pins that every `cargo tauri build` step in both release workflows
      bakes the matching channel and keeps the official bug report endpoint env.
- [ ] Workspace tests green and warning-denied Clippy clean; update `docs/release-updates.md` and
      `handoff.md`.
