# Clipline Updates

Clipline uses Tauri's signed updater. The app checks a channel-specific
`latest.json` file uploaded as a GitHub Release asset.

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

For now, publish Nightly manually from a Windows checkout:

```powershell
# For every standalone release, review Microsoft's current WebView2 Fixed
# Version release even when the pinned version does not change. Update
# webview2-fixed-runtime.json (reviewed_on/review_due_on and version when
# needed), then download the reviewed x64 .cab from the official Fixed Version
# section:
# https://developer.microsoft.com/en-us/microsoft-edge/webview2/
#   expand.exe -F:* <runtime>.cab apps\clipline-app\webview2-fixed
# Keep the folder name (with version) in sync with tauri.standalone.conf.json
# — both the resources glob and the webviewInstallMode path. The preflight
# rejects a review older than 30 days, config drift, and a missing runtime
# executable in the staged payload.
.\scripts\verify-webview2-runtime.ps1 -RequirePayload

$env:TAURI_SIGNING_PRIVATE_KEY = Get-Content .local-secrets\clipline-updater.key -Raw

# Download and stage the exact reviewed LGPL FFmpeg archive used for gallery
# posters and the optional FFmpeg encoder tier. The script hashes the archive
# before opening it, extracts only the manifest allowlist, validates the
# executable/version/configuration and per-file hashes, and emits
# PROVENANCE.json beside the staged runtime.
$ffmpegManifest = Get-Content apps\clipline-app\ffmpeg-runtime.json -Raw | ConvertFrom-Json
$ffmpegInputs = Join-Path $env:LOCALAPPDATA "Clipline\release-inputs"
New-Item -ItemType Directory -Path $ffmpegInputs -Force | Out-Null
$ffmpegArchive = Join-Path $ffmpegInputs $ffmpegManifest.archive_name
Invoke-WebRequest -Uri $ffmpegManifest.archive_url -OutFile $ffmpegArchive
.\scripts\stage-ffmpeg-resource.ps1 -ArchivePath $ffmpegArchive
.\scripts\verify-ffmpeg-resource.ps1

# 1. Regular build (from apps/clipline-app so config discovery works)
Set-Location apps/clipline-app
cargo tauri build
# stage target/release/bundle/nsis/Clipline_<ver>_x64-setup.exe + .sig

# 2. Standalone build (overlay merges over tauri.conf.json)
#    Both builds write the SAME bundle path, so stage step 1's exe and .sig
#    before starting this one or the regular installer is overwritten.
cargo tauri build --config tauri.standalone.conf.json
# stage and rename to Clipline_<ver>_x64-standalone-setup.exe, then RE-SIGN it
# under the new name — do not rename the .sig. A minisign trusted comment
# embeds `file:<name>` and is covered by the signature, so a renamed .sig still
# claims the regular installer's filename:
#   cargo tauri signer sign -f ..\..\.local-secrets\clipline-updater.key -p ""
#     <staged standalone exe>
# The build's own key prompt dies in a non-interactive shell; pass -p "".

# 3. Author latest.json and latest-standalone.json (version, pub_date,
#    platforms.windows-x86_64.{signature,url}); the url in each must point at
#    the corresponding renamed release asset.

gh release delete nightly --cleanup-tag --yes
gh release create nightly <bundle assets> --prerelease --title "Clipline Nightly"
```

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
build staging directory and its binaries are intentionally git-ignored; its
allowlisted `PROVENANCE.json` and license are bundled into both installers.
Tauri runs `scripts/verify-ffmpeg-resource.ps1` again through
`build.beforeBundleCommand` immediately before either installer is bundled.
This offline preflight rejects a README-only, incomplete, modified, or
misconfigured runtime; do not bypass it or replace it with a network fetch.
A GitHub Actions workflow can automate this later, but pushing workflow files
requires a token with GitHub's `workflow` scope.

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
