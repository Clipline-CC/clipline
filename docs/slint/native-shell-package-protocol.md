# Native Slint shell package protocol

Status: **internal-only candidate; not distributed or installed**. This protocol proves that a
first-party NSIS path can package the native Slint shell without Tauri or WebView2. It does not
authorize publishing, signing, installation on the developer workstation, or replacement of the
shipping Tauri application.

## Frozen identity and isolation

The installer metadata preserves product `Clipline`, publisher `Clipline`, base identifier
`io.clipline.app`, the version in both `apps/clipline-app/Cargo.toml` and `tauri.conf.json`, and the
reviewed `icons/icon.ico`. Installation is current-user only (`RequestExecutionLevel user`, HKCU,
and current-user shell context).

The internal candidate is deliberately isolated from the user's installed Clipline:

- binary and artifacts contain `Clipline-Slint-Internal-Candidate`;
- regular installs use `%LOCALAPPDATA%\Programs\Clipline Slint Candidate\regular` and uninstall key
  `io.clipline.app.slint-candidate.regular`;
- standalone installs use the corresponding `standalone` path/key;
- a shared candidate-only variant marker refuses regular/standalone crossing;
- production updater filenames remain reserved and cannot equal either internal artifact name.

The executable's package probe reports the production identity as package metadata, but the M6
runtime still uses `io.clipline.app.slint-spike` for single-instance activation. That separation is
intentional protection while the installed application is open; production runtime identity
cutover remains pending.

## Reproducible inputs and bounds

`scripts/build-slint-installer.ps1` accepts a prebuilt regular or standalone executable plus an
independently supplied SHA-256. It verifies the hash before executing a bounded, hidden,
side-effect-free `--clipline-package-probe`, requires an exact version/variant/identity response,
copies with create-new semantics, and rechecks the hash after probing and immediately before NSIS.
Required JSON/TOML/text inputs and probe output are read with explicit byte limits. Every copied
source is anchored by size and SHA-256 before staging and compared with the staged bytes. The
executable is capped at 256 MiB; aggregate sources are rejected before copying at 512 MiB, and the
final staged payload (including its generated manifest) is independently capped at 512 MiB.

FFmpeg remains a separately spawned, independently replaceable LGPL process. Production staging
must pass `scripts/verify-ffmpeg-resource.ps1`, then the candidate builder independently requires
the exact `ffmpeg-runtime.json` allowlist plus `README.md` and `PROVENANCE.json`. The installer also
contains root `THIRD-PARTY-NOTICES.md` and `ffmpeg\LICENSE.txt`. Missing, dirty, oversized,
reparse-point, size-drifted, or hash-substituted inputs fail before NSIS.

NSIS 3.11 and 7-Zip are explicit/discovered local tools. Their paths, versions, and SHA-256 values
are recorded; the scripts never download or install tools, never invoke Tauri bundling, and never
run the installer. The built installer is extracted with 7-Zip, its manifest-covered files are
compared byte-for-byte with staging, unexpected application payload is rejected, and any filename
containing WebView2 or Tauri (or an `ui/` tree) fails closed.

## Installer behavior

The explicit first-party NSIS source refuses downgrades and cross-variant installs. `/P` selects
passive mode, `/REINSTALL` authorizes a same-version repair, `/UPDATE` preserves existing shortcut
state, and the updater's exact `/P /R /UPDATE /ARGS` contract restarts the candidate only after a
successful install. The per-variant install directory is fixed: registry state, the directory UI,
and `/D=` cannot redirect it into the shipping application. The primary Slint process owns a
candidate-only local mutex; install/update/uninstall waits at most 30 seconds to acquire it before
touching payloads. Uninstall removes only enumerated candidate files and candidate-only HKCU
metadata, and preserves its uninstaller/metadata for retry when a required delete fails. It never
deletes settings, credentials, recordings, or the shipping application.

## Evidence boundary

PowerShell helper tests use synthetic, invocation-owned roots and exercise valid regular and
standalone staging plus tampered FFmpeg, missing license/notices/provenance, dirty payload,
oversized executable, wrong executable hash, wrong version, cross-install variant, pre-existing
output, and WebView payload rejection. CI may build and extraction-check an internal candidate but
must delete it afterward and must not upload, release, publish, sign, or execute it.

The following acceptance gates remain **pending** and require explicit operator approval in clean,
isolated Windows VMs:

- Windows 10 regular and standalone clean install;
- Windows 11 regular and standalone clean install;
- upgrade and downgrade-refusal matrix;
- uninstall with settings/media preservation;
- passive `/P /R /UPDATE /ARGS` signed-update handoff;
- production-identity migration and rollback to the Tauri build;
- Authenticode/minisign release signing and public updater delivery.

Until those gates are accepted, this output is not a release artifact and Milestone 6 makes no
installed-package claim.

## Milestone 6 closeout validation

Task 11 validation passed on 2026-08-02 without starting the shipping application or touching its
profile: CI-mode `cargo test --workspace`, warning-denied workspace Clippy, package-feature Slint
tests and strict Clippy for both variants, the PowerShell installer helper suite, the live Windows
process-fence test, the migration/repository contracts, and both debug and optimized benchmark
probes. The debug probe reported opt-level 0 and therefore correctly refused benchmark-safe status;
both probes reported `autostart_registry_mutation: false`, while the optimized benchmark profile
reported opt-level 3 and `benchmark_shell_safe: true`.

Two independent read-only audits returned GO after the fixed-directory, mutex-transaction,
uninstall-retry, exact-argument, source-anchoring, bounded-I/O, and extraction-receipt fixes. These
checks close the non-distributed construction/extraction proof only. The isolated VM, signing,
installed update, real-GPU, matched-memory, and accessibility gates above remain pending.

## 2026-08-02 local extraction evidence

Both optimized feature-specific binaries were packaged from the reviewed FFmpeg staging and
compiled with the first-party NSIS source. NSIS 3.11 SHA-256 was
`42850802704ecb11163f7e0329d35ee54bd288953200d4966e226d572848cfc5`. The explicit local
7-Zip 26.02 x64 extractor SHA-256 was
`83967f1b02b43c4efeda302795722c809e0e81b8307de73558d10484d5676a7d`.

| Variant | Internal artifact | Bytes | SHA-256 | Source executable SHA-256 |
|---|---|---:|---|---|
| Regular | `Clipline-Slint-Internal-Candidate_0.1.43_x64-setup.exe` | 51,837,733 | `53cee6d8980cc16b2f46e8981b03e5179084e971b234f7586d9077f2754cd7dd` | `0473645fd2a527f18f3630b8dae4371f0355bf153ca71e4acbbcff9d62ec4b6d` |
| Standalone | `Clipline-Slint-Internal-Candidate_0.1.43_x64-standalone-setup.exe` | 51,840,198 | `c189aa6296737e011900a7ade8e151f548dc57d30de398c9bad6bc196968326f` | `e1a6d025655e3d76e4c6a4f7da7f33bb684fde15522fd13d05d02b2ee6a9f4ce` |

Each receipt records the installer, source executable, both NSIS source, and builder SHA-256 values,
plus `extracted_without_execution: true` and `webview_payloads: 0`; every staged file size and hash
matched its extracted file. The installers and receipts remain untracked local evidence under
`artifacts/slint-package-candidate-frozen/`. They were not executed or installed. Both receipts
pin `installer.nsi` at `ece3042bf142f49f73f3d3ee4584db54dfc5d1bcec8ca36ecab348171c0bb007`
and the builder at `5269247f3b612691e0bb50a8d577198c9b08cd618d49309db8c2700bfa3c6ca9`.
