# Clipline Updates

Clipline uses Tauri's signed updater. The app checks a channel-specific
`latest.json` file uploaded as a GitHub Release asset.

## Storage quota behavior

Saved-media quotas stay non-destructive by default: when an install reaches its configured
quota, recording and replay saves pause until the user deletes media or raises the quota.
Settings → Storage can opt into oldest-first auto-delete of managed clips before that lock.
Clipline retains all non-empty recordings, including short osu! sessions that older builds
discarded as startup transients.

## Nightly

The enabled channel is Nightly:

```text
https://github.com/Clipline-CC/clipline/releases/download/nightly/latest.json
```

Each nightly ships two installer variants built from the same commit:

- **Regular** (`Clipline_<ver>_x64-setup.exe`) — embeds the WebView2 Evergreen
  bootstrapper; small download.
- **Standalone** (`Clipline_<ver>_x64-standalone-setup.exe`) — bundles the
  WebView2 Fixed Version runtime inside the install folder, for users who do
  not want the system-wide WebView2 runtime. Nothing WebView2-related is
  installed system-wide. Adds ~150 MB to the installer.

Each variant has its own updater manifest (`latest.json` /
`latest-standalone.json`); the app picks the right one at runtime by checking
its baked-in `webviewInstallMode` (see `is_standalone_install` in `app.rs`),
so standalone installs never update into the Evergreen installer.

### Agent runbook: “make a new Nightly release”

When the user asks for a new Nightly, carry out this entire sequence:

1. Confirm the intended feature PRs are merged into `develop` with green Ubuntu and Windows CI.
2. Read the current rolling `nightly/latest.json`, choose the next patch version, and create the
   usual unticked release plan in `docs/superpowers/plans/`.
3. Update `apps/clipline-app/Cargo.toml`, the `clipline-app` entry in `Cargo.lock`, and
   `apps/clipline-app/tauri.conf.json` to that exact version.
4. Re-review the current Microsoft WebView2 Fixed Version release even when the pinned version is
   unchanged. Refresh `reviewed_on` / `review_due_on`; when the runtime changes, update its exact
   CAB URL, size, SHA-256, and both paths in `tauri.standalone.conf.json` together.
5. Run `scripts/verify-webview2-runtime.ps1`, `cargo test --workspace`, and
   `cargo clippy --workspace --all-targets -- -D warnings`. Confirm the release-only diff contains
   no accidental product changes.
6. Commit and push the release metadata on a branch, open a PR into `develop`, and merge after
   Ubuntu and Windows CI passes. Do not tag a feature branch or a commit that is not yet contained
   in remote `develop`.
7. Create and push the immutable `nightly-v<version>` tag at that exact release commit.
8. Watch the **Nightly Release** GitHub Action, review the unsigned artifacts, and approve its
   `release-signing` environment gate. Do not manually replace the rolling `nightly` tag or
   upload assets while the action is running.
9. Confirm `gh release view nightly` targets the release commit and exposes exactly seven assets.
   The action already redownloads and hashes every public asset; treat a failed verification as a
   failed release even if GitHub shows a prerelease.
10. Record the published commit, release URL, version, and verification result in `handoff.md`, then
    report the outcome to the user.

For a transient Actions failure, rerun the same tag workflow. If the release inputs or code need a
new commit, bump to the next patch version and create a new tag; never force-move an existing
`nightly-v<version>` tag. If a failure happens during final promotion, inspect the rolling
`nightly` release before retrying so an already-published release is not replaced unnecessarily.

Nightly publication is automatic from the version tag. After the release commit is on `develop`,
the final trigger is:

```powershell
git switch develop
git pull --ff-only origin develop
$version = (Get-Content apps/clipline-app/tauri.conf.json -Raw | ConvertFrom-Json).version
git tag "nightly-v$version"
git push origin "nightly-v$version"
```

`.github/workflows/nightly.yml` rejects tags whose version does not exactly match Cargo,
Cargo.lock, and Tauri, tags outside `develop`, and version regressions. It runs workspace tests and
Clippy, bakes the Nightly update-channel default into both installers
(`CLIPLINE_DEFAULT_UPDATE_CHANNEL=nightly`), builds and preserves the regular installer, downloads
and verifies the hash-pinned WebView2 and FFmpeg inputs, and builds the standalone
installer with updater artifacts disabled. A separate GitHub-hosted, artifact-only job in the
`release-signing` environment uses the hash-pinned Tauri signer to sign both installers and fill
their updater manifests, after the environment reviewer approves. All seven assets are uploaded to a draft staging
release before the action replaces the rolling `nightly` release. The published assets are then
downloaded again and compared byte-for-byte with the staged build.

The workflow needs `TAURI_SIGNING_PRIVATE_KEY` only as a `release-signing` environment secret; the key has no
password, so `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` may remain unset. The version tag remains as an
immutable audit marker while the separate `nightly` tag continues moving for installed clients.

The release must include both updater metadata assets (`latest.json`,
`latest-standalone.json`). A WebView2 Fixed Version review is required for
every standalone release and at least every 30 days. Compare the official
release notes with the pinned version, update `webview2-fixed-runtime.json`,
update both paths in `tauri.standalone.conf.json` when the version changes,
stage the matching runtime, and run the preflight above. Before publication,
play an H.264/Opus clip through its end in the standalone build and confirm the
HEVC/AV1 capability probes still enable only codecs that the runtime can play.
When bumping FFmpeg, select a retained immutable LGPL-shared release, review
its license and configuration, then rotate every version, URL, archive/file
size, and hash in `apps/clipline-app/ffmpeg-runtime.json` together. Run the
staging script against the exact archive and review the logged provenance.
Never use BtbN's floating `latest` asset. `apps/clipline-app/ffmpeg/` is a
build staging directory and its binaries are intentionally git-ignored.

**Regular installer** no longer embeds `ffmpeg/` (slim-core on-demand runtime).
Do **not** stage FFmpeg before a regular `cargo tauri build` if you are measuring
the lightweight SKU. Measured regular setup after the drop: **9.35 MiB**.

**Standalone / offline SKU** still lists `ffmpeg/` in `tauri.standalone.conf.json`.
For that build only: stage with `scripts/stage-ffmpeg-resource.ps1`, then run
`cargo tauri build --config tauri.standalone.conf.json`; its standalone-only
`beforeBundleCommand` runs `scripts/verify-ffmpeg-resource.ps1` automatically.
The regular `tauri.conf.json` no longer runs `beforeBundleCommand` verify-ffmpeg.
The active tag-triggered GitHub Actions workflow performs this ordering automatically.

CI runs on PRs and pushes to both release branches. `develop` requires a PR and passing
`test (ubuntu-latest)` / `test (windows-latest)` checks. Both branches reject force-pushes and
deletion; only the maintainer can create version tags, and existing version tags are immutable.

If `beforeBundleCommand` fails with `Get-FileHash` not recognized, run `cargo tauri build`
from bash rather than pwsh. Do not bypass the verification script to work around the shell.

After publishing, verify what is actually downloadable rather than what was
staged: fetch each asset from its public URL, confirm the bytes match the staged
installers, and check that the signature **in each manifest** validates the
downloaded bytes under the `pubkey` in `tauri.conf.json`. That is precisely what
the updater does, and it catches a mismatched, stale, or crossed-over signature
that per-file checks miss.

## Signing

The updater public key is committed in `apps/clipline-app/tauri.conf.json`.
The matching private key was generated locally at:

```text
.local-secrets/clipline-updater.key
```

Add the private key contents to the `release-signing` environment secret, then remove the
repository-level secret. Environment access is limited to versioned release tags and requires
the maintainer's approval. Benchmark jobs generate throwaway keys and receive no signing secrets:

```text
TAURI_SIGNING_PRIVATE_KEY
```

The generated key has no password, so `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` can
be omitted or left empty.

If this private key is lost, future update bundles cannot be signed for
currently installed builds. Generate a new key only when you are ready to rotate
the public key in the app.

### Rotation after the benchmark exposure

The original key was exposed to third-party benchmark runners. Treat rotation as pending until
both Stable and Nightly clients can reach an old-key-signed bridge release. Generate a new key
outside CI; ship its public key in that bridge, while signing the bridge installers with the old
key. Keep the bridge manifests available on both channels long enough for existing clients to
upgrade. Then replace the protected environment's key and sign subsequent releases with the new
key. Clients that missed the bridge need a separately retained old-key-signed bridge manifest
or a manual installer; replacing the rolling manifest immediately would strand those clients.
Do not overwrite the current key or public key during a routine hardening change.

## Stable

The enabled Stable endpoint is GitHub's latest non-prerelease:

```text
https://github.com/Clipline-CC/clipline/releases/latest/download/latest.json
```

Standalone installs use `latest-standalone.json` at the same latest URL. Settings → General →
Updates exposes **Nightly** and **Stable**. A fresh install starts on the channel its package
tracks (`CLIPLINE_DEFAULT_UPDATE_CHANNEL` baked by the release workflow: Stable installers default
to Stable, Nightly installers and local dev builds to Nightly), and a saved choice always wins;
legacy settings files written before the channel existed stay on Nightly. `STABLE_CHANNEL_ENABLED`
in `apps/clipline-app/src/updates.rs` is the compile-time gate for that option.

Each stable ships the same two installer variants as Nightly. Manifest installer URLs point at
the versioned release (`/releases/download/v<version>/...`), not at `/releases/latest/download/`
for the binaries, so a cached manifest cannot fetch a newer release's differently named setup.

### Agent runbook: “make a new Stable release”

When the user asks for a new Stable, carry out this entire sequence:

1. Confirm the intended commit is on `develop` with green Ubuntu and Windows CI, then fast-forward
   `main` so the two branches are identical.
2. Read the current `/releases/latest/download/latest.json` (404 on the first Stable is expected),
   choose the next patch version, and create the usual unticked release plan.
3. Update `apps/clipline-app/Cargo.toml`, the `clipline-app` entry in `Cargo.lock`, and
   `apps/clipline-app/tauri.conf.json` to that exact version. Re-review WebView2 Fixed Version
   metadata as for Nightly.
4. Run `scripts/verify-webview2-runtime.ps1`, `cargo test --workspace`, and
   `cargo clippy --workspace --all-targets -- -D warnings`.
5. Commit and push release metadata on a branch, merge its PR into `develop` after Ubuntu and
   Windows CI passes, then fast-forward `main` again. Do not tag a commit that is not yet
   contained in remote `main`.
6. Create and push the immutable `v<version>` tag at that exact `main` commit.
7. Watch the **Stable Release** GitHub Action, review the unsigned artifacts, and approve its
   `release-signing` environment gate. Do not upload assets while the action is running.
8. Confirm `gh release view v<version>` is a published non-prerelease targeting the release commit,
   is GitHub's latest release, and exposes exactly seven assets. Confirm
   `/releases/latest/download/latest.json` matches the staged manifest.
9. Record the published commit, release URL, version, and verification result in `handoff.md`.

For a transient Actions failure, rerun the same tag workflow. If the release inputs or code need a
new commit, bump to the next patch version and create a new tag; never force-move an existing
`v<version>` tag.

Stable publication is automatic from the version tag. After the release commit is on `main`:

```powershell
git switch main
git pull --ff-only origin main
$version = (Get-Content apps/clipline-app/tauri.conf.json -Raw | ConvertFrom-Json).version
git tag "v$version"
git push origin "v$version"
```

`.github/workflows/stable.yml` rejects tags whose version does not exactly match Cargo, Cargo.lock,
and Tauri, tags outside `main`, and version regressions against the current GitHub latest
non-prerelease. It builds the same seven assets as Nightly (baking the Stable update-channel
default via `CLIPLINE_DEFAULT_UPDATE_CHANNEL=stable`), uploads them to a draft GitHub release
for `v<version>`, then publishes that release as latest. It does not move or delete previous Stable
releases. `docs/release.workflow.yml` remains the future SignPath Authenticode template and is not
the active Stable pipeline.
