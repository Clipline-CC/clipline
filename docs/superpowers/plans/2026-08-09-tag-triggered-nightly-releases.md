# Tag-triggered Nightly Releases

> **For agentic workers:** Execute this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking and remain unticked by repository convention.

**Goal:** Turn an immutable `nightly-v<version>` tag into Clipline's complete rolling `nightly`
prerelease without changing the updater URLs already shipped to users.

## Task 1: Make every release input reproducible

- [ ] Extend the reviewed WebView2 manifest with the exact official CAB URL, size, and SHA-256.
- [ ] Add a staging script that verifies the CAB before replacing the ignored runtime payload.
- [ ] Keep the regular installer free of bundled FFmpeg and stage FFmpeg only for standalone.

## Task 2: Add release asset generation

- [ ] Validate that `nightly-v<version>` matches Cargo, Cargo.lock, and Tauri versions exactly.
- [ ] Generate the regular and standalone updater manifests from their final filename-bound
      signatures.
- [ ] Generate concise release notes from commits since the previous rolling Nightly target.

## Task 3: Add the GitHub Actions release transaction

- [ ] Trigger only for `nightly-v*` tags, serialize releases, and never cancel one in progress.
- [ ] Confirm the tagged commit belongs to `develop`, then run workspace tests and Clippy.
- [ ] Build and preserve the regular installer before staging standalone-only runtimes.
- [ ] Build, rename, and re-sign the standalone installer under its final asset name.
- [ ] Upload all seven assets to a draft staging release before replacing the rolling `nightly`
      release and tag.
- [ ] Redownload the public release and compare every asset with the staged bytes.

## Task 4: Verify and document operation

- [ ] Add neutral repository contract coverage for trigger, permissions, ordering, and inputs.
- [ ] Run workspace tests and warning-denied workspace Clippy.
- [ ] Update release documentation and `handoff.md` with the one-command tag workflow.
