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
https://github.com/dain98/clipline/releases/download/nightly/latest.json
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
6. Commit and push the release metadata to `develop`. Do not tag a feature branch or a commit that
   is not yet contained in remote `develop`.
7. Create and push the immutable `nightly-v<version>` tag at that exact release commit.
8. Watch the **Nightly Release** GitHub Action until it finishes. Do not manually replace the
   rolling `nightly` tag or upload assets while the action is running.
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
Clippy, builds and preserves the regular installer, downloads and verifies the hash-pinned
WebView2 and FFmpeg inputs, builds and re-signs the renamed standalone installer, and generates
both updater manifests plus release notes. All seven assets are uploaded to a draft staging
release before the action replaces the rolling `nightly` release. The published assets are then
downloaded again and compared byte-for-byte with the staged build.

The workflow needs only the existing `TAURI_SIGNING_PRIVATE_KEY` repository secret; the key has no
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

Two environment traps cost time on 0.1.42 and are worth expecting:

- `ci.yml` triggers only on `push` to `main` and on `pull_request`, so the
  version-bump commit pushed to `develop` gets **no** CI checks at all
  (`gh api .../check-runs` returns `total_count: 0`). Do not read that as green.
  Verify the release commit locally and confirm its code is identical to the
  last CI-green merge commit — `git diff <merge> <release>` should show only
  version strings and docs — then say so explicitly in the release notes.
- If `beforeBundleCommand` fails with `Get-FileHash` not recognized, run
  `cargo tauri build` from bash rather than pwsh. The verification script itself
  is fine; do not edit or bypass it to work around the shell.

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

Add the private key contents to the repository secret:

```text
TAURI_SIGNING_PRIVATE_KEY
```

The generated key has no password, so `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` can
be omitted or left empty.

If this private key is lost, future update bundles cannot be signed for
currently installed builds. Generate a new key only when you are ready to rotate
the public key in the app.

## Stable

Stable is modeled in settings but intentionally disabled until Clipline has
stable releases. When stable is ready:

1. Flip `STABLE_CHANNEL_ENABLED` in `apps/clipline-app/src/updates.rs`.
2. Publish non-prerelease GitHub releases with updater `latest.json`.
3. Re-enable the Stable option in the General settings UI.
