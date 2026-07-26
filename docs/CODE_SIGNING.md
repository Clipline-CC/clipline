# Code signing (SignPath, free for open source)

## Why

Clipline ships an **unsigned** NSIS installer. Combined with its behavioral
fingerprint — enumerating processes (the RAM-usage indicator) plus capturing
per-process audio (the audio-splitting feature, PR #37) — AV/EDR ML engines
score the binary as *grayware*. VirusTotal shows CrowdStrike Falcon flagging
`Clipline_x.y.z_x64-setup.exe` as `Win/grayware_confidence_70%`. Real-time
Defender (cloud ML / behavioral) and EDRs can **silently quarantine it
pre-execution** → the reported symptom: "double-click, no window, nothing in
Task Manager, no dialog."

Authenticode **code signing** is the durable fix: a signed binary with
building reputation is dramatically less likely to be ML-flagged or silently
blocked, and an EV cert clears SmartScreen immediately. SignPath signs verified
open-source projects **for free**.

## What signs what (important)

The reported failure is the *installed app* not launching, so the file that
real-time AV scans at runtime is the **inner `clipline-app.exe`**, not just the
installer. Two layers:

1. **The NSIS installer** — signing it clears SmartScreen on download/run and
   AV scanning of the installer. The release workflow does this.
2. **The inner `clipline-app.exe`** — this is the one that must be signed to fix
   "the installed app won't launch." Options:
   - Configure the SignPath **artifact configuration** to recurse into the NSIS
     installer and sign the contained `.exe` (verify NSIS recursion is
     supported for your SignPath plan), **or**
   - Two-pass build: `cargo build --release` → SignPath-sign
     `target/release/clipline-app.exe` → bundle the signed exe with
     `cargo tauri build` → SignPath-sign the resulting installer. More robust;
     more workflow plumbing.

   Start with #1; if affected users still report blocks on the installed app,
   move to the two-pass.

## One-time SignPath setup

1. **Apply to the OSS program.** Sign in at <https://signpath.io> with GitHub
   and request open-source status for the public `dain98/clipline` repo (free
   tier). Wait for approval.
2. **Organization** → note the **Organization ID** (a GUID).
3. **Project** linked to the GitHub repo → note the **project slug**
   (e.g. `clipline`).
4. **Trusted build system**: add **GitHub Actions** as a trusted build system
   so SignPath accepts artifacts uploaded by this repo's workflows.
5. **Artifact configuration**: define one for the NSIS installer (slug e.g.
   `nsis-installer`). Configure it to Authenticode-sign the installer (and the
   inner `.exe` if recursion is supported — see above).
6. **Signing policies**: create `test-signing` (auto-signs, for CI smoke tests)
   and `release-signing` (the real one; OSS release signing typically requires a
   manual approver to click "approve" per request).
7. **CI API token**: create a SignPath **CI user + API token**.

## Repository configuration (Settings → Secrets and variables → Actions)

Secrets:

| Secret | What |
| --- | --- |
| `SIGNPATH_API_TOKEN` | SignPath CI user API token |
| `TAURI_SIGNING_PRIVATE_KEY` | Existing minisign **updater** key (base64) — the one whose pubkey is in `tauri.conf.json` |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | Password for that updater key |

Variables:

| Variable | Example |
| --- | --- |
| `SIGNPATH_ORGANIZATION_ID` | `00000000-0000-0000-0000-000000000000` |
| `SIGNPATH_PROJECT_SLUG` | `clipline` |
| `SIGNPATH_SIGNING_POLICY_SLUG` | `release-signing` |
| `SIGNPATH_ARTIFACT_CONFIG_SLUG` | `nsis-installer` |

The private diagnostic destination is not a repository variable. Every build is pinned to
`https://support.dain.cafe/api/v1/reports`, and the build rejects attempts to replace it.

## Activating the workflow

The pipeline ships as **`docs/release.workflow.yml`** rather than under
`.github/workflows/`. Pushing a file into `.github/workflows/` requires a token
with the `workflow` OAuth scope, which the automation account does not hold — so
the file rides along here and **you** move it into place from a machine whose
GitHub credentials have that scope:

```sh
git mv docs/release.workflow.yml .github/workflows/release.yml
git commit -m "ci: activate signed-release workflow"
git push
```

(Do this only after the SignPath setup and repository secrets/variables below
are in place — the workflow needs them to run.)

## How a release runs (`.github/workflows/release.yml`)

Trigger: push a `v*` tag (or run it manually via *workflow_dispatch*).

1. Download the archive pinned by `ffmpeg-runtime.json`, stage its exact
   allowlist, and verify the hashes, provenance, version, and LGPL configuration.
2. Build the NSIS installer + Tauri updater artifacts (`cargo tauri build`);
   Tauri repeats the FFmpeg verification offline immediately before bundling.
3. Upload the unsigned installer and submit it to SignPath; wait for the signed
   result (a `release-signing` request may pause for manual approval).
4. **Regenerate the updater signature** over the *signed* installer. Authenticode
   signing rewrites the PE, so the `.sig` Tauri made during the build is stale —
   the workflow re-runs `tauri signer sign` on the signed file and rebuilds
   `latest.json`. (Skipping this breaks auto-update: the updater would reject the
   signed installer.)
5. Publish a **draft** GitHub release with the signed installer, its `.sig`, and
   `latest.json`. Review, then publish.

## Before flipping the release to non-draft

- The updater endpoint in `tauri.conf.json` points at the **`nightly`** release's
  `latest.json`. Decide whether a signed build should publish `latest.json` to
  `nightly` or to the version tag, and adjust the workflow's release step.
- After the first signed release, submit the binary to Microsoft and CrowdStrike
  as a **false-positive** to accelerate reputation, then re-check on VirusTotal.
