# Clipline — Development Handoff

> For a fresh Claude Code session (or human) continuing this project.
> **`ddoc.md` is the single source of truth** for product/architecture decisions. This file is
> the bridge: where the project stands, how it's built, what bit us, and what's next.

## Checkpoint (2026-08-07): Nightly 0.1.46

Plan: `docs/superpowers/plans/2026-08-07-nightly-0.1.46.md` (`dfac393`).

Nightly 0.1.46 is the one-week soak candidate for Clipline's first stable release. It combines the
slim regular installer and on-demand FFmpeg runtime, lower tray memory through a destroyable
WebView, the sparse-capture GOP watchdog fix, first-run manual and one-click setup, F6 as the new
install default, cancellable and Library-accessible clipboard workflows, visible Cloud upload
progress, and the non-destructive saved-media quota lock.

PR #142 merged to `develop` at `7315a6e9` after green Ubuntu and Windows checks. Microsoft's
current WebView2 release documentation lists Runtime 151 as the stable line and SDK 1.0.4129.50 as
requiring Runtime 151.0.4129.50 or newer. The pinned standalone payload remains the later
151.0.4129.59 x64 Fixed Version CAB and was re-reviewed for this release on 2026-08-07.

The release commit advances only Clipline/Tauri version metadata, the WebView2 review dates, this
handoff, and the release plan from the CI-green merge. Publication and downloaded-asset hashes are
recorded here after all signed assets are independently verified.

---

## Checkpoint (2026-08-07): saved-media quotas are non-destructive

Plan: `docs/superpowers/plans/2026-08-07-non-destructive-storage-quota.md` (`88e6d10`).

The oldest-first saved-clip collector has been removed from `clipline-storage`; there is no longer
an automatic saved-media deletion API. Storage scans are observational, and only explicit Library
deletes may remove a saved clip. Zero-byte reservations and temporary replay-cache segments remain
the only automatic cleanup targets. Short and imperfectly finalized non-empty full sessions are
kept for recovery instead of discarded.

The recorder now resolves and recovers its media root before opening capture hardware, then blocks
startup when the library has no capacity. Replay saves measure the selected encoded window and add
conservative muxing headroom before creating an output file. Full sessions reserve 64 MiB for safe
finalization; their once-per-second check combines the startup inventory total with only the active
file size instead of rescanning the media tree. Inspection failures are logged and skipped rather
than terminating recording. A completed output that unexpectedly crosses the limit is kept and
locks future recording.

Quota-full is a durable backend state that gates recorder starts, restarts, save commands, global
and low-level hotkeys, and tray saves. The frontend shows an accessible modal, marks recording as
disabled, and offers Library management, the media folder, Storage settings, and an explicit
recheck. Raising/disabling the quota, switching to a media folder with enough room, or deleting
enough clips clears the lock and resumes recording when it was previously desired.
Refresh-driven rechecks update the displayed usage silently, so dismissing the dialog to manage
clips does not reopen it after every deletion; the explicit Check again action may announce it.

The prior handoff entries describing saved-clip auto-GC are historical and are superseded by this
checkpoint and `ddoc.md`.

Validation is green: the full workspace test suite, fresh-cache warning-denied workspace Clippy,
JavaScript syntax checks, and diff checks all pass.

---

## Checkpoint (2026-08-07): cancellable clipboard export and library feedback

Plan: `docs/superpowers/plans/2026-08-07-clipboard-library-qol.md` (`02758a4`).

Clipboard share exports now have a process-owned generation token. Starting another copy cancels
the prior job; closing the main WebView to the tray or quitting Clipline also cancels the active
job. The FFmpeg polling loop observes cancellation every 25 ms, kills and reaps the child, and
returns through the existing temporary-file cleanup. Cancellation is rechecked before Windows
clipboard ownership changes, so a completed-but-abandoned export cannot replace clipboard data.

The Review copy button remains shareable by default through five minutes. Longer clips now copy
the original media file immediately, and Shift-click still explicitly copies the original. Local
Library cards expose both `Copy to clipboard` (original) and `Copy shareable clip` (forced
compatible export) in their right-click menu; cloud-only and game-play menus hide both actions.
Library share exports use the clip's default audio-track selection when the clip is not already
open in Review.

Cloud uploads now publish app-wide start and completion notices. While a local clip's cloud record
is queued, uploading, processing, or retrying, a spinner appears immediately after its Library title;
the existing byte progress, deck status, refresh arbitration, retry state, and local-delete
feedback remain intact.

The cancellation generation, five-minute rule, context-menu ownership, upload notices, and busy
indicator are contract-tested. Full workspace tests, fresh-cache warning-denied workspace Clippy,
JavaScript syntax checks, and diff checks are green. Manual retest: right-click a local clip and
try both copy actions; open a clip over five minutes and confirm normal toolbar Copy reports the
original; force a shareable export and close Clipline while it runs; upload a clip and confirm the
start notice, title spinner, and completion notice.

---

## Checkpoint (2026-08-07): one-click recommended setup

Plan: `docs/superpowers/plans/2026-08-07-smart-configuration.md` (`abbfbba8`).

The wizard now opens on a full-app welcome screen that introduces Clipline before showing two
prominent choices. `Start setup` opens the existing four-page manual wizard; `Set this up for me`
runs the recommended flow for users who do not want to tune it. The recommendation reuses the
existing wizard form and Settings transaction: device enumeration completes first, then the draft
is set to F6, 10 GB, launch on startup, primary display, default output audio at 100%,
30-second memory replay, 720p, Balanced quality, 60 FPS, pause without a game, and all built-in
game integrations. The default microphone is enabled at 100% mono only when an input device is
available.

F6 is also the backend settings default, startup-registration fallback, and initial rail label.
Existing saved hotkeys are preserved; this change intentionally does not migrate current users.

The same installed-game detector runs immediately and stages every result for addition. The flow
then jumps to Review, where a compact `Set up for you` card summarizes the profile, microphone
state, and detected-game count. Detection failure is non-fatal: Review explains it and Back exposes
the existing retry control. Back also lets the user edit the generated draft and detected-game
selection. Nothing is persisted until `Start Clipline`; finishing adds any staged games and uses
the existing `save_settings` transaction.

PR review hardening made the Settings replay path safe for existing users. Replayed setup seeds
every wizard-owned field from the saved settings, preserves the secondary hotkey, game auto-detect,
advanced recording mode, and other controls the wizard does not expose, and offers Cancel plus
Escape to discard the draft and return to Settings. Detected games remain staged through Review
and are added only inside the successful save attempt; scans lock navigation while pending and
their transient results reset whenever the wizard opens. Finishing a replay preserves the current
recording state, while a genuine first run still starts recording after save. Review now discloses
memory versus disk replay storage, the recommended preset selects the enumerated primary display,
and the automatic update check waits until genuine first-run setup closes.

The one-click markup, preset values, microphone availability rule, all-game selection, Review
state, welcome choice, and no-early-save boundary are protected by the UI contract. JavaScript
syntax, full workspace tests, warning-denied workspace Clippy from a clean `clipline-app` cache,
and diff checks are green. Manual retest: open Settings > Misc > Play first-time wizard, confirm
Cancel and Escape return to Misc without changing settings, then reopen it and verify `Start setup`
shows the saved values. Click `Set this up for me`, confirm the scan lands on Review with 720p /
60 FPS / Balanced / 30 sec in memory, and save only if those preset changes are wanted.

---

## Checkpoint (2026-08-06): first-run setup wizard

Plan: `docs/superpowers/plans/2026-08-06-first-run-setup-wizard.md` (`b681e28`).

Clipline now distinguishes a genuinely new install (neither `settings.json` nor its recovery copy
exists) from a recovered or damaged existing install. New installs keep the recorder stopped and
open a four-page, non-skippable setup flow inside the native shell. Existing installs retain their
previous startup behavior, and any successful settings save clears the in-process first-run flag so
a recreated WebView cannot reopen onboarding.

The wizard implements the approved Basics, Capture + recording, Games, and Review screens. Its
intentional first-run defaults are F6, the normal media directory, 10 GB, launch on startup,
output audio on/default/100%, microphone off/default/100%/mono, primary display, pause without a
game, 30-second replay, 1080p, Balanced quality, and 60 FPS. Supported game profiles come from the
real plugin catalog and start enabled according to their profile defaults. Other Games runs the
existing installed-game detector inline and converts selected results through the same custom-game
normalization used by Settings. Device enumeration, display enumeration, mic testing, folder
selection, hotkey parsing, and range presets are also shared with the existing UI.

Finishing copies the wizard choices into the existing Settings model, calls the existing
`save_settings` transaction, and only then starts recording. Save failures remain visible without
dismissing the wizard. First-run background controls are inert, the titlebar remains available, and
keyboard Tab can leave the hotkey recorder.

Settings now includes a `Misc` tab with a `Play first-time wizard` action. It reopens the real
wizard, protects unsaved Settings edits with the existing discard warning, seeds wizard controls
from saved settings, and resets transient state so the setup flow can be completed more than once
in the same app session. Detected Other Games support Select all, and checked games stay staged
until the final save; the redundant `Add selected games` step was removed.

Persistence/startup classification and UI contracts cover the new behavior. Full workspace tests,
fresh-cache warning-denied workspace Clippy, JavaScript syntax checks, formatting, and diff checks
are green. A development build launched against isolated `%APPDATA%`; Basics, device-backed Capture
+ recording, supported Games, and a real installed-game scan were visually checked at the normal
1200×760 inner window size. Manual retest: walk all four pages, optionally test the microphone and
game detector, finish setup, confirm recording starts, then restart and confirm the wizard does not
return.

---

## Checkpoint (2026-08-05): sparse-capture GOP watchdog

Plan: `docs/superpowers/plans/2026-08-05-sparse-capture-gop-watchdog.md` (`0d5ec35`).

A live Automatic/AMD AMF H.264 session stopped with `encoder did not produce a keyframe before
pending GOP duration exceeded 10.0 seconds`. The encoder was healthy: WGC can deliver no frames
while the captured display is static, but the recorder's missing-keyframe watchdog measured the
capture PTS span. AMD's GOP setting counts encoded frames, and its variable-rate MFT samples can
legitimately span the preceding static interval, so sparse input looked like a stalled encoder.

`Recorder` now measures unsealed GOP progress using encoded frame count and the first sample's
configured nominal duration. Later variable-rate sample durations and wall-clock PTS gaps no
longer advance the failure clock. The combined video/audio 64 MiB byte guard remains unchanged,
and a continuously emitting encoder that stops producing keyframes still fails after ten seconds
of encoded frame time.

The exact sparse-timestamp red/green regression, all 46 pipeline tests, full workspace tests,
fresh-cache capture Clippy, and warning-denied workspace Clippy are green. Manual retest: start
recording, leave the captured display static for at least 15 seconds, resume activity, and confirm
the recorder remains active and can save a replay.

---

## Checkpoint (2026-08-04): Slim-core destroyable WebView + on-demand FFmpeg

Plan: `docs/superpowers/plans/2026-08-04-slim-core-webview-ffmpeg.md`.

### What shipped in-tree
- **Milestone A:** `"create": false` cold autostart; async tray/close `Destroying`→`Destroyed` with queued open; per-window frontend readiness generations + durable status/warning replay.
- **Milestone B:** capability matrix; managed-runtime verifier separate from `locate()`; single-flight ensure/cancel/status with `archive_size`; replaceable FFmpeg encoder cache; **regular** `tauri.conf.json` no longer embeds `ffmpeg/` (standalone still may).

### Budgets / measurements
- Idle hard: **zero** Clipline WebView2 children; tree **PWS ≤120 MiB** (≤90 stretch). Harness CSV: destroy/autostart **70.9–92 MiB** PWS; recorder-stopped control **~14–15 MiB** PWS.
- Same-process destroy rebound ≤ first destroy +15 MiB (cycle + final). Absolute commit / cross-process cold deltas = telemetry only.
- Regular installer measured **9.35 MiB** (`9,808,604 bytes`) `Clipline_0.1.45_x64-setup.exe` after dropping `ffmpeg/` — under ≤**25 MB** hard gate; no `avcodec-*.dll` / no `avcodec` substring in setup. (Measure build used `createUpdaterArtifacts: false` locally; signed release build should stay in the same ballpark.)

### Core vs Optional
- **Core:** WGC/WASAPI, Hybrid MP4, MFT H.264, LoL markers, library/trim, Cloud, tray/hotkeys, on-demand FFmpeg *installer* UX.
- **Optional / separate:** managed FFmpeg bytes, standalone Fixed Version WebView2, osu! enrichment, disk replay (opt-in), native HEVC/AV1 preview, FFmpeg-free share export (deferred).

### Operator notes
- Tray/close destroys the UI; Open recreates and rehydrates via `frontend_ready`.
- `ensure_ffmpeg_runtime` is no-op only for `ManagedVerified` LOCALAPPDATA trees; PATH/override = `ExternalUnmanaged`.
- Do **not** default disk replay on; Cloud stays Core.

### PR #139 review fixes (2026-08-05)
- Managed FFmpeg install/status/cancel is reachable from Settings and from blocked poster, audio-preview, and shareable Copy Clip actions; successful installs refresh encoder capabilities and retry the blocked action.
- Cancellation remains worker-owned through extraction and crosses a locked, non-cancellable publish boundary; failed/cancelled workers clean staging before publishing terminal state.
- Runtime hashing streams in 64 KiB chunks, extraction hashes while copying, status verification runs off the async/UI thread, and successful publication reuses staged verification instead of rereading the full runtime tree.
- A failed WebView destroy restores the live frontend readiness generation and lifecycle mode instead of leaving the shell stuck in `Destroying`.
- The standalone/offline Tauri overlay retains the FFmpeg verification preflight even though the regular slim config no longer runs or bundles it.

---

## Checkpoint (2026-08-04): Nightly 0.1.45

Plan: `docs/superpowers/plans/2026-08-04-nightly-0.1.45.md` (`59a358b`).

Nightly 0.1.45 publishes **#138**, the review-playback first-frame flicker fix. New finalized
recordings no longer preserve positive internal video gaps shorter than the preceding frame as
empty MP4 edits. Instead, the shared writer extends that preceding frame through the unpresentable
rounding gap while retaining leading gaps, frame-sized or larger gaps, all audio gaps, and backward
timestamp rejection. Trims and selected-audio remuxes use the same normalized writer path.

PR #138 merged to `develop` as `673cde5` after green Ubuntu, Windows, and Greptile checks and a
non-blocking review. Microsoft's live Fixed Version selector now offers 151.0.4129.59 for x64, so
the standalone runtime advances from 150.0.4078.83 for this release. The official x64 CAB is
304,114,944 bytes with SHA-256
`056858a027a7bf29893b6013c0eb0c6ea7e29755a20c9d043be469d9d78657dc`.

Both release-input preflights, full workspace tests, fresh-cache warning-denied workspace Clippy,
release-version metadata validation, and the release-only diff check are green.

**Published** on the rolling `nightly` prerelease from `develop` commit `1493535`, seven assets.
The `nightly` tag and release both resolve to full commit
`14935359c9e865db4e6ad7a406c8119b4a235ff7`. Every public asset was downloaded again and matched
the staged SHA-256 digest and GitHub's reported digest.

| asset | bytes | sha256 |
| --- | ---: | --- |
| `Clipline_0.1.45_x64-setup.exe` | 54,318,090 | `6a56ef81be050e486d1cee60ce6dc66ad3a656c8e62dce13852aa116aec04a4c` |
| `Clipline_0.1.45_x64-setup.exe.sig` | 420 | `c5a72cbe2e20b6d38c7c4ed7cde8132a35c50032fb856397622ea968051a2117` |
| `Clipline_0.1.45_x64-standalone-setup.exe` | 282,607,582 | `0a102ec9cf135dc65fd513b2aafd630fc69a3127ac7e2bfb90651c697803c893` |
| `Clipline_0.1.45_x64-standalone-setup.exe.sig` | 436 | `02868fe3b820a6121876ad285d32bb31b38e45cc6df7ceae4d5556416338b589` |
| `latest.json` | 1,407 | `6df7c9c5a4d10dc7b06c0157eb2b27a8de41ed2913602ba1d496a9ae7652f64a` |
| `latest-standalone.json` | 1,433 | `04e104c2e29d4e8ef8f362d74628c2c6bbb00271092cf88ad8be046661efa8d1` |
| `release-notes-0.1.45.md` | 2,446 | `783c31eca2c48c99bb2146c69e00229c8c6967af75869431a0638727c600160e` |

Both downloaded manifests parse as version 0.1.45, point at their expected rolling release URLs,
and contain the exact downloaded sidecar signatures. Both public installers verify under the
updater public key compiled into Clipline; both crossed installer/signature pairs are correctly
rejected.

The standalone installer was extracted without installation and launched against isolated app
data. Its packaged 0.1.45 app and all seven WebView2 children used the bundled, Authenticode-valid
Microsoft 151.0.4129.59 runtime. The reporter's 29.007-second H.264 clip loaded with both audio
tracks selected and played through `ended` with monotonic media time and no media, page, console,
or app-log error. Runtime probing enabled H.264 and correctly left unavailable HEVC and AV1
disabled on this machine.

GitHub CI does not run on the release-only push to `develop`. The release commit's application
source is identical to CI-green merge `673cde5`; its delta is limited to version metadata, WebView2
runtime metadata/configuration, and release documentation.

## Checkpoint (2026-08-04): review video edit-list flicker

Plan: `docs/superpowers/plans/2026-08-04-review-video-edit-list-flicker.md` (`77fa3de`).

An affected 29.007-second user clip flashes its stale initial frame twice during playback in
Clipline but not in a desktop player. Its H.264 stream has 1,740 unique, strictly ordered decoded
frames and no decode errors, so the apparent first-frame repeats are not present in the source
picture data. The video track instead has three internal empty edits, each exactly one 90 kHz tick
(11 microseconds), at approximately 5.003, 14.506, and 27.006 seconds. Capture-clock rounding can
put a fragment one tick beyond the MP4 writer frontier; desktop players conceal these unpresentable
gaps, while WebView2 can briefly expose its stale initial video surface at an edit boundary.

`HybridMp4Writer::set_track_decode_time` now absorbs only positive internal video gaps shorter than
the preceding sample by extending that sample's duration. It updates the duration run, media
duration, presentation run, and next decode time together. Leading video gaps, frame-sized or larger
video gaps, every audio gap, and backward-time rejection are unchanged. Because trimming and audio
track selection use the same writer, remuxes of older affected clips are normalized as well as new
recordings.

The supplied clip remuxes without a video edit list while retaining all 1,740 video frames and both
1,450-packet Opus tracks. Decoded video has the same aggregate MD5 before and after, total container
duration is unchanged, and video duration grows by only the three absorbed ticks (33 microseconds).
The focused red/green regression, complete `clipline-mp4` suite, full workspace tests, targeted
formatting, diff check, and fresh-cache warning-denied workspace Clippy are green. A fresh
development build was launched successfully; the user-visible WebView playback check remains.

Manual retest: play a new 30-second output-plus-microphone clip end to end, repeat with only Output
Audio selected, then seek and replay while checking for flashes and A/V drift. A deliberate capture
interruption lasting at least one video frame should still remain an explicit timeline gap.

## Checkpoint (2026-08-02): Nightly 0.1.44

Plan: `docs/superpowers/plans/2026-08-02-nightly-0.1.44.md`.

Nightly 0.1.44 publishes **#133**, the support-report-driven multitrack audio mixing and diagnostic
redaction reliability release.

User-visible since 0.1.43: cloud uploads and normal Copy exports can combine output and microphone
tracks whose starts are offset by less than one Opus packet without producing overlapping or
backward MP4 timestamps. Mixing now uses one continuous 48 kHz timeline, consumes each track's own
pre-skip, preserves long gaps without manufacturing silence packets, and rejects corrupt MP4 versus
decoded Opus duration mismatches beyond the supported one-tick quantization tolerance. Support
reports also preserve valid JSONL after redacting JSON-escaped Windows paths.

PR #133 is merged to `develop` as `d149b50` with green Ubuntu, Windows, and Greptile checks; both
Codex review threads were answered and resolved before merge. Microsoft's current official Fixed
Version download remains 150.0.4078.83, matching the staged standalone runtime, and its required
review date was refreshed for this release.

**Published** on the rolling `nightly` prerelease from `develop` commit `bd76a6e`, seven assets.
Every asset was downloaded again from the GitHub release and matched the staged SHA-256 digest.
Both downloaded manifests parse as version 0.1.44, point at their expected rolling release URLs,
and contain the exact downloaded sidecar signatures. Both downloaded installers verify under the
updater public key compiled into Clipline; crossing the standalone signature onto the regular
installer is correctly rejected.

| asset | bytes | sha256 |
| --- | --- | --- |
| `Clipline_0.1.44_x64-setup.exe` | 54,320,782 | `820df11c22acfbe93423685281d364d72c0e96e61b0affec5613ad79ea09c8fe` |
| `Clipline_0.1.44_x64-standalone-setup.exe` | 277,012,373 | `4700e21da8b1f65b5d05501b7751e74d7bc4d13089f60a11a9c897c3969e17ca` |

GitHub CI does not run on version-only pushes to `develop`; the release commit's application source
is identical to CI-green merge `d149b50`. Its delta is limited to three version strings, WebView2
review dates, and release documentation. Full workspace tests, a clean-cache warning-denied
workspace Clippy run, both release-input preflights, manifest validation, and local updater
signature verification passed before publication.

The standalone installer was extracted without installation and launched against isolated app data.
Its packaged 0.1.44 app and all six WebView2 children used the bundled 150.0.4078.83 runtime. It
loaded a ten-second H.264 clip with separate output and microphone Opus tracks, prepared both audio
previews, showed `2/2 selected`, and played through `ended` with no media, page, or global error.
Runtime probing reported H.264 and AV1 support, correctly left HEVC unavailable on this machine, and
found three encoders.

## Checkpoint (2026-08-02): valid redacted support JSON

Plan: `docs/superpowers/plans/2026-08-02-support-log-json-redaction.md` (`4adecf7`).

Support report log entries remain valid JSONL when diagnostic strings contain Windows drive paths.
The shared path regex previously consumed only the first backslash of JSON's doubled `\\` path
separator, leaving an invalid escape such as `\U` in the redacted line. Path separators now consume
one or more adjacent backslashes, covering both plain diagnostic text and JSON-escaped strings while
preserving the existing redaction and bundle structure. The fix and exact parse-after-redaction
regression are commit `e386e64`.

The focused red/green regression, complete application suite, full workspace suite, fresh-cache app
Clippy, warning-denied workspace Clippy, formatting, and diff checks are green.

## Checkpoint (2026-08-02): staggered selected-audio mixing

Plan: `docs/superpowers/plans/2026-08-02-staggered-audio-mix.md` (`7e71a86`).

A 0.1.43 support report reproduced `unsupported mp4: overlapping or backward sample presentation
times` when both output and microphone audio were selected for a cloud upload or shareable Copy.
The native Opus mixer emitted one full packet at every source packet start, so two valid tracks
offset by less than one packet produced overlapping mixed samples that the final remux correctly
rejected.

The shared file-backed and in-memory mixer now maps every selected track onto one continuous
48 kHz timeline and emits fixed, non-overlapping 20 ms packets. It handles sub-packet track offsets,
consumes each source track's own Opus pre-skip, tolerates normal 959/961-tick container-duration
quantization, preserves long gaps without encoding thousands of silent packets, and bounds decoded
packet expansion before allocation. The fix is commit `5037755`.

PR #133 review follow-up (`docs/superpowers/plans/2026-08-02-pr133-review-followup.md`,
`fee4e01`) additionally rejects MP4 sample-table and decoded Opus duration mismatches beyond the
supported ±1-tick quantization before PCM can be cropped or padded (`a11cf49`). Codex's separate
commit-history comment required no rewrite: both original plan commits already precede their
respective implementation commits and each fix retains its own rollback boundary.

The exact staggered-track file regression, the complete `clipline-mp4` suite, full workspace tests,
fresh-cache crate Clippy, and warning-denied workspace Clippy are green. The report's separately
observed malformed redacted-log JSON is fixed by the checkpoint above.

## Checkpoint (2026-07-29): Nightly 0.1.43

Plan: `docs/superpowers/plans/2026-07-29-nightly-0.1.43.md`.

Nightly 0.1.43 publishes the merged sharing and cloud-upload reliability work: **#130** (shareable
H.264/AAC clipboard exports), **#131** (stable review state and feedback after cloud uploads), and
**#105** (canonical public Cloud share URLs).

User-visible since 0.1.42: normal Copy now produces a broadly compatible H.264/AAC-LC MP4 while
preserving the selected audio, and Shift+click copies the untouched original; HEVC/AV1 sources use a
proven H.264 encoder fallback. Cloud upload completion keeps the current review open across
equivalent Windows path spellings, confirms intentional local deletion after foreground refresh,
and retains cleanup errors. Public and unlisted Cloud clips copy the API-provided canonical public
URL so chat clients can unfurl title, poster, and video metadata; private clips expose no share-link
action.

All three PRs are merged to `develop` with green Ubuntu, Windows, and Greptile checks. Microsoft's
current official stable WebView2 release remains 150.0.4078.83, matching the staged standalone
runtime; its required review date was refreshed for this release.

**Published** on the rolling `nightly` prerelease from `develop` commit `7af00d5`, seven assets.
Both installers were downloaded again through their public release URLs, matched against the staged
bytes, and verified using the signature in the corresponding downloaded manifest under the updater
public key compiled into Clipline:

| asset | bytes | sha256 |
| --- | --- | --- |
| `Clipline_0.1.43_x64-setup.exe` | 54,315,070 | `b4e4cb2aa8a8b3ff98be5de511299b04045c42b9d4a11c8ccfde00354b8bbd4d` |
| `Clipline_0.1.43_x64-standalone-setup.exe` | 276,912,747 | `4efdfa6cbbc23fe2d9c806e833df82286047fa150209e6bed4d2550c5576393a` |

GitHub CI did not run on release commit `7af00d5` because the workflow triggers on pull requests and
pushes to `main`, not version-bump pushes to `develop`; GitHub reports zero check runs for that SHA.
The release commit's application source is identical to CI-green merge `29b5109`. Its delta is
limited to three version strings, the WebView2 review date, and two release documents. Full
workspace tests, fresh-cache warning-denied Clippy, and both release-input preflights passed
locally before packaging. The published standalone installer was then extracted into an isolated
directory without installation: its packaged app launched seven processes from the bundled
150.0.4078.83 runtime, played a 10-second H.264 clip plus both output and microphone Opus sidecars
through `ended` with no media/page error, and reported H.264, HEVC, and AV1 decodable.

## Checkpoint (2026-07-29): PR #131 Codex review follow-up

Plan: `docs/superpowers/plans/2026-07-29-pr131-codex-review-followups.md`.

Two actionable Codex findings are fixed. When an authoritative Library refresh pairs equivalent
Windows paths such as `\\?\D:\…` and `D:\…`, the refreshed clip metadata is now merged while the
active review keeps its original path spelling. This prevents the video source and later
path-keyed actions from being silently rewritten during alias reconciliation.

Cloud-upload feedback also respects background refresh deferral. If an upload finishes while the
Library is not foreground-current, its cleanup error and `Delete local after upload` confirmation
are retained in one bounded pending slot. The next completed foreground refresh first reconciles
the viewer, then publishes the deferred feedback exactly once. The other Codex comments required
no new change: plan and implementation were already separate commits, and cleanup-error ordering
was fixed in the preceding Greptile follow-up.

Focused red/green UI contracts, `cargo test --workspace`, and a fresh-cache
`cargo clippy --workspace --all-targets -- -D warnings` are green.

## Checkpoint (2026-07-29): PR #131 review follow-up

Plan: `docs/superpowers/plans/2026-07-29-pr131-review-followup.md`.

Greptile's P1 review finding was valid: `uploadClipToCloud` published the backend cleanup error
before the authoritative Library refresh, whose partial-scan warning handler owns the same global
error surface and could overwrite the more actionable cleanup failure. Cleanup errors are now
republished after `await refresh()`, while uploads without a backend error continue to leave any
Library scan warning visible.

The UI contract regression requires that ordering. Its red/green run, `cargo test --workspace`, and
a fresh-cache `cargo clippy --workspace --all-targets -- -D warnings` pass are green.

## Checkpoint (2026-07-28): cloud upload review completion

Plan: `docs/superpowers/plans/2026-07-28-cloud-upload-review-completion.md`.

Cloud upload completion no longer ejects freshly exported trims from review merely because Windows
spells the canonical export path as `\\?\D:\…` and the authoritative Library rescan spells the same
path as `D:\…`. Active-clip reconciliation now uses the existing Windows-aware path identity helper
instead of raw string equality, so uploads that preserve the local MP4 keep the viewer open.

`CloudUploadResult` now explicitly reports `local_deleted`. When `Delete local after upload`
successfully removes the primary MP4, the authoritative refresh intentionally returns to the
Library and the global notice surface confirms `cloud upload ready · local copy deleted`. If cloud
media verification or primary deletion fails, the local review remains open; backend post-upload
or cleanup errors are surfaced globally instead of being hidden behind a generic ready status.
Primary deletion is still reported accurately if a later sidecar cleanup fails.

Focused red/green regressions cover path-equivalent refresh behavior, the post-delete notice
contract, cleanup-error visibility, and upload-result serialization. `cargo test --workspace` is
green and a fresh-cache `cargo clippy --workspace --all-targets -- -D warnings` pass is clean.

## Checkpoint (2026-07-28): shareable clipboard PR review follow-ups

Plan: `docs/superpowers/plans/2026-07-28-pr130-review-followups.md`.

PR #130 review feedback is implemented. HEVC/AV1 share exports now reuse the capture pipeline's
proven per-backend H.264 rate-control flags at an 8 Mbps target and 16 Mbps buffer, instead of
relying on encoder defaults. The entire ordered encoder fallback sequence shares one
duration-scaled deadline, so each failed backend cannot restart the full timeout. Cache pruning now
recognizes both legacy `.mp4.tmp` files and the unique, potentially nested
`.mp4.<pid>.<counter>.tmp` intermediates left by abandoned exports while retaining malformed or
unowned lookalikes.

The cache namespace is `share-export-v3-aac-h264-cbr8m`, invalidating prior HEVC/AV1 transcodes made
with default encoder settings. Focused regressions, the CI-mode full workspace suite, and
warning-denied workspace Clippy pass. The unrelated interactive-desktop WGC device test timed out
waiting for its first frame in both the initial non-CI workspace run and an isolated retry; it is
self-skipped under the repository's documented CI condition and no WGC code changed in this work.

## Checkpoint (2026-07-27): shareable clipboard export

Plan: `docs/superpowers/plans/2026-07-27-shareable-clipboard-export.md`.

The review Copy button now prepares a broadly shareable MP4 by default. It preserves the current
audio selection, natively remuxes one selected Opus track or mixes multiple selected tracks, then
uses the separately spawned bundled LGPL FFmpeg process to encode one 48 kHz stereo AAC-LC track.
H.264 video is stream-copied without quality loss. Explicit HEVC/AV1 recordings are detected from
their MP4 sample entries and tried through the machine's proven FFmpeg H.264 encoders instead of
silently producing another incompatible file. Shift+click copies the untouched source MP4 with all
original codecs and tracks.

The cache namespace is `share-export-v3-aac-h264-cbr8m`, so earlier Opus-in-MP4 clipboard exports
cannot be reused. FFmpeg work runs off the UI thread, drains bounded diagnostics, has a
duration-scaled hard timeout, cleans intermediate/partial files, and publishes the cache entry
atomically. The button tooltip documents Shift+click, and progress/success text distinguishes
shareable and original copies.

The pinned release FFmpeg runtime was staged and a real cached H.264/Opus clip was converted to
H.264 plus one `mp4a` AAC-LC 48 kHz stereo track. Focused MP4/library/UI tests pass, the full
workspace suite passes, and warning-denied workspace Clippy is clean. The first workspace run was
concurrent with Clippy and triggered the existing real-device WGC timing assertion under load; the
device test passed immediately in isolation and the sequential full workspace rerun passed.

## Checkpoint (2026-07-26): Nightly 0.1.42

Plan: `docs/superpowers/plans/2026-07-26-nightly-0.1.42.md`.

Nightly 0.1.42 publishes the merged memory-footprint and review-audio work: **#109** (clip-start audio
repeat), **#106** (replay retention, hidden-webview memory, split meter), and **#107** (memory
follow-ups, native software H.264 MFT, FFmpeg thumbnail hardening).

User-visible since 0.1.41: the split-second audio repeat at the start of every clip is gone; replay
memory tracks the footage a save can use rather than the byte budget's 2× overshoot headroom (85.8 MB
→ ~45 MB retained at the dev machine's settings, recording process 147–180 MB → ~103 MB); hiding to
the tray releases WebView2 rendering resources instead of keeping them resident (tray-idle tree ~335 MB
→ ~155 MB) and a cold autostart no longer renders indefinitely; the RAM meter separates Clipline's own
process from child processes; and large-library rendering is bounded with self-recovering thumbnails,
taskbar lifecycle recovery without focus, and active clip sources protected during upload.

All three PRs were manually verified on hardware before release: warm-path clip open and scrub for the
audio fix, the rail meter and a five-minute tray hide with hotkey saves for the memory work, and a
full pass on the merged `develop` build.

**Not in this release:** the scrub and track-switch audio alignment work is specification only and
deliberately unimplemented — issue #110, branch `review-audio-alignment`. Those defects predate this
release and are unchanged by it. The brief echo when switching audio tracks mid-playback is a known,
accepted residual.

**Published** on the rolling `nightly` prerelease from `develop` commit `e97c750`, seven assets. Both
installers were verified after publication by downloading them from their public URLs and confirming
the signature in each manifest validates the downloaded bytes under the updater public key in
`tauri.conf.json` — the same check the updater performs:

| asset | bytes | sha256 |
| --- | --- | --- |
| `Clipline_0.1.42_x64-setup.exe` | 54,308,414 | `7a0e000d58bd90cd6c3651bcff7431d58ce5a66f596b2c4a52e3d13f574628fa` |
| `Clipline_0.1.42_x64-standalone-setup.exe` | 276,937,081 | `69476973aedad680c7f9b74623b90f13eeabe5864b8c24a97c746ae52438b258` |

GitHub CI did not run on `e97c750`: `ci.yml` triggers only on pushes to `main` and on pull requests,
so a version-bump push to `develop` produces zero checks. Gates were run locally instead — 1210
workspace tests, fresh-cache warning-denied Clippy, and both release-input preflights — and the
release commit's code is byte-identical to CI-green merge commit `ae34662`, the delta being three
version strings and two docs. `docs/release-updates.md` now records this and the other release traps.

## Checkpoint (2026-07-25): FFmpeg thumbnail reliability

Plan: `docs/superpowers/plans/2026-07-25-ffmpeg-thumbnail-reliability.md`.

Thumbnail failures on source builds and some installed machines had three indistinguishable causes:
the executable search omitted the installed `%LOCALAPPDATA%\Clipline\ffmpeg` runtime, release
bundling could silently consume the gitignored README-only staging directory, and the FFmpeg child
wrote its temporary JPEG beside the clip where Windows Controlled Folder Access can deny an
independently distributed process. The gallery swallowed every backend error into the same gradient
fallback, leaving users no recovery path.

FFmpeg discovery now checks the installed LocalAppData runtime before the legacy roaming development
bundle and PATH, deduplicating candidates while keeping the explicit and packaged overrides first.
Poster extraction emits one bounded MJPEG through concurrently drained stdout/stderr pipes; Clipline
itself owns and atomically publishes the sibling temporary file. Missing-runtime errors are logged
once by bounded category, without clip paths or FFmpeg stderr, and show a persistent Library warning
with an in-process `Retry thumbnails` action. Other per-media failures remain local gradient
fallbacks.

Every Tauri bundle now runs the offline `scripts/verify-ffmpeg-resource.ps1` preflight. It rejects
missing, unexpected, modified, reparse-point, provenance-mismatched, version-mismatched, GPL, or
nonfree payloads before packaging. The source payload remains intentionally gitignored and the
existing pinned staging workflow remains the only networked release step.

Validation is green:

- `cargo test --workspace`
- cold-cache `cargo clippy --workspace --all-targets -- -D warnings`
- JavaScript syntax, PowerShell parser, formatting, and `git diff --check`
- installed payload verification, with README-only staging rejected before bundling
- Computer Use E2E: a debug build regenerated a poster from the installed LocalAppData fallback;
  removing every runtime produced the warning, then restoring FFmpeg and clicking Retry regenerated
  the missing poster without an app restart

## Checkpoint (2026-07-25): PR #107 review follow-ups

Plan: `docs/superpowers/plans/2026-07-25-pr-107-review-follow-ups.md`.

The native lifecycle no longer gates `Foreground` publication on fallible show, restore, or focus
calls. Both focus and resize events now reconcile the authoritative minimized state, so taskbar
restores that omit a focus event re-show the WebView2 controller before publishing `Foreground`.

Large-library renders build one normalized local-path index, use constant-size gallery identity
inputs already collected during filtering/sorting, and expire negative poster entries after 30
seconds. FFmpeg discovery caches successes only. The bulk-selection label now says “Select page.”

Upload leases now track the underlying Windows file identity, including hard-link and junction
aliases. Delete/rename return an intentional “clip is uploading” error, and quota GC protects active
sources while continuing with the next deletable clip. Software MFT caller-owned output samples are
reused after clearing attributes and logical length, and activated MFTs call `ShutdownObject` on
normal drop and constructor-error unwind. Drain continues to pass the input stream ID, matching
Microsoft's corrected Media Foundation documentation.

The second review tightened test and measurement portability: child WebView2 processes that exit
mid-sample are skipped atomically while root-counter failures stay fatal, the poster timeout fixture
re-invokes the Rust test binary instead of depending on PowerShell/PATH, and one Boa regression now
cross-checks gallery path keys against player path equality. The active-upload identity lock remains
intentional: it linearizes kernel lease acquisition with registry publication, and measured local
contention was only tens of microseconds per candidate open.

Validation is green:

- `cargo test --workspace`
- fresh-cache `cargo clippy --workspace --all-targets -- -D warnings`
- real 30-frame WARP software-MFT encode/reuse regression
- JavaScript syntax, PowerShell parser, formatting, and `git diff --check`
- Computer Use native minimize/restore smoke; the restored app rendered the full library instead of
  a blank WebView and was left open in `Waiting`

## Checkpoint (2026-07-25): native Microsoft software H.264 MFT

Plan: `docs/superpowers/plans/2026-07-25-native-software-mft-h264.md`.

The `MfSoftware` probe result is now an executable native path rather than an advertised-but-skipped
candidate. `SoftwareMftH264Encoder` selects only Microsoft's inbox synchronous H.264 MFT, converts
the captured GPU BGRA frame to CPU NV12, feeds aligned system-memory samples, and emits the same
AVCC packet/config contract as the existing async hardware MFT. It preserves the caller's
timestamps and durations, honors transforms that supply their own output samples or require caller
allocation, refreshes output-stream requirements after stream changes, and drains with the actual
input stream ID. The existing FFmpeg `h264_mf -hw_encoding 0` route remains the separate-process
fallback when available.

The Windows integration regression uses a WARP D3D device to encode 30 frames at 640x360 through
the advertised inbox transform, checks exact timestamp cardinality, AVCC framing, SPS/PPS, and the
first IDR. It skips when that optional Windows component is absent (notably some Windows Server CI
images); the dev machine exercises it for real. The service routing regressions prove MFT software
selection cannot silently fall through to FFmpeg.

Validation is green:

- `cargo test --workspace`
- fresh-cache `cargo clippy --workspace --all-targets -- -D warnings`
- focused real WARP software-MFT encode and application routing tests
- Computer Use E2E: the app reported `Software · H.264`, recorded the 1280x800 display, saved and
  played a 29.4-second two-audio-track MP4, then stopped and finalized without an encoder error.
  The temporary games-only setting change was restored and the app was left armed in `Waiting`.

The E2E artifact is
`C:\Users\dain9\Videos\Clipline\2026-07-25 06-44\clip_1784987122.mp4` (1.7 MB). Its optional audio
preview sidecar reported that FFmpeg was unavailable, but native WebView2 playback of the main MP4
and its selectable audio tracks succeeded; sidecar extraction is separate from recording.

## Checkpoint (2026-07-25): Memory follow-up after PR #106

Plan: `docs/superpowers/plans/2026-07-24-memory-follow-up.md`.

This follow-up supersedes the retention and save-path implementation details in the earlier
memory-footprint checkpoint below.

**Save Replay no longer duplicates the encoded window.** A memory-backed save borrows the
`Segment` payloads already owned by the ring. A disk-backed save keeps payload-free segment
descriptors, validates the selected file region, opens one segment at a time, and streams samples
through the MP4 writer's 64 KiB transfer buffer. Audio-prefix selection is a metadata view rather
than a mutation/copy. RAM and disk paths have a byte-identical multitrack regression, including a
mid-window audio prefix.

**The ring now retains the exact usable replay span.** Duration pressure keeps the latest keyframe
at-or-before the requested cutoff across the existing ring plus the incoming segment; there is no
fixed 15 s retention margin. Byte pressure can still advance to the next keyframe, because the hard
memory cap wins during genuine encoder overshoot. The persisted `buffer_seconds` field remains only
as a normalized compatibility mirror of `replay_window_s`; runtime no longer treats it as a second
setting. The capture seed allocation is moved instead of cloned, sealed GOP payload/sample vectors
are exact-sized, and the application-owned WGC latest-frame queue is one frame (the required WinRT
frame-pool depth remains two).

**Hidden UI work is revisioned and bounded.** Native state records `Foreground`, `Tray`, or
`Taskbar` with a monotonic revision. `frontend_ready` returns a snapshot after the lifecycle
listener is installed, and revision-gap recovery forces teardown/refresh if an event was missed.
Once native hide/minimize succeeds, background entry invalidates local/cloud work, stops microphone
testing and Web Audio, releases review media, disconnects poster observation, removes both gallery
roots, hides the controller, and requests WebView2 `Low`. Foreground restore uses `Normal`, restores
the controller, and coalesces deferred work into one refresh. Async settings/device loads, cloud
media, rename restoration, posters, and boot work all reject stale lifecycle generations.

**Large libraries, posters, uploads, and session metadata have explicit bounds.**

- Local and cloud galleries render at most 60 cards per page. Off-page/inactive image sources and
  DOM are released; selection remains path-keyed across pages.
- Poster URL/unavailable entries use a 120-entry LRU. Cached and uncached posters share the same
  viewport gate. Frontend requests and backend FFmpeg extraction are each capped at two, backend
  extraction is single-flight per canonical clip, FFmpeg discovery is cached, and children have a
  30 s execution timeout followed by kill/reap, with 64 KiB bounded stderr.
- Multipart uploads reopen a bounded file slice for every attempt and stream it instead of
  allocating a server-sized part (previously up to 64 MiB). Two top-level uploads may run at once.
  A Windows sharing lease keeps the source immutable from validation/checksum through every
  direct/proxy retry.
- Full-session MP4 duration entries are aggregated online, all-sync tracks keep no `stss` vector,
  video stores only sync-sample numbers, chunk offsets use 8 bytes each, and `stsc` changes are
  run-length encoded as fragments arrive. Replay-only game markers prune against the recorder's
  actual oldest retained media timestamp, preserving keyframe lead-in and encoder lag.

Validation on the combined tree is green:

- `cargo test --workspace`
- fresh-cache `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt --all -- --check`
- JavaScript syntax checks, PowerShell parser check, and `git diff --check`

`scripts/measure-save-replay-memory.ps1` now samples root/child private working set and private
commit every 50–100 ms during repeated real saves, with GPU local/non-local allocations recorded
separately. **No new before/after numbers are claimed yet**: the minimum/default/maximum and
RAM/disk matrix needs real capture footage and deliberate settings changes. Do not substitute the
older resident-set measurements below for that run.

Remaining conditional risk: ISO-BMFF requires one final sample-size entry per variable-sized
sample. `TrackState::sizes` and the serialized final `moov` therefore still grow with a multi-hour
full session and briefly coexist at finalize. Measure that metadata at the intended maximum session
length before adding a file-backed table spool or rebuilding tables from the already-written
fragment metadata; either is a larger crash-recovery change than the online table compression.

Manual acceptance checklist:

1. Let both RAM and disk replay modes fill, save repeatedly with output plus microphone/game audio,
   and verify playable files, duration coverage, audio sync, and no save-time memory spike near the
   ring size.
2. Exercise 5 s, default 60 s, and maximum 120 s replay settings; verify the compatibility field
   round-trips to the same value and saved coverage starts at the covering keyframe.
3. Hide to tray and taskbar while a poster/cloud request and microphone test are active, save by
   global hotkey, then reveal; verify the mic stays stopped, no blank window appears, and the
   gallery refreshes once without stale video reattaching.
4. Page through local/cloud libraries larger than 60 clips, including search/group/sort changes,
   cross-page selection, rename/delete, and account/media-root changes; verify counts, images, and
   poster cache invalidation.
5. Retry both direct and proxy multipart uploads and try to rename/delete the source while one is
   active; mutation should be rejected until the upload releases its lease.

## Checkpoint (2026-07-25): Memory footprint reduction

Plan: `docs/superpowers/plans/2026-07-24-memory-footprint-reduction.md`.

**Replay retention is now bounded by span as well as bytes.** `estimated_buffer_bytes` sizes the
ring with a 2× encoder-overshoot headroom so a bitrate spike cannot evict footage the save window
needs — but eviction was byte-only, so the headroom became a target instead of a cap: the ring grew
until it *reached* the budget, holding ~2× the usable footage, and ~5× when the encoder undershoots
on low-motion content (fewer bytes/second means more seconds fit under the cap). `planning::
eviction_plan` now resolves both bounds in one plan — larger count wins, then advanced to the next
keyframe so the front always starts a decodable GOP. Sequencing them separately would let the byte
bound, which has no keyframe awareness, strand a headless GOP. Retention is derived via
`replay_buffer_seconds`, never read from `AppSettings::buffer_seconds`: `save_to` normalizes only a
clone, and validation accepts `buffer_seconds == replay_window_s`, which would leave zero headroom.
Measured on the dev machine (30 s window, 720p Sharp): ring 85.8 MB cap → ~45 MB retained, app
process plateau 147–180 MB → ~103 MB, held across a 12-minute soak.

**WebView2 no longer stays fully resident in the tray.** `WebviewWindow::hide` hides only the
native window; the controller kept rendering with nothing on screen (hidden four minutes moved
child private working set by <1 MB, GPU pinned at 132.0 MB). Two changes: `Webview::hide/show`,
which reaches `SetIsVisible` through wry with no COM or new unsafe, and
`MemoryUsageTargetLevel::Low` on hide / `Normal` on reveal. Visibility alone reclaimed only ~20 MiB
and **missed the plan's 40 MiB gate** — kept anyway because not rendering an invisible window is
correct, and because `--autostart` skips `open_main_window` entirely and was rendering
indefinitely. `Low` cleared the gate by ~4.5×: 188.3 / 199.2 / 177.9 MiB, tray-idle tree resident
set ~335 MB → ~155 MB. `Low` keeps scripts and network alive, unlike `TrySuspend`; do not mix the
two models.

Three precision notes on that measurement, because the headline is easy to overstate:

- It is **trimmed from the resident set**, not proven released. Only private working set was
  sampled; the private-commit cross-check was not run, so decommit vs. page-out is unmeasured.
  `scripts/measure-hidden-webview-memory.ps1` records commit alongside it for a decisive re-run.
- 188.3 MiB is the **combined** visible→hidden effect of playback suspension, `SetIsVisible` and
  `Low`. Subtracting the visibility-only median puts `Low`'s increment near **168 MiB**.
- **Two confirmed runs plus one corroborating run.** Run 3's playback probe returned empty and its
  GPU ended at 33.8 MiB rather than ~5 MiB. The committed harness now fails closed on missing
  playback confirmation instead of measuring a partially-inflated state.

**The RAM meter reports app and children separately.** It previously summed the whole tree, so
~230 MB of webview sat on Clipline's own figure — during this work a WebView2 playback spike of
+110 MB read as a ring leak. Labelled "child", not "webview", because the walk also catches the
`ffmpeg.exe` child on the CPU encoder path.

Sharp edges found along the way:

- **`encoder_label` discards `EncoderApi`** — MFT and FFmpeg both render `AMD AMF · H.264`. An
  `encoder_selected` diagnostic now logs api/backend/codec. This machine resolves `api=Mft`, so
  the planned per-frame readback work (`nv12.rs`, `cpu_video.rs`) is **skipped**: frames stay on
  the GPU. Revisit only if the FFmpeg path becomes a default.
- **The meter cannot measure hidden-state savings** — `main.js` only polls while
  `!document.hidden`. Acceptance needs an external harness.
- **Measuring memory: pick the right metric.** Committed private bytes do not move when Windows
  trims a hidden process, so they cannot distinguish "decommitted" from "paged out"; private working
  set shows the resident change. Record **both**. A process-tree walk also needs a creation-time
  check, or PID reuse sweeps in unrelated processes — this machine runs ~19 `msedgewebview2`
  processes belonging to other apps, and an early harness reported an impossible 3,886 MB tree.
  The validated implementation is `scripts/measure-hidden-webview-memory.ps1`.
- **The in-app child-memory line does *not* have that PID-reuse protection.** `memory.rs` builds
  the tree from bare PID/parent-PID entries and then queries bare PIDs, so the child line and the
  legacy summed total can transiently include unrelated processes. The root-process headline — the
  number the meter now leads with — is unaffected. Pre-existing, but newly user-visible: worth a
  follow-up applying the harness's creation-time check to `child_process_ids_from_entries`.
- **Release still carries ~191 MB of IMAGE mappings** (debuginfo plus mapped system/WebView2
  DLLs), barely below debug's 205 MB. File-backed and shared, so it does not inflate private bytes.

Known-unwired, found but deliberately untouched: the buffer crate implements and tests the
"don't re-clip overlapping footage" smart mode (`exclude_before_s`), but the only `save_replay` call
site passes `None` (`service.rs:2267`), so consecutive saves overlap. That is a product decision,
not memory work.

The shell is **rail-only** — `ui/index.html` hardcodes `class="app rail"` and `styles.css` calls it
"the only mode now". Verify meter changes against the 64px rail; the wide-sidebar rules are vestigial.

Investigated and **not** a leak: `ENRICHMENT_PASSES` (`osu_api.rs`) is a per-root single-flight
lease registry removed on `Drop`, not an unbounded per-clip set. Bounding it would break the
single-flight behaviour it exists to provide.

## Checkpoint (2026-07-24): canonical public clip share URLs

Cloud upload records now treat `remote_clip_id` as authenticated remote identity and `remote_url`
strictly as the server-issued public share URL. Upload progress never synthesizes
`{public_origin}/clip/{clip_id}`; after processing, and again after the upload flow changes
visibility, the client reads `GET /api/v1/clips/{clip_id}` and persists
`ClipDetailResponse.public_url` verbatim. Public and unlisted clips therefore copy the canonical
`/c/c_...` URL used by unfurled Discord embeds. Private detail responses clear the saved URL, and
settings normalization removes both stale private URLs and legacy synthesized owner routes,
including routes saved under a previously configured host.

Private clips still remain in the Cloud library and dedupe against later upload attempts because
those behaviors key on `remote_clip_id`, not shareability. Their UI offers the authenticated
**Open cloud page** action but hides copy-link affordances and labels the missing public link
explicitly. Progress events omit absent `remote_url` fields so byte/status updates cannot erase a
freshly synchronized share URL. The authenticated `/clip/{clip_id}` route remains isolated to the
native open-page command and is never stored or copied as a share URL.
Native/API, settings-migration, DOM-free Cloud/player, and UI contract regressions cover the
transition matrix. The PR #105 Greptile follow-up also keeps a URL-less public/unlisted visibility
response in recoverable `uploaded_processing` state when the canonical detail refresh fails,
instead of terminally recording a public upload with no share action. Workspace tests and
fresh-cache warning-denied Clippy are green; a live Cloud upload plus Discord unfurl remains the
final deployment-dependent acceptance check.

## Checkpoint (2026-07-23): Nightly 0.1.41

Nightly 0.1.41 contains PR #103's WASAPI endpoint-loss recovery and PR #104's private diagnostic
reporting workflow. Recoverable output, process-loopback, and microphone endpoint invalidations now
re-activate in place without aborting the recorder, reuse process identity safely, preserve A/V
timing through the outage, and emit bounded lost/recovered diagnostics.

Clipline now keeps bounded structured desktop logs, captures panic and frontend failures, and
provides a Settings > Support workflow for preparing, previewing, saving, discarding, and explicitly
submitting a sanitized diagnostic bundle. Reports exclude recordings, credentials, and raw settings,
remain local until the user confirms submission, and can only be sent to the compiled-in official
private intake endpoint. Review follow-ups cover redaction, staging cleanup, cancellation, retries,
upload validation, coherent UI state, and keyboard navigation. Both changes passed workspace tests,
fresh-cache warning-denied Clippy, Windows and Ubuntu CI, dependency security, and manual acceptance.

## Checkpoint (2026-07-23): WASAPI device-loss recovery

A mid-recording endpoint invalidation no longer aborts the recorder. Previously a single
`AUDCLNT_E_DEVICE_INVALIDATED` (0x88890004) from `GetNextPacketSize`/`GetBuffer` propagated as
`CaptureError::DeviceLost`, killed the service loop ("recording: capture device lost…"), and
failed a second time when shutdown drained the same dead client ("…additionally, finish: …").
Typical trigger: the default render endpoint re-enumerating (headphone/USB/Bluetooth
disconnect, monitor audio power-cycle, default-device switch). An invalidated `IAudioClient` is
permanently dead; only re-activation recovers it.

`WasapiPcmCapture` now stores an `EndpointTarget` (output/process/microphone plus device id) and
re-activates it on a 1 s retry cadence after a recoverable HRESULT (0x88890004, 0x88890010
service-not-running, 0x88890026 resources-invalidated). While the endpoint is dead, the existing
idle-desktop silence machinery covers the outage: delivery idleness exceeds the quiet grace, the
assembler advances with capped silence, and the first packet from the re-activated endpoint
re-anchors on its QPC timestamp — A/V sync survives the gap with no new timeline code. A dead
client is no longer drained while it waits, so repeated poll failures cannot slide the retry
deadline forward. Failed activation attempts schedule from their completion time, preserving the
full 1 s cadence even after a 1.5 s process-loopback timeout.

Process-loopback targets store the process creation time as an instance identity and will only
re-activate while both PID and creation time still match, preventing PID reuse from redirecting a
track to another process. Explicit output and microphone targets recover strictly on the endpoint
that actually activated at startup; default-device targets continue to follow the current default.
Contract violations (null buffer, sample overflow, decode failure) and non-recoverable HRESULTs
stay fatal; startup activation failures remain loud `Init` errors. `finish_packets` inherits the
same path, so shutdown never fails on a dead endpoint.

New diagnostics land in the log: `wasapi_device_lost` (source, hresult, rate-limited at 30 s)
and `wasapi_device_recovered` (outage_ms). Neutral tests cover the `DeviceReactivation` state
machine, HRESULT classification, `DrainFailure` mapping, and diagnostic display; a live device
test (CI-skipped on runners) simulates invalidation and proves the endpoint swap mid-capture.
Review regressions additionally cover non-sliding deadlines, post-attempt retry scheduling,
process identity, strict recovery selection, and startup fallback target resolution. Workspace
tests, live-device capture tests, and fresh-cache warning-denied Clippy are green.
Plan: `docs/superpowers/plans/2026-07-23-wasapi-device-loss-recovery.md`.

## Checkpoint (2026-07-23): Nightly 0.1.40

Nightly 0.1.40 contains PR #102's complete full-session GOP-timing fix. Finite GOP samples are
quantized cumulatively, and each fragment now allocates from the MP4 writer's actual monotonic
frontier toward its requested absolute endpoint. Repeated same-sign rounding ties therefore get
absorbed by later representable samples instead of accumulating into another two-tick backward
decode-time request.

Crowded, duplicate, and locally jittering finite timestamps retain every encoded dependency and
degrade to positive MP4 durations without terminating capture or the replay ring. Seal validation
also completes before pending video is taken or audio is drained, so an invalid seal cannot silently
drop a GOP. Deterministic regressions cover repeated cross-GOP ties, multiple 100 ns gaps, crowded
timestamps, local regressions, and failed-seal A/V preservation. The independent final review
approved the remediation with no blocking findings.

## Checkpoint (2026-07-22): boundary-constrained GOP quantization

Nightly 0.1.39 narrowed but did not eliminate the full-session decode-time failure. Two positive
100 ns-style intervals in one GOP each remained shorter than one 90 kHz tick, so independently
flooring both intervals advanced the locally accumulated GOP frontier by two ticks while the next
GOP retained its absolute start. The existing writer tolerance correctly rejected that `3602` to
`3600` backward movement.

Finite GOP seals now quantize cumulative sample boundaries within the configured video timescale.
Every MP4 sample keeps a nonzero duration, ticks are reserved for every remaining sample, and a
normally spaced final sample lands on the sealing keyframe boundary by construction. Crowded or
slightly backward finite timestamps retain every encoded dependency and minimally extend the span
instead of terminating capture or the replay ring.

PR #102 review found that independent per-GOP rounding could still accumulate across many
boundaries: accepting a one-tick overlap left the writer frontier stale, and a later boundary could
eventually be two ticks behind. Fragment samples are now quantized against their requested absolute
endpoint while allocating from the writer's actual frontier, so each representable GOP absorbs
earlier rounding drift. The capture timeline never asks the strict MP4 writer to move backward.
Seal validation also runs before pending video is taken or audio is drained, preventing a failed
seal from silently losing a GOP. Regressions cover repeated cross-GOP ties, two adjacent 100 ns
gaps, crowded timestamps, independent sub-tick jitter, and preservation of pending A/V state.

## Checkpoint (2026-07-22): Nightly 0.1.39

Nightly 0.1.39 contains PR #101's full-session finalization fix. Encoded video intervals shorter
than 100 us now retain their representable timing down to one configured MP4 timescale tick, so a
valid tightly spaced or variable-refresh-rate frame no longer creates an artificial two-tick
overlap at the next GOP boundary. The MP4 writer remains strict, the capture-side tolerance still
accepts only a one-tick rounding tie, and larger timestamp regressions continue to fail safely.

## Checkpoint (2026-07-22): sub-millisecond full-session GOP boundary

A Nightly 0.1.38 full-session recording failed at stop with video track 0 attempting to move from
decode tick 4,051,257 back to 4,051,255. The earlier one-tick boundary fix correctly covers
independent quantization ties, but the pipeline also floored every adjacent video interval at
100 us. A valid interval between one 90 kHz tick and that floor was lengthened within its GOP; the
next GOP retained its absolute start stamp and could therefore appear several ticks earlier.

Sealed video samples now use one configured video-timescale tick as their minimum positive
duration, matching the MP4 format's actual representable floor. The MP4 writer remains strict, and
the capture-side tolerance still accepts only a one-tick rounding tie, so real regressions of two
ticks or more are not hidden. A deterministic full-session fixture reproduces the reported
two-tick failure with adjacent frames seven ticks apart, verifies the stored interval remains seven
ticks, finalizes the file, and retains the existing larger-regression guard coverage.

## Checkpoint (2026-07-21): Nightly 0.1.38

Nightly 0.1.38 contains PR #100's recorder and review quality-of-life release. It adds an optional
games-only recorder pause with a durable Waiting state, explicit restart-as-administrator handling
for elevated games, immediate opening of newly exported clips, and fullscreen review playback. The
follow-up review remediation makes recorder transitions generation-safe, replays startup Waiting
state after frontend readiness, and preserves accurate private-working-set RAM sampling across
normal/elevated launches and older supported Windows builds.

## Checkpoint (2026-07-21): PR 100 review remediation

All five unresolved PR 100 findings are addressed. Recorder status events are now accepted only
from the currently installed service generation, so late stopped/recording events cannot overwrite
the intentional games-only `Waiting` state after either game detection or a settings restart.
Committing a waiting settings transition always advances the generation, including the no-sender
race where a detector restart is already spawning. The frontend readiness handshake also replays
the durable waiting status after its listeners exist, eliminating the startup-only lost event.

The RAM sampler keeps the low-privilege `PROCESS_MEMORY_COUNTERS_EX2` fast path but falls back to
the prior `VirtualQueryEx` / `QueryWorkingSetEx` resident-private-page walk when EX2 is unavailable
on older supported Windows builds. Child processes request `PROCESS_VM_READ` only for that fallback.
New runtime race, readiness replay, UI contract, and memory fallback regressions pass; the full
workspace test suite and a fresh-cache warning-denied workspace Clippy pass are green.

An independent follow-up review found one remaining non-blocking race in manual recorder start:
the Waiting notification was emitted after releasing the runtime lock without re-checking state.
`start_recording` now queries the durable Waiting state immediately before emitting, so a game that
starts a service in that gap prevents the stale Waiting update. A structural regression protects
the guard; workspace tests and fresh-cache warning-denied Clippy remain green.

## Checkpoint (2026-07-21): elevation decision and privilege-invariant RAM meter

The elevated-game warning now requires an explicit button choice. Backdrop clicks and Escape no
longer dismiss it; `Restart as Administrator` and `Not Now` remain available, while the dialog can
still disappear when the elevated game itself is no longer active.

The apparent administrator-mode RAM jump was a measurement-permission bug rather than evidence of
a duplicate Clipline process. The old sampler requested `PROCESS_VM_READ` and silently omitted
sandboxed WebView2 children during a normal launch, then counted them once elevation granted the
read. It now uses `K32GetProcessMemoryInfo`'s `PROCESS_MEMORY_COUNTERS_EX2.PrivateWorkingSetSize`
through `PROCESS_QUERY_LIMITED_INFORMATION`. A live normal-integrity probe succeeded against the
WebView2 renderer, so the same process-tree private working set is visible before and after an
administrator restart. Focused memory and elevation-dialog regressions pass. The workspace suite
passes apart from the VM-only live WGC frame test timing out twice, and a fresh-cache workspace
Clippy pass is clean. That WGC timeout is unchanged capture-device behavior and does not exercise
the modal or memory sampler.

## Checkpoint (2026-07-20): recorder and review quality-of-life bundle

Four requested workflow features are implemented. Settings > Games now has an opt-in `Pause
recorder when no game is open` toggle, defaulting off for legacy and new settings. With automatic
game detection enabled, the recorder remains armed in a distinct `Waiting` state without owning a
capture/encode service; Save Replay is disabled until an enabled game appears, game entry starts a
fresh buffer, and game exit stops the active run instead of falling back to desktop capture. The
service generation guard also owns waiting notifications, so a concurrent manual stop cannot be
overwritten by a stale policy transition.

The elevated-game warning again offers an explicit `Restart as Administrator` action. Ordinary
launches remain `asInvoker`, UAC cancellation keeps the normal process and retry UI alive, and a
successful launch uses only the current executable plus an exact parent PID/creation-time handoff
before the elevated child enters Tauri. This is a per-launch choice and the dialog warns that the
rolling replay buffer resets.

Successful trim/play exports now show an `Open clip` action next to the transient export status;
the action opens the exact result already inserted into the library cache. The review transport
also has fullscreen enter/exit controls backed by the WebView fullscreen API, with `F` toggling and
Escape reserved for leaving fullscreen before the existing close-review shortcut.

Focused settings, runtime-state, Windows handoff, player-core, and 82 UI-contract tests pass. The
full workspace test suite passes, including the native device-aware suites, and fresh-cache app
Clippy plus workspace Clippy pass with warnings denied. Native interaction acceptance remains for
the four user-facing flows.

## Checkpoint (2026-07-21): Nightly 0.1.37

Nightly 0.1.37 is the first updater build containing PR #89's combined audit remediation and the
follow-up capture, replay, cloud, and review fixes. The release is built from the synchronized
`main` / `develop` promotion point after workspace tests, warning-denied Clippy, Windows and Ubuntu
CI, RustSec, and manual replay/audio verification passed.

## Checkpoint (2026-07-21): second PR 89 review pass

The presigned object-upload client now refuses every redirect, matching the authenticated/control
clients. A 307 regression proves a reusable PUT body is not forwarded to the redirect target; the
direct-upload path falls back normally on the returned non-success response.

WASAPI's discontinuity fade no longer spends its 40 ms ramp on digital-silence pairs before the
first live sample in a mixed packet. Fully silent buffers and cross-buffer fades retain their
existing behavior. The native media-folder picker keeps the canonical path for authorization but
returns a user-facing path without Windows `\\?\` / `\\?\UNC\` prefixes. Local Library refreshes
canonicalize their unchanging media root once, while every individual asset remains independently
canonicalized and checked beneath that root before exact WebView scoping.

The review's proposed audio-sidecar rate nudge remains intentionally rejected: commit `e7ca91e`
implemented it and `a85ceae` removed it after audible rate oscillation. Mid-session settings saves
also continue refusing to overwrite an externally corrupted file; startup quarantine/recovery is
the deliberate data-preserving boundary. The three retry backoffs remain separate because their
jitter, caps, and status semantics differ.

Focused regressions, the full workspace test suite, and fresh-cache warning-denied workspace Clippy
pass.

## Checkpoint (2026-07-21): PR 89 review regressions

Seven actionable PR review findings are fixed. Settings saves continue when the optional low-level
save hook failed to install, while hotkey syntax is still validated. WASAPI keeps a requested QPC
anchor across packets with missing or invalid timestamps and consumes it only when a finite
timestamp arrives.

The storage ownership boundary now includes a narrow pre-marker migration signal: only MP4s using
Clipline's generated `clip_<timestamp>[_attempt]` or `session_<timestamp>[_attempt]` names are
adopted without a sidecar. This restores quota accounting/GC for legacy replays and recovery for
legacy `session_*.mp4.recording` files while arbitrary unmarked MP4s remain untouched. Recovered
legacy recordings receive an ownership marker before finalization.

Clipboard sharing replaces only `CF_HDROP` and no longer empties the entire clipboard before the
new handle is accepted. A failed Cloud tab refresh remains non-authoritative so cached completed
uploads stay visible. Cloud duplicate detection now hashes the requested payload first and skips
only an exact local clip ID; changing audio selection or replacing media at the same path starts a
new upload, while exact re-uploads still return the completed record.

Focused regressions and the full workspace test suite pass. Fresh-cache warning-denied workspace
Clippy is clean.

## Checkpoint (2026-07-20): one-tick full-session GOP boundary overlap

A full-session writer failed after roughly 86 seconds with video track 0 attempting to move from
decode tick 7,731,609 back to 7,731,608. Segment sample durations are quantized relative to each
GOP while the next GOP start is quantized from the absolute recording origin. At an exact rounding
boundary those equivalent timestamps can differ by one 90 kHz tick (about 11 microseconds).

The MP4 writer remains strict about backward decode time and now exposes its current per-track
frontier. The capture pipeline alone treats a one-tick overlap as a rounding tie and keeps the
already-written frontier; regressions of two ticks or more still fail. Regression coverage writes
the observed adjacent-segment shape through the full-session path and separately proves that the
larger-regression guard remains intact.

## Checkpoint (2026-07-20): legacy cloud upload path identity

Completed cloud records created by older builds can contain canonical Windows paths such as
`\\?\D:\Videos\...`, while the local library reports the same clip as `D:\Videos\...`. Exact
frontend path comparison hid the uploaded visibility badge, made cloud entries appear local-only,
and exposed the upload action again. Local/cloud pairing now uses a shared Windows-aware path
comparison that strips the verbatim prefix, normalizes separators, and compares Windows paths
case-insensitively without changing POSIX semantics.

The backend applies the same identity rule when finding, replacing, and removing upload records.
An upload request for a completed record now returns that existing record before any media transfer,
with a second local-clip-ID check after hashing as defense in depth. Regression coverage includes
legacy verbatim paths, cloud-library availability, frontend wiring, and duplicate-upload prevention.

The Cloud library tab starts a forced server request as soon as it is selected. While that request
is active and after it succeeds, the server response is authoritative: finalized local upload
history absent from the response is no longer rendered as generic, broken cloud cards. Active and
still-processing upload records remain visible until the server begins returning them.

## Checkpoint (2026-07-20): semi-static capture inflated video PTS

Direct frontier measurement overturned the audio-clock diagnosis below. Two replay saves taken
737.28 wall-clock seconds apart advanced the video frontier by 741.02 seconds (`1.00507x`) but the
audio frontier by 737.44 seconds (`1.00021x`). Independent five-minute probes measured both raw
MOTU endpoints at only +32 ppm versus QPC and the production `WasapiLoopback` sources within one
Opus packet of wall time. League/game sessions with sustained real frames were audio-perfect;
idle-desktop and test captures accumulated roughly 0.3--0.5% apparent audio lead. The audio path,
MOTU clock, MP4 muxer, replay ring, and players are not the source of this drift.

The defect was `CadencedCapture` in `apps/clipline-app/src/service.rs`. Timeout duplicates advance
on a synthetic `1/fps` grid. When a backend returned before its requested timeout, the handler
still emitted a full video cadence step and reset its wall anchor to `now`. Stale real-frame retries
made that path repeat on semi-static content, so video PTS advanced faster than wall/QPC time. A
moving game regularly supplied accepted QPC-stamped frames and hid the ratchet by re-anchoring it.

Premature timeouts now remain timeouts until the existing cadence deadline. They neither emit a
duplicate nor reset the wall anchor. Once a real wall interval has elapsed, duplicate PTS still
advance on the configured grid, catch up across missed intervals, reuse the latest captured
texture, and remain monotonic. A regression reproduces the failure: 120 one-millisecond early
returns previously advanced PTS by 2.000 seconds in only 0.181 seconds; video PTS is now bounded by
elapsed wall time.

Plan commit `fc767ef`; implementation `aeeb7b0`. The 422-test app suite and the full workspace pass,
including serial real-device WGC, DXGI, WASAPI, MFT, shared-clock, and FFmpeg tests. Warning-denied
workspace Clippy and clean-cache app Clippy are clean. Manual acceptance is a ten-minute
idle-desktop run followed by multiple 30-second replays, then a moving game test. Both audio tracks
should reach the video tail within normal one-frame/Opus headroom with no crackle, startup
transient, or keyframe regression.

> The next two checkpoints record superseded audio-clock hypotheses and failed experiments. Keep
> them only as history; the direct video/audio frontier measurements above are authoritative.

## Checkpoint (2026-07-20): QPC servo rejected and rolled back

Manual testing rejected the continuous QPC audio clock servo. After the recorder had been running
for roughly 53 minutes, `clip_1784585886.mp4` contained 30.000 seconds of video but only 25.540
seconds of Output Audio and 25.520 seconds of Microphone audio. The same run logged repeated WASAPI
data discontinuities. The one-packet servo therefore amplified real device interruptions into
multi-second audio loss and made synchronization dramatically worse.

The servo implementation has been removed and WASAPI packet placement restored exactly to the
previous nominal-cadence path: QPC anchors the first packet and post-idle/discontinuity recovery,
while continuously delivered PCM retains every sample. Do not revive the resampling approach
without first adding raw packet-QPC/sample-count telemetry and testing a controlled synchronized
A/V fixture over a long-running buffer. The restored 204-test capture suite, including hardware
device tests, passes.

## Checkpoint (2026-07-20): continuous QPC audio clock servo

The nominal-cadence follow-up did not fix A/V sync. In `clip_1784581736.mp4`, video lasts exactly
30.000 seconds while Output Audio ends at 29.618 seconds and Microphone at 29.598 seconds. The
captured source was a synchronized YouTube osu! video, which made independent measurement possible:
cross-correlation of audio spectral onsets against gameplay-region frame changes placed audio
roughly 350--400 ms before video. Whole-section and two separate active-play windows all peaked
near -367 ms, matching the missing tail and the user's VLC/Clipline observation.

WGC `SystemRelativeTime` and WASAPI packet QPC are timestamps on the same synchronization clock.
Keeping only the first audio anchor and then advancing at nominal 48 kHz let device-clock error
accumulate across the full recorder uptime; a later 30-second replay selected earlier audio content
and omitted its matching tail. Audio now holds one real packet for QPC lookahead and resamples it to
a **cumulative** shared-clock sample frontier. Fractional device intervals therefore do not round
into long-running drift. Half-open interpolation uses the following packet's first stereo pair at
the boundary, avoiding forced packet endpoints and the periodic holes from discontinuous gap fill.

The pending packet remains a hard silence-synthesis frontier. Actual delivery idle flushes after
100 ms, terminal drain flushes immediately, timestamp-error input falls back to contiguous PCM,
and explicit device discontinuities flush/reset before the existing 40 ms onset fade. Regressions
cover cumulative 514.4-pair clock intervals, cross-packet waveform continuity, and finite idle
flush. The 205-test capture suite (including WGC, DXGI, WASAPI, MFT, shared-clock, and FFmpeg device
tests), workspace tests, warning-denied workspace Clippy, and clean-cache capture Clippy pass.

Plan commit `ca62bda`; implementation `c332e2d`. Restart Clipline, let the buffer run for at least
two minutes, then save another synchronized-source replay and compare its beginning/end in both VLC
and Clipline. Also listen for any return of periodic crackle.

## Checkpoint (2026-07-20): nominal WASAPI cadence and encoded MFT keyframes

The next successful 30-second replays proved that the crackle fixes had introduced progressive A/V
lead: `clip_1784530928.mp4` has 30.100 seconds of video but only 29.700 seconds of Output Audio and
29.680 seconds of Microphone audio, and the user confirmed sound arrived before picture. The
one-packet-lookahead path was converting each 512-pair MOTU packet to its roughly 510-pair QPC
interval. That removed real PCM continuously and compressed about 0.4 seconds out of each 30-second
track.

Continuous WASAPI packets now retain every device sample and append at the nominal 48 kHz cadence.
QPC is used only for the first anchor, after 100 ms of actual device-delivery idleness, or for an
explicit `DATA_DISCONTINUITY`. Quiet loopback still receives finite synthetic silence, idle resume
still gets the bounded late-recovery fade, terminal drain remains immediate, and no timestamped
packet is held back. Neutral regressions reproduce the observed 512-pair packets on 510-pair QPC
steps and require all 153,600 pairs to span 3.2 seconds exactly.

The same long-running recorder later hit the ten-second pending-GOP safety bound. The AMD H.264 MFT
path classified keyframes only from `MFSampleExtension_CleanPoint`, although the encoded H.264 IDR
NAL is the authoritative signal and some hardware MFT output omits the optional flag. MFT packets
now accept either CleanPoint or an encoded IDR, matching the FFmpeg path. The ten-second/byte limits
remain unchanged, so a genuinely stalled encoder is still bounded instead of consuming memory.

Plan commit `bb30ed1`; audio implementation `1fe0ce9`; keyframe implementation `93c3d5f`. The
204-test capture suite, including real WGC, DXGI, WASAPI, MFT, shared-clock, and FFmpeg device tests,
passes, as do workspace tests, warning-denied workspace Clippy, and clean-cache capture Clippy.
Retest a fresh 30-second replay with a simultaneous visual/sound cue near both ends, then leave the
buffer running and save multiple replays to exercise repeated keyframe boundaries.

## Checkpoint (2026-07-20): repeat replay save and review-audio EOF

The first crackle-free replay, `clip_1784529665.mp4`, exposed two follow-on boundary bugs. Its video
is exactly 30.000 seconds, while Output Audio ends at 29.655 seconds and Microphone at 29.635 seconds.
The audio-only review sidecars preserve those endpoints. During the remaining video tail, the review
timer saw each ended audio element paused and called `play()` again; WebView restarted it from zero,
so roughly the first 350 ms of audio played at the end. VLC correctly remained silent.

The sidecar synchronization policy now receives each element's duration and ended state. An ended
sidecar stays exhausted while video is beyond its duration, but a seek back inside the sidecar range
seeks and resumes it normally. A pure regression covers both decisions and the UI contract requires
the live transport state to be wired into the policy.

The next Save Replay failed with `media sample timestamp precedes recording origin`. Continuously
delivered WASAPI audio can trail video and be sealed into a later GOP, but replay materialization
filtered pre-origin audio only from the first selected segment. Origin filtering now visits every
selected segment and every audio track before fragment timestamps are built. A two-segment fixture
places stale audio in the later segment and verifies exact sample/data removal plus timestamp advance.

Plan commit `cf0083d`; player implementation `b0f306a`; replay implementation `5147791`. All 89
player-core tests, 78 UI contracts, 206 capture tests, workspace tests, warning-denied workspace
Clippy, and clean-cache app/capture Clippy pass.

Retest the existing first replay through its final second in Clipline, seek back from EOF, then save
at least two new replays from one continuously running buffer. Both new saves must finalize.

## Checkpoint (2026-07-20): continuous WASAPI delivery no longer becomes synthetic silence

VLC reproduced the crackle in `clip_1784527236.mp4`, proving the artifact was encoded rather than a
review-player problem. Typed telemetry on the configured MOTU M Series `Out 1-2` and `In 1-2`
tracks showed recurring complete-packet late recovery: each event corrected 10--11 ms and both
sources accumulated roughly 150 ms per 30 seconds. The five-millisecond recovery fade consequently
became a periodic encoded level hole during otherwise continuous audio.

WASAPI capture now holds one timestamped chunk, interpolates it to the following QPC interval, and
treats pending real PCM as a hard frontier for poll-time silence. Crucially, the fallback flush is
based on 100 ms with no device packet arriving, not on packet timestamp age: this MOTU driver reports
a source timeline that drifts behind video even while samples arrive continuously. A genuinely quiet
loopback still flushes finitely, stream finish still flushes immediately, and the discontinuity/late
fade remains available only for actual startup, idle resume, and device discontinuities.

Neutral fixtures cover the observed 512-sample/510-sample interval, endpoint preservation, genuine
timestamp gaps, the pending-real-audio synthesis frontier, finite idle flush, and 300 consecutive
chunks without a packet reanchor. The real output-plus-microphone build ran for 45 seconds with zero
`wasapi_late_audio_reanchored` events; only the two expected startup discontinuities appeared. The
205-test capture suite, workspace tests, warning-denied workspace Clippy, and clean-cache capture
Clippy pass. Plan/telemetry commits begin at `e06752d`; core implementation commits are `de9b804`,
`ec70d82`, and `a2fb2e3`.

Retest with a fresh recording of at least one minute while game output and microphone remain active,
then save a 30-second replay and listen in VLC. The old MP4 remains unchanged and will still crackle.

## Checkpoint (2026-07-20): review sidecar rate artifacts

The fresh replay `clip_1784527236.mp4` still sounded crackly throughout in Clipline. Its two Opus
packet timelines are continuous, decoded samples have no impulses at two-second GOP boundaries,
and the two generated review sidecars are packet-for-packet stream copies of the source tracks.
Their encoded-packet SHA-256 hashes match exactly, ruling out replay materialization, muxing, capture,
and sidecar extraction as the source of this symptom.

The review transport checked each hidden audio element every 500 ms and changed its playback rate
to 0.95x or 1.05x whenever ordinary drift exceeded 25 ms. Returning inside the deadband restored
1.00x, so WebView continuously time-stretched two independent Opus decoders. Ordinary playing drift
now keeps the video's requested playback rate. Forced seeks, paused alignment, invalid-sidecar
recovery, and gross drift over 500 ms still seek; selected-track routing, mute/volume, preparation,
and lifecycle behavior are unchanged. Focused sidecar tests, workspace tests, warning-denied
workspace Clippy, and clean-cache app Clippy pass. Plan commit `814e4ee`; implementation commit
`a85ceae`.

Retest the same `clip_1784527236.mp4` in Clipline with both tracks, Output only, and Microphone only;
a new recording is not required. Also seek while playing/paused and change playback speed. If the
same file still crackles, compare it in an external player before changing capture again.

## Checkpoint (2026-07-20): smooth WASAPI late recovery

The 30-second replay `clip_1784525638.mp4` began cleanly after the discontinuity fade, but crackled
throughout. Its Opus packet timelines are continuous; decoded PCM instead contains isolated deep
10 ms holes (about 40 dB on Output Audio near 28.27 seconds and 21 dB on Microphone near 23.64
seconds). Recorder diagnostics repeatedly reported `wasapi_late_audio_reanchored` every two to
three seconds. When a quiescent endpoint resumed behind already-committed synthetic silence, the
late-buffer recovery correctly retained the complete live chunk but joined an arbitrary waveform
sample directly to digital silence, creating a hard audible edge.

Live experiments with both 30 ms and 60 ms normal-poll allowances produced the same recovery cadence,
proving a fixed timeout cannot outwait endpoints that stop delivering while quiet. Normal capture
therefore keeps 30 ms of active-delivery headroom, and every actual synthetic-silence-to-live
recovery now receives a five-millisecond linear fade. The fade retains every live sample, reaches
full amplitude inside the first Opus frame, and leaves following samples untouched. Stream finish
separately waits three Opus frames, drains only real buffered audio within the video boundary, and
does not synthesize tail silence. Regressions cover the fade shape and sample retention, poll
horizon, and terminal-only audio. The real shared-clock hardware test passed with 20.0 ms maximum
segment skew and total drift, inside the 45 ms contract. The 200-test capture suite, workspace
tests, warning-denied workspace Clippy, and clean-cache capture Clippy pass. Initial plan/terminal
drain commits `1b13651`/`58109ac`; final plan/implementation commits `565954e`/`b029b80`.

Retest a fresh replay of at least 30 seconds with Output Audio and Microphone active throughout.
Listen from start to finish with both selected, then each track alone. The old file is unchanged and
will retain its encoded holes and hard boundaries; only recordings made by this build receive the
smoothed late recovery.

## Checkpoint (2026-07-20): WASAPI discontinuity onset fade

The next 188-second full session `session_1784524668.mp4` contained a loud sound at its beginning.
Both Opus streams are structurally continuous, but Output Audio begins at 11.687 ms with a non-zero
broadband transient: the first 20 ms peaks around -24.5 dBFS and decays by roughly 30 dB over the
following 60 ms. Recorder diagnostics show `wasapi_data_discontinuity` on both sources at the exact
05:17:48 recording start, confirming that the abrupt source boundary was encoded into the file.

WASAPI capture now applies a 40 ms linear stereo fade after conversion, resampling, and configured
gain. The fade is armed at capture startup and re-armed before each packet marked
`DATA_DISCONTINUITY`; explicit digital-silence buffers do not consume it. Timestamps, sample counts,
gap filling, late-buffer recovery, Opus framing, and diagnostics are unchanged. The neutral
regression covers a two-buffer ramp, digital-silence deferral, steady-state pass-through, and
re-arming. The capture suite, real shared-clock device test, workspace tests, warning-denied
workspace Clippy, and clean-cache capture Clippy pass. Plan commit `475a5eb`; implementation commit
`7920ad0`.

Retest a new full session with Output Audio and Microphone enabled, stop after at least ten seconds,
and replay 0:00 several times. The old file remains unchanged and will still contain its encoded
transient; only recordings made by this build receive the discontinuity fade.

## Checkpoint (2026-07-20): smooth multi-track review audio synchronization

The follow-up 74-second full session `session_1784523792.mp4` was mostly audible after the delayed
WASAPI recovery, but multi-track playback stuttered at exactly two seconds. Both Opus tracks have
continuous 20 ms packets and continuous decoded PCM through that boundary, so the saved media is
intact. The default review selection enables Output Audio and Microphone, which makes the player
extract and run two independent audio sidecars alongside the video element.

The review player compared each sidecar clock with video every 500 ms and hard-seeked the audio
element whenever ordinary drift exceeded 100 ms. That turned natural WebView media-clock drift into
an audible skip or repeated fragment. Playing sidecars now use bounded +/-5% rate correction outside
a 25 ms deadband and return to the requested video rate when aligned. Hard seeks remain for explicit
seeks, paused alignment, invalid sidecar clocks, and gross drift over 500 ms. The pure player
regression failed under the old behavior and covers correction in both directions plus every
hard-seek boundary. Focused tests, workspace tests, warning-denied workspace Clippy, and clean-cache
app Clippy pass. Plan commit `3abaf7c`; implementation commit `e7ca91e`.

Retest the reported file from the beginning with both tracks selected, then let it play for at least
one minute. Confirm the two-second stutter and periodic skips are gone. Seek while playing and
paused, and toggle Output only, Microphone only, both, and mute; each selection should remain synced.

## Checkpoint (2026-07-20): delayed WASAPI audio recovery

A real 989-second League full-session recording exposed both enabled audio tracks stuttering into
permanent silence. FFprobe confirmed a valid 59,332-frame H.264 video and two complete 49,458-packet
Opus tracks, ruling out truncation or missing mux samples. Output contained real audio only around
7.40--13.74 seconds and microphone only during the opening seconds; the rest decoded as exact
digital silence. Clipline logged no device-loss error, and the original 995 MB session and its
sidecars were inspected read-only and remain untouched.

The finite WASAPI poller advances a quiet source with synthesized silence to keep it aligned with
video. With only one Opus frame of delivery allowance, a delayed real buffer could arrive entirely
behind that synthetic frontier. The assembler discarded it, the next video poll synthesized more
silence, and a consistently delayed endpoint could never catch up; partial overlap caused the
audible stutter before lockout.

The assembler now distinguishes synthetic advancement from genuine duplicate/late buffers. When
silence has overtaken live audio, it preserves the complete real chunk at the current monotonic
position and retains that one timestamp correction for following chunks. Late chunks without a
synthetic advance keep the prior trimming behavior. A typed, per-source, 30-second-rate-limited
`wasapi_late_audio_reanchored` diagnostic records the correction in milliseconds. Deterministic
partial- and full-overlap fixtures failed under the old behavior and now preserve every live sample.
The real shared-clock hardware test passed with 16.6 ms maximum segment skew and 43.3 ms total
drift, inside the existing 45 ms contract. Capture tests, workspace tests, warning-denied workspace
Clippy, and clean-cache capture Clippy pass. Plan commit `71e9977`; implementation commit `65f45ff`.

Retest a five-minute full session with output and microphone activity near the beginning, middle,
and end, plus one replay save. Confirm neither track stutters into silence, both remain synced, and
any `wasapi_late_audio_reanchored` log line is rate-limited and followed by audible recording.

## Checkpoint (2026-07-20): replay audio-origin save

Manual replay acceptance after the full-session startup fix exposed the same
`media sample timestamp precedes recording origin` invariant at a different boundary. Replay save
rebases the MP4 timeline to the first selected video GOP, but an indivisible 20 ms Opus packet can
begin before that GOP's keyframe and end after it. That packet is correctly retained across GOPs
for full-session continuity, yet it has a negative timestamp when its later GOP becomes the first
segment of a replay.

Replay materialization now removes complete audio samples from only the first selected segment
while their start precedes the selected video origin, then advances that track's start by the exact
removed durations. Ring contents, full-session muxing, later replay segments, delayed/gapped audio,
and the MP4 writer's negative-timestamp validation remain unchanged. A deterministic fixture puts
the replay keyframe at 1.51 s inside the 1.50--1.52 s Opus packet: it reproduced the production
error before the fix and now drops exactly that packet, starts audio at 1.52 s, and finalizes the
replay. Capture tests, workspace tests, warning-denied workspace Clippy, and clean-cache capture
Clippy pass. Plan commit `47cd9cc`; implementation commit `c91d805`.

Retest Save Replay with system or microphone audio after capture has run longer than one GOP.
Confirm the warning does not recur, the clip appears in Library, and playback begins cleanly with
synchronized audio.

## Checkpoint (2026-07-19): full-session audio-origin finalization

A real full-session stop exposed `media sample timestamp precedes recording origin`; the non-empty
`.mp4.recording` was preserved as designed. The recorder defines its timeline at the first encoded
video packet and already drops engine-init audio lead-in, but the predicate retained an indivisible
20 ms Opus packet when it began before that video origin and ended after it. The asynchronous
full-session writer then correctly rejected the packet's negative relative timestamp and reported
the failure at finalization.

Startup-audio filtering is now shared by both first-keyframe and GOP-seal paths and retains only
packets whose start is at or after the video origin, with the existing sub-nanosecond tolerance.
The packet that straddles the origin is dropped whole; later delayed and gapped audio timing is
unchanged. A deterministic 510 ms video-offset fixture reproduced the exact finalization error
before the fix and now produces a finalized MP4. Existing lead-in and delayed/gapped mux tests,
all workspace tests, warning-denied workspace Clippy, and clean-cache capture Clippy pass. The
reported preserved recording was not opened, renamed, or deleted. Plan commit `f563812`;
implementation commit `daff93a`.

## Checkpoint (2026-07-19): single-PUT uploads declare MP4 content type

The consolidated manual Cloud acceptance run found that a real single-PUT upload failed with HTTP
400: the server requires `Content-Type: video/mp4`. Clipline's chunked proxy path already declared
that media type, but its streamed single-PUT request sent only `Content-Length`. The existing mock
verified the body without constraining the header, so the divergence was not covered.

The single-PUT request now sends the same explicit MP4 content type, and the focused mock requires
it. Plan commit `92f05b6`; implementation commit `0d3475a`. The focused test failed before the
implementation and passes afterward. CI-mode workspace tests and warning-denied workspace Clippy
pass. The local real-device WGC shared-clock test separately failed twice because the hardware
encoder did not emit a keyframe before the existing ten-second pending-GOP bound; that capture
failure is unrelated to the HTTP-only change. Retest by uploading a small clip through a deployment
that selects `single_put` and confirm progress reaches processing/ready without the HTTP 400.

## Checkpoint (2026-07-19): immediate playback for newly exported clips

The first consolidated manual-acceptance run found one clear failure: a 30-second trim exported
from a 2.0 GB, 33:43 session completed with flat process memory (about 152--155 MB), but its newly
inserted Library card consistently showed WebView media error 4. The original session remained
intact and playable. This was an authorization race, not evidence of a failed large-file mux:
`list_clips` exact-scoped every discovered MP4 for Tauri's asset protocol, while `export_clip`
returned a new path and the renderer inserted it directly into the Library cache before another
scan could grant that path.

`export_clip` now receives the application handle, retains the validated configured media root,
and exact-scopes the completed MP4 before returning it to the renderer. A focused UI contract first
reproduced the missing command invariant and now requires that grant. The Library unit-test group
passes. Plan commit `d8226f6`; implementation commit `23f7aef`. All workspace tests,
warning-denied workspace Clippy, and clean-cache warning-denied app Clippy pass. Retest by exporting
a trim and opening its card immediately without refreshing or restarting; confirm metadata,
playback, seeking, and a second reopen all work and the source remains playable.

## Checkpoint (2026-07-18): explicit application module boundaries

The combined audit's L-14 is fixed with incremental, compatibility-safe module boundaries. The
largest application shells now delegate diagnostic-log ownership, media-root probing, clip naming,
and cloud cache identity to focused Rust modules with narrow parent-only APIs. Tauri command names
and externally visible behavior remain unchanged, while repository contracts prevent those domains
from being folded back into the command/service monoliths.

The renderer now enters through `bootstrap.mjs`, which explicitly imports frozen presentation,
player, and Cloud core surfaces before loading the remaining controller adapter. The classic
`PlayerCore` and `CloudCore` globals remain only as the Boa/gradual-migration compatibility layer.
Filename stems, marker-kind labels, month names, clip titles, and gallery day labels now share one
DOM-free presentation core. Its unified suffix policy strips MP4, MOV, MKV, and WebM consistently,
closing the observed local/cloud title disagreement.

Plan commit `e859f5d`; implementation commit `6c86a72`. Boa tests cover the shared suffix, marker,
and calendar policies; UI contracts require the module bootstrap and explicit imports; repository
contracts enforce all four Rust owners and reject the duplicated helpers. All 421 app tests, 88
player-core tests, seven repository contracts, 77 UI contracts, CI-mode workspace tests,
fresh-cache app Clippy, and warning-denied workspace Clippy pass. Computer Use verified the module
build in the nine-of-nine Library, General and disconnected Cloud Settings, and active review
playback. No new manual-only item remains.

## Checkpoint (2026-07-18): consolidated divergence-prone paths

The combined audit's L-15 is fixed. Memory and disk replay rings now share keyframe-window and
eviction planning, while `ReplayStorage` owns the remaining backend dispatch for metrics, window
loading, and insertion. Folder commands share one off-main-thread native dialog constructor while
retaining their distinct media-authorization rules. Game discovery no longer hides drift behind a
module-wide dead-code allowance.

Process-loopback activation reports a typed operation timeout, so recorder fallback no longer
classifies errors by display text. The MP4 walker and both trim readers share one overflow-checked
normal/large/terminal box-header decoder. All four fragment payload transports now share sample
validation, `moof`/`mdat` planning, chunk bookkeeping, decode-time advancement, and sequence commit;
only their payload I/O differs.

Plan commit `c6bbc94`; implementation commit `621c6dc`. Tests prove memory/disk eviction safety,
typed timeout classification, checked header boundaries, and byte-identical output across owned,
borrowed, single-source, and per-track-source MP4 writes. A repository contract rejects the blanket
allowance, duplicated dialog/header/state paths, timeout substring matching, and the FFmpeg codec
no-op. All 421 app tests, 18 buffer tests, 194 capture tests, 112 MP4 tests plus integrations,
CI-mode workspace tests, fresh-cache changed-crate Clippy, and warning-denied workspace Clippy pass.
Computer Use verified the rebuilt nine-of-nine Library with recording active. Existing media-root
and Windows capture lifecycle acceptance scenarios cover the native boundaries, so no duplicate
manual-only item was added.

## Checkpoint (2026-07-18): coalesced off-thread memory sampling

The combined audit's L-16 is fixed without changing the displayed metric. `MemorySampler` now owns
one async mutex and a one-second monotonic cache of either success or failure. The first stale caller
runs the exact private-resident process-tree walk on Tauri's blocking pool while concurrent callers
wait; they then reuse the completed result rather than duplicating the address-space scan.

`memory_status` is asynchronous and reads the managed sampler. The renderer keeps its two-second
visible cadence, skips invokes while the document is hidden, and refreshes immediately on
`visibilitychange` when shown again. Child-process enumeration, conhost exclusion, and private
working-set semantics are unchanged.

Plan commit `938b3ea`; implementation commit `fb30ca0`. Async fixtures prove eight concurrent calls
execute one measurement and that failures are cached then retried after expiry. The UI contract
requires the async managed sampler, blocking-pool boundary, hidden guard, and visibility refresh.
All 421 app unit tests, 77 UI contracts, CI-mode workspace tests, fresh app Clippy, and
warning-denied workspace Clippy pass. Computer Use verified a live RAM value, minimized the rebuilt
app for three seconds, restored it, and observed sampling resume with the nine-of-nine Library
healthy. No manual-only item remains.

## Checkpoint (2026-07-18): transition-only Cloud gallery rendering

The combined audit's L-32 is fixed. Cloud upload progress reconciliation is now DOM-free in
`CloudCore` and returns the normalized record plus a `renderRequired` decision. Byte-only multipart
ticks still update the deck percentage immediately, but they preserve the upload record timestamp
and do not rebuild either gallery or rearm poster observers.

The first record plus path, local/remote identity, URL, visibility, status, or error transitions
still render synchronously. That preserves Cloud membership, search/filter results, sort order,
visibility badges, processing/failure states, and terminal uploaded state. Explicit null values in
native events now authoritatively clear stale remote/error fields rather than being mistaken for an
omitted field.

Plan commit `1bd80ca`; implementation commit `255a8a6`. Boa tests cover byte-only reconciliation,
all meaningful transitions, and a 500-event burst that produces zero gallery renders and no
timestamp churn. A UI contract proves the constant-size percentage update precedes the single
conditional render, and JavaScript syntax checks pass. All 419 app unit tests, eight CloudCore tests,
77 UI contracts, CI-mode workspace tests, fresh app Clippy, and warning-denied workspace Clippy are
green. Computer Use verified the rebuilt nine-of-nine Local gallery and disconnected Cloud view.
The existing large real-account upload scenario now also checks gallery stability during progress.

## Checkpoint (2026-07-18): typed rate-limited capture diagnostics

The combined audit's L-31 is fixed. ToolHelp snapshot entries now call their fallback executable
value `image_name`, while `AudioProcessInfo.process_path` remains reserved for a queried full image
path. Internal image lookup names and fixtures preserve the existing case-insensitive basename/path
matching and process-tree grouping behavior without implying that ToolHelp supplied a path.

WASAPI discontinuities now emit a typed `CaptureDiagnostic` through a process-wide handler installed
by the desktop before capture can start. Clipline routes those events into its existing bounded log;
each capture source emits immediately, suppresses repeats for 30 seconds, then reports the number
suppressed. Gap fill and packet handling are unchanged. Activation-blob safety comments now name the
actual `CoTaskMemAlloc` plus `PROPVARIANT`/`PropVariantClear` ownership path. The audit's cited FFmpeg
print was already absent, and a production-source contract keeps it absent.

Plan commit `c40ac40`; implementation commit `e5c51c2`. Pure tests cover the limiter sequence,
suppressed counts, typed formatting, and handler delivery; repository contracts enforce snapshot
naming, comment accuracy, no production WASAPI/FFmpeg `eprintln!`, and early desktop handler
installation. All 193 capture tests plus integrations, all 419 app tests in the CI-mode workspace,
fresh capture/app Clippy, and warning-denied workspace Clippy pass. Computer Use verified the
rebuilt nine-of-nine Library, and the live log received structured discontinuity events. The
existing Windows capture lifecycle acceptance scenario remains sufficient; no duplicate manual
item was added.

## Checkpoint (2026-07-18): centralized Windows platform helpers

The combined audit's L-30 is fixed. Generic Credential Manager ownership, decoding, and
write/read/delete behavior now live behind one safe `CredentialStore`; Cloud and osu! keep only
their domain labels and transactional adapters. Successful Win32 calls that return a null
credential, malformed nonempty blobs, invalid UTF-8, and embedded-NUL target/user strings all fail
safely, while the single owned credential wrapper guarantees `CredFree` on every branch.

Shell opening, free-space queries, atomic file replacement, null-terminated UTF-16 conversion, and
Windows error conversion are likewise centralized under `src/windows/`. Settings, poster, and osu!
enrichment publication share the replacement helper; game-icon and shell paths share the UTF-16
boundary. Neutral wall-clock helpers now live in `util`, removing the app/service/osu!/media clock
copies without changing their signed or unsigned call-site types.

Plan commit `5f69751`; implementation commit `b26b88e`. Seven Windows helper tests cover credential
decoding/labels, UTF-16, shell result boundaries, and the existing elevation/instance wrappers; a
signed-time boundary and a recursive repository contract prevent the duplicated APIs and clocks
from returning. All 419 app tests, CI-mode workspace tests, fresh app Clippy, and warning-denied
workspace Clippy pass. Computer Use verified the rebuilt nine-of-nine Library and opened the local
osu! API setup guide in Chrome through the centralized shell helper. The existing real credential
transaction acceptance scenario remains sufficient, so no duplicate manual item was added.

## Checkpoint (2026-07-18): bounded runtime diagnostic logging

The combined audit's L-29 is fixed. The process-lifetime diagnostic handle is now a locked writer
that tracks its active byte count and rotates before the next line would cross 1 MiB. Rotation
flushes and closes the live Windows handle, replaces one bounded old generation, and reopens the
active file. An oversized pre-fix log is migrated by retaining only its newest bounded tail, and a
single UTF-8 message is truncated on a character boundary so it cannot defeat the cap.

Generic window diagnostics now discard high-frequency move and resize events while retaining
focus, destroy, DPI, drag/drop, theme, and explicit close behavior. The redundant per-line flush is
gone; `File` writes remain direct and rotation performs the required flush.

Plan commit `7607b11`; implementation commit `d95568f`. Five log fixtures cover repeated
multi-generation rotation, newest-line retention, UTF-8 truncation, and legacy-tail migration;
window-event fixtures cover noisy and retained variants. All 413 app tests, CI-mode workspace tests,
fresh app Clippy, and warning-denied workspace Clippy pass. Computer Use moved the rebuilt window:
the log gained only the expected focus loss/gain pair, no move/resize lines, and the nine-of-nine
Library remained healthy. No manual-only item remains.

## Checkpoint (2026-07-18): collision-safe Riot ID matching

The combined audit's L-26 is fixed. League player names now parse into a normalized game name and
an optional normalized full Riot ID. Event attribution requires the full identity when both the
event and local player include taglines, while retaining the name-only fallback when either Live
Client payload omits a usable tagline.

Player-summary lookup scans the entire participant list for an exact full Riot ID before considering
fallbacks. When a participant supplies a valid `riotId`, that identity also takes precedence over
its legacy untagged `summonerName`, so an earlier same-name player with a different tagline cannot
shadow the local player.

Plan commit `af0322a`; implementation commit `2c40f15`. New fixtures put the wrong same-name
participant first, vary case and separator whitespace, reject a foreign taglined event, and retain
untagged compatibility. All 30 League unit tests plus its HTTP, marker, and poll integration tests
pass; fresh crate Clippy, 409 app tests within the CI-mode workspace suite, and warning-denied
workspace Clippy are green. Computer Use verified the rebuilt nine-of-nine Library. No manual-only
item remains for these deterministic payload variants.

## Checkpoint (2026-07-18): explicit event clock-anchor validation

The combined audit's L-25 is fixed. `recording_offset_s` now uses
`Instant::checked_duration_since` and returns a typed `ClockSyncError` when an anchor was sampled
before recording start. Legitimate negative offsets for game events that occurred before recording
remain unchanged; only the invalid wall-clock relation is rejected.

The League poller validates its newly sampled anchor immediately after the game-clock request,
before fetching cumulative event data or advancing `EventTracker`. The neutral error maps to the
existing Live Client invalid-response boundary with a diagnostic, so future/backfill misuse fails
visibly without silently shifting or consuming markers.

Plan commit `ae25fa1`; implementation commit `a4d2ad7`. All 13 event tests pass, including the typed
earlier-anchor case. A League HTTP integration supplies a future recording start, observes the
diagnostic error, and proves the event endpoint receives zero requests; normal negative-offset and
continuity tests remain green. Both changed crates pass fresh warning-denied Clippy, followed by
CI-mode workspace tests and workspace Clippy. Computer Use verified the relinked nine-clip Library.
No manual-only item remains for this latent invariant.

## Checkpoint (2026-07-18): bounded direct-upload retry backoff

The combined audit's L-24 is fixed. Retryable direct object-storage PUT failures now wait between
attempts using 250 ms / 500 ms exponential steps plus deterministic per-upload, part, and attempt
jitter. `Retry-After` delta seconds and HTTP dates become a minimum delay, with all local/server
delays capped at 30 seconds for foreground failure reporting. Tokio timers keep task abort/future
drop cancellation immediate.

Malformed request construction and redirect configuration errors now fall back from the direct
provider immediately; timeout/connect/request/body failures remain retryable. Existing status
policy still refreshes expired 403 presigns and retries 408, 429, and 5xx responses, while provider
fallback and terminal missing-ETag behavior are unchanged.

Plan commit `9083940`; implementation commit `dd896dc`. Pure tests cover deterministic exponential
jitter, server minimums/capping, delta/date/expired/malformed `Retry-After`, and existing integration
tests prove expired presigns still make three spaced PUTs and provider failure still restarts through
proxy. After a fresh app-crate clean, all 409 app tests, CI-mode workspace tests, and warning-denied
workspace Clippy pass. Computer Use verified the rebuilt nine-clip Library. The existing real Cloud
upload acceptance scenario now includes throttled direct-upload timing; no duplicate item was added.

## Checkpoint (2026-07-18): live extracted plugin icons

The combined audit's L-22 is fixed. Parsed profiles and resolved immutable catalog presentation
remain `OnceLock`-cached, while each `list_game_plugins` command gets an owned snapshot and overlays
only extraction-backed icons from the current cache file. A missing file is therefore not memoized:
if detection extracts it later, the next catalog request observes it in the same process. Manifests
with either explicit `extracted` icon mode or no bundled icon share this behavior.

Game detection finishes synchronous icon extraction before emitting its active-game event. The
renderer now refreshes the catalog on that event, updating supported-game rows, rail/cards, and an
open plugin settings dialog without an app restart. File reading/base64 work stays at startup and
game-change command boundaries rather than render paths.

Plan commit `91b1ada`; implementation commit `ea11121`. A temporary cache test proves missing-then-
created icon visibility in one process; catalog tests preserve immutable-cache identity while
requiring independent dynamic snapshots, and the detection refresh has a UI contract. After a
fresh app-crate clean, all 407 app tests, CI-mode workspace tests, JavaScript syntax, and
warning-denied workspace Clippy pass. Computer Use verified the rebuilt nine-clip Library and both
bundled League of Legends/osu! icons in Supported games. No manual-only item remains.

## Checkpoint (2026-07-18): partial local Library scans

The combined audit's L-21 is fixed. Local Library enumeration now returns a typed result with
readable clips plus warnings. Failure to open or enumerate the configured media root remains fatal,
but an unreadable child entry/session is named, logged, skipped, and no longer hides clips from
readable sibling sessions. Sorting and exact-file asset authorization still run over every returned
clip.

The frontend applies a partial-scan warning only after the local request-generation gate accepts
that result, so an older slow refresh cannot overwrite newer Library state. A later complete scan
clears the prior Library warning only when it still owns the visible error text, preserving any
unrelated error that appeared afterward. Warning text is rendered through `textContent`.

Plan commit `252602e`; implementation commit `5e69249`. Deterministic tests inject an access-denied
child beside a readable session and verify the readable clip plus named warning, while a missing
root remains fatal. The warning ordering/clearing UI contract and changed JavaScript syntax checks
pass. After a fresh app-crate clean, all 406 app tests, CI-mode workspace tests, and warning-denied
workspace Clippy pass. Computer Use verified the rebuilt complete Library at nine of nine clips
without a warning. No manual-only item remains for this deterministic enumeration boundary.

## Checkpoint (2026-07-18): serialized microphone test sessions

The combined audit's L-20 is fixed. Microphone test state now owns a monotonic generation and stop
sender. Allocating a generation, stopping the previous session, and installing its replacement are
one locked transaction, so concurrent starts cannot overwrite the only control sender and strand a
worker holding the microphone. Workers stop on either an explicit message or channel disconnect,
and named thread creation is fallible with conditional state rollback.

Live monitor publication and error/stopped completion are serialized against generation
replacement. A superseded worker therefore cannot emit a late level/error event or clear the
newer active session. Explicit stop and replacement also remain ordered after any in-progress
event publication.

Plan commit `0765beb`; implementation commit `065c9a7`. Focused tests cover disconnected control
channels, 12 concurrent replacements with one surviving generation, and stale publish/finish
rejection. After a fresh app-crate clean, all 404 app tests, CI-mode workspace tests, and
warning-denied workspace Clippy pass. Computer Use verified the rebuilt nine-clip Library plus two
real default-microphone start/stop cycles; controls returned to idle and the process settled at 32
threads after stopping. No manual-only item remains for this lifecycle boundary.

## Checkpoint (2026-07-18): validated capture readback boundaries

The combined audit's L-19 is fixed. WASAPI buffers are now viewed only as alignment-one byte
slices and decoded with fixed-size little-endian copies, avoiding typed-slice alignment
assumptions for float32 and PCM16/24/32. Frame/sample/byte arithmetic is checked, truncated or
extra buffers are rejected, and non-silent null buffers fail safely. A packet guard pairs every
successful `GetBuffer` with exactly one `ReleaseBuffer`, including validation errors and unwinding.

NV12 readback validates nonzero even dimensions, row pitch, allocation sizes, plane offsets, and
the complete addressable mapped span before allocation or pointer arithmetic. Null mapped pointers
are rejected. The shared D3D read-map guard now guarantees exactly one `Unmap` on every return and
unwind path for both NV12 and BGRA staging reads.

Plan commit `efac254`; implementation commit `bd2d617`. Misaligned and malformed audio fixtures plus
NV12 dimension/pitch/overflow layout tests pass. Capture has 193 unit, four end-to-end, and one
FFmpeg roundtrip test green; CI-mode workspace tests and warning-denied workspace Clippy also pass
after a fresh capture-crate clean. The current adapter lacks a video processor, so the real NV12
converter device test self-skipped; the existing Windows capture lifecycle acceptance scenario
covers the hardware path and no additional manual-only item is needed.

## Checkpoint (2026-07-18): narrow renderer authority

The combined audit's L-17, L-18, and L-33 are fixed. The renderer no longer sends an external URL
to the native shell. It sends only `remote_clip_id`; native code validates the same conservative ID
alphabet used for Cloud assets, constructs one encoded path segment from the saved public/host URL,
and launches that configured origin. Private deployments and a distinct public frontend remain
supported without granting arbitrary renderer-selected navigation.

Marker presentation now uses shared own-property lookup, so inherited keys such as `constructor`
and `__proto__` cannot become kinds/categories/icons. CSS marker art accepts only a simple bundled
`assets/markers/*.png` path or canonical PNG data URL; invalid art falls back to the existing SVG
glyph. Gallery/review call the same DOM-free helper. The main-window capability now retains only
core defaults, toggle-maximize, close, drag, and the three used autostart operations; direct
minimize remains a native command, while direct maximize/unmaximize/resize grants are gone.

Plan commit `b80fff3`; implementation commit `bdff7aa`. Focused native/player/UI contracts passed,
including inherited-object and CSS-delimiter fixtures. After a fresh app-crate clean, all CI-mode
workspace tests and warning-denied workspace Clippy passed (401 app, 87 player-core, 76 UI-contract
tests). Computer Use verified the rebuilt nine-clip Library and exercised maximize/restore,
minimize/reopen, titlebar dragging, close-to-tray, and single-instance restoration. The app remains
open for testing. Only a real-account Cloud page-origin check remains on the final manual list.

## Checkpoint (2026-07-18): verified FFmpeg release staging

The combined audit's L-13 is fixed. Release staging no longer accepts an arbitrary directory or
copies its contents wholesale. `ffmpeg-runtime.json` pins BtbN's retained
`autobuild-2026-06-30-13-34` x64 LGPL-shared FFmpeg archive, archive digest, exact version and
license-safe configuration, upstream source/build links, and the size/hash of each allowed runtime
file. The selected version3 build excludes GPL/nonfree mode plus libx264/libx265.

`stage-ffmpeg-resource.ps1` hashes the regular archive before opening it, selects only the nine
manifest entries, verifies each extracted file, executes only the verified `ffmpeg.exe` for the
version/configuration probe, and builds the complete resource in an owned temporary directory. It
then atomically replaces staging and emits deterministic `PROVENANCE.json` beside the retained
license and independently replaceable FFmpeg runtime. Release instructions and third-party notices
now document immutable rotation, exact source/build provenance, and LGPL replacement rights.

Plan commit `87c3e32`; implementation commit `2890d0a`. The focused repository contract passed.
A tiny archive with the exact expected name was rejected on SHA-256 before ZIP access. Real staging
removed an injected `evil.dll`, produced exactly 11 resource files, and matched every declared
size/hash plus the receipt. After a fresh app-crate clean, all CI-mode workspace tests and
warning-denied workspace Clippy passed. This batch changes release inputs only, so no native app
rebuild was required. The final acceptance list now includes inspecting both installed variants and
exercising their packaged FFmpeg runtime.

## Checkpoint (2026-07-18): owned dependency and fixed-runtime maintenance

The combined audit's L-12 is fixed. The abandoned `audiopus`/`audiopus_sys` pair is gone. Capture,
MP4 mixing/remux, and app fixtures now share `shiguredo_opus` 2026.1.0 with libopus 1.6.1. Clipline
carries a narrow Apache-2.0 controlled fork because that release publishes `opus.lib` for Windows
while its build script expects `libopus.a`. The fork chooses the correct platform filename and
embeds the reviewed Windows plus Ubuntu 22.04/24.04 artifact hashes; it refuses unknown targets or
changed artifacts. Provenance, exact patches, owner, review deadline, and removal conditions are
recorded beside the fork and in `docs/dependency-policy.json`.

The two `reqwest` release lines cannot safely converge in this repository today: Clipline and the
pinned cloud API use 0.12, while `tauri-plugin-updater` owns 0.13. The exact split is now a quarterly
expiring exception with an upstream convergence trigger. Moving one first-party caller alone would
retain both stacks; downgrading the updater would discard current fixes.

The standalone WebView2 runtime now has a machine-readable version/review manifest and a release
preflight. The script rejects manifest/Tauri path drift, review windows beyond 30 days, overdue
reviews, and a missing staged `msedgewebview2.exe`. The repository contract also expires the review
automatically in CI. Every standalone release must review the official Fixed Version release and
regress H.264/Opus playback plus HEVC/AV1 capability detection.

Plan commit `c6aae09`; implementation commit `706d329`. The fresh build passed 401 app tests, 190
capture tests, 109 MP4 tests, all remaining workspace tests, and warning-denied workspace Clippy.
RustSec reports zero vulnerabilities and 18 informational unmaintained warnings, down from 19.
Computer Use verified the rebuilt nine-clip Library and active H.264/Opus playback advancing from
0:00 to 0:09. The final acceptance list contains the standalone installer/runtime/update test that
requires release staging; existing real capture/export tests cover the new Opus codec boundary.

## Checkpoint (2026-07-18): reproducible dependency security gates

The combined audit's L-11 is fixed. `anyhow` is locked to 1.0.103, clearing
RUSTSEC-2026-0190. Running the newly added RustSec gate also surfaced newer actionable advisories,
so `quinn-proto` is now 0.11.15 and the XML chain is on `quick-xml` 0.41 through `plist` 1.10.
Because released `wayland-scanner` 0.31.10 still pins vulnerable quick-xml 0.39, Cargo temporarily
patches only that build-time crate to the exact upstream commit that already adopted 0.41; there is
no advisory ignore.

All remote workflow actions are pinned to full reviewed commits with version/channel comments,
checkout credentials are not persisted, and workflow tokens are least-privilege. A separate
dependency-security workflow runs RustSec on dependency changes, weekly, and on demand. The checked
in audit policy keeps ignores empty and documents the owner/rationale/expiry/removal requirements
for any future exception. Dependabot proposes weekly Cargo and GitHub Actions updates.

Plan commit `d2b1492`; implementation commit `a1b3e20`. A repository-security integration contract
pins the fixed crate floors, SHA-only remote actions, readable pin comments, RustSec presence,
empty-ignore policy, and both Dependabot ecosystems. The local cargo-audit 0.22.2 scan reports zero
vulnerabilities; its 19 informational unmaintained warnings feed directly into L-12. Fresh-cache app
Clippy, CI-mode workspace tests (401 app tests plus the repository contract), and workspace Clippy
pass with warnings denied. No native or manual-only acceptance item is needed for this CI/lockfile
batch.

## Checkpoint (2026-07-18): pinned League loopback transport

The combined audit's L-10 is fixed. League Live Client bases are now parsed once and accepted only
as plain HTTP(S) root URLs with no credentials, query, or fragment. Numeric IPv4/IPv6 loopback
addresses are retained, while `localhost` is rewritten to `127.0.0.1` before request construction,
so DNS and hosts-file changes cannot move the connection off loopback.

The dedicated reqwest client disables redirects and all configured proxies before enabling invalid
certificates for Riot's self-signed local endpoint. Fixed Live Client paths are joined against the
normalized URL instead of concatenated renderer/configuration text. The existing one-second connect,
two-second request/read, and 4 MiB response bounds remain intact.

Plan commit `783482b`; implementation commit `a49813e`. The League crate has 28 unit tests plus five
integration tests. New coverage pins IPv4/IPv6/localhost normalization, rejects remote hosts and URL
tricks, structurally requires proxy/redirect disabling, and proves a redirect target receives zero
requests. Fresh-cache League Clippy, CI-mode workspace tests (401 app tests), and workspace Clippy
pass with warnings denied. Computer Use verified rebuilt app startup and the nine-clip Library. The
existing real-match/network-interruption acceptance scenario covers endpoint continuity.

## Checkpoint (2026-07-18): backend-owned filesystem authority

The combined audit's L-09 is fixed. Changing the media root now requires an exact, transient
authorization issued by the native folder picker; renderer text alone cannot grant a new root.
The picker starts from the persisted backend setting rather than a renderer-provided path, and
validation rejects filesystem/drive roots plus the Windows profile, Windows, ProgramData, and
Program Files roots. Authorization remains retryable after an unrelated save failure and is
consumed only after the settings/runtime/storage transaction commits.

The asset protocol no longer has static or runtime recursive directory grants. Library MP4s,
generated poster JPEGs, Cloud cache files, and audio previews are canonicalized, containment- and
extension-checked, then granted one exact file at a time. Custom-game icon extraction now accepts a
process id, re-enumerates running windows in the backend, and only passes an existing canonical
local `.exe` to Windows Shell APIs; renderer paths, UNC paths, and device paths are rejected.

Plan commit `03a8776`; implementation commit `f80117b`. The app suite has 401 tests and 74 UI
contracts, including native-folder authorization, sensitive-root rejection, local executable path
validation, and exact-scope ownership. Fresh-cache app Clippy, CI-mode workspace tests, and
workspace Clippy pass with warnings denied. Computer Use verified all nine local posters, live clip
playback, the backend-rooted native folder picker with cancellation, and backend-enumerated custom
game windows without modifying settings or media.

## Checkpoint (2026-07-18): explicit origin-bound plain HTTP consent

The combined audit's L-08 is fixed. Entering a plain-HTTP Clipline Cloud URL now reveals an
explicit checkbox that names the normalized origin receiving the password. The renderer no longer
derives `plain_http_confirmed` from the URL scheme. It blocks `cloud_connect` before invocation
unless the checkbox is checked and its stored origin exactly matches the active normalized origin;
HTTPS requests continue with the flag false.

The acknowledgment is transient and resets when the scheme, host, or effective port changes.
Path-only edits on the same origin retain it. Programmatic host replacement is also safe because
the request-time comparison rejects stale consent even before input-event synchronization. Backend
validation remains authoritative for the limited loopback/private HTTP hosts Clipline permits.

Plan commit `036c882`; implementation commit `962ba5e`. Five pure CloudCore tests cover checked,
unchecked, wrong-origin, wrong-port, and empty consent states, while 73 UI contracts pin the
pre-request guard, explicit control, origin reset, backend flag, and bounded layout. Fresh-cache
app Clippy, CI-mode workspace tests (398 app tests), and workspace Clippy pass with warnings denied.
Computer Use verified the normalized warning and visible checkbox, a blocked unconfirmed connect,
port-change invalidation after consent, and clean wrapping for a long URL. No manual-only item
remains for this finding.

## Checkpoint (2026-07-18): cloud auth preserves unsaved settings

The combined audit's L-07 is fixed. Connect and disconnect now snapshot the complete settings form
before their first await. After authentication changes, a pure CloudCore merge patches only the
backend-owned host/public URL, connected identity, credential target, and upload-record fields into
`currentSettings`, `settingsDraft`, and the dirty-comparison baseline. It no longer calls the full
`fillSettings` repaint that replaced unrelated draft values and controls.

Recording, audio, storage, game, and general edits survive unchanged. User-editable Cloud defaults
and delete-local policy also remain the draft values until Save Settings, while authoritative
account and upload state immediately drives the profile, gallery, and connection UI. Account-key
changes still invalidate cloud request generations and cached listings.

Plan commit `d3c90a9`; implementation commit `4ad75ac`. A pure merge fixture covers unrelated
settings, Cloud preferences, identity, credentials, public URL, cloned upload records, and account
replacement; the 73 UI contracts pin pre-await snapshots and prohibit full settings repaint during
auth refresh. Fresh-cache app Clippy, CI-mode workspace tests, and workspace Clippy pass with
warnings denied. Computer Use verified the rebuilt Cloud settings pane and clean return to the
nine-clip Library. The existing real-account credential acceptance scenario now also checks draft
preservation across reconnect/disconnect.

## Checkpoint (2026-07-18): isolated concurrent poster generation

The combined audit's L-06 is fixed. Every FFmpeg poster attempt now reserves a distinct sibling
temp file with `create_new` and a process/counter identity. An RAII owner removes exactly that file
on spawn failure, encode failure, publish failure, or early return, so overlapping attempts cannot
delete or overwrite one another and no in-flight-key map can grow over time.

Only a successful FFmpeg exit reaches publication. Windows uses `MoveFileExW` with replace-existing
and write-through flags to atomically replace a stale cached poster; other platforms use the native
rename boundary. The visible poster is therefore always either the previous complete JPEG or one
new complete JPEG, even when two requests finish together. This also corrects stale-poster refresh
on Windows, where plain `std::fs::rename` could not replace an existing destination.

Plan commit `9440a95`; implementation commit `509e5cd`. The app suite now has 398 unit tests,
including independent concurrent reservations, owner-scoped cleanup, and real Windows atomic stale
replacement. Fresh-cache app Clippy, CI-mode workspace tests, and workspace Clippy pass with warnings
denied. Computer Use verified normal startup and complete cached thumbnails across the nine-clip
Library. No manual-only item remains for this filesystem concurrency boundary.

## Checkpoint (2026-07-18): validated multipart upload work lists

The combined audit's L-05 is fixed. Before either authenticated proxy upload or direct object-store
upload reads a chunk, one shared validator now checks the server's complete missing-parts list. Part
size must be positive and within the 64 MiB client bound, the file-derived part count must fit the
protocol, and every part number must be nonzero, unique, and within the file-derived range. Valid
resumable subsets retain their server-provided order. The file reader keeps its per-part checks as a
second defensive boundary.

The H-05 file-streaming batch had already replaced `saturating_sub(1)` and rejected part zero at the
reader. This batch closes the remaining list-level gap, preventing duplicate chunks from being sent
and acknowledged twice and preventing malformed work from reaching either network transport.

Plan commit `6ba62d0`; implementation commit `b353966`. The app suite now has 396 unit tests; new
fixtures cover zero, duplicate, out-of-range, empty, reordered valid, proxy, and direct work lists.
Fresh-cache app Clippy, CI-mode workspace tests, and workspace Clippy pass with warnings denied.
Computer Use verified normal startup and the nine-clip Library. No manual-only item remains for this
malformed-protocol boundary.

## Checkpoint (2026-07-18): unified keyboard contracts

The combined audit's L-03 is fixed. Settings parsing now produces one crate-private typed hotkey
specification containing modifier state and a distinct function-key, keyboard-key, or mouse-button
value. The Windows low-level hook maps that specification directly to virtual keys instead of
reparsing the normalized display string, so literal `Ctrl+Shift+F` can no longer be mistaken for a
malformed function key while `F1` through `F24` and mouse buttons retain their existing mappings.

The orphaned review-player `KeyF` intent was removed because focus mode and its UI had already been
removed; the browser event is no longer prevented for an action the dispatcher cannot perform. The
global player shortcut guard now derives modal ownership from `document.querySelector("dialog[open]")`
instead of an incomplete dialog-id list, automatically covering detected-games, window-picker,
rename-file, and future native dialogs while preserving the separate Settings and form guards.

Plan commit `94ab793`; implementation commit `cc836fa`. The app suite now has 394 unit tests, 86
player-core tests, and 72 UI contracts, including literal/function/mouse virtual-key identity,
released `KeyF`, and data-driven modal ownership. Fresh-cache app Clippy, CI-mode workspace tests,
and workspace Clippy pass with warnings denied. Computer Use verified normal startup, the Hotkeys
settings pane with both binding fields, and clean close back to the nine-clip Library. No new
manual-only item remains for this deterministic contract.

## Checkpoint (2026-07-18): exact Windows native-resource ownership

The combined audit's L-01 is fixed. WASAPI mix formats now carry an explicit borrowed-stack or
owned-COM allocation variant. Only the `GetMixFormat` variant calls `CoTaskMemFree`, and RAII frees
it on unsupported-format, initialization, service, start, and success paths. The fixed process
loopback format can no longer reach a stack-pointer free. The finding's unused event-handle branch
had already disappeared with M-14's pull-mode process loopback conversion and was verified absent.

Media Foundation `ProcessOutput` now writes into an owned guard whose `pSample` and `pEvents` fields
release on every success, stream-change, missing-sample, and arbitrary error branch. Taking a sample
atomically replaces its owner slot with `None`, so packet conversion errors release the moved sample
normally while the guard releases only remaining fields.

Plan commit `b3ffca4`; implementation commit `3c5d059`. The capture suite now has 190 unit tests,
including borrowed/COM wave-format ownership and drop-spy coverage for taken, cleared, and untouched
`ManuallyDrop` values. Fresh-cache capture Clippy, CI-mode workspace tests (393 app tests), and
workspace Clippy pass with warnings denied. Computer Use verified normal startup and the nine-clip
Library. No new manual-only item remains beyond the existing Windows capture lifecycle scenario.

## Checkpoint (2026-07-18): enforced shared D3D11 synchronization

The combined audit's M-23 is fixed. The Windows D3D wrapper now has one idempotent guard that casts
to `ID3D10Multithread`, enables protection when absent, and verifies the device reports protection
before returning. Clipline-created hardware and WARP devices use that same guard instead of a
separate unchecked setter.

Every safe boundary that accepts and then shares a caller-provided D3D11 device now establishes the
invariant before immediate-context work: WGC and DXGI capture construction, D3D video-processor
conversion, NV12/BGRA readback, GPU and CPU FFmpeg encoder construction, and the D3D-aware Media
Foundation encoder. Query/enable failures propagate through the existing capture, Windows, or
encoder error type instead of proceeding with an undocumented concurrency precondition.

Plan commit `fe22cca`; implementation commit `fe55590`. The capture suite now has 187 unit tests.
A WARP test starts from deliberately disabled protection and covers enable/idempotence; the public
BGRA readback test proves that boundary repairs the same device. On the real interactive desktop,
the caller-provided WGC constructor also restored deliberately disabled protection and captured a
frame. Fresh-cache capture Clippy, CI-mode workspace tests (393 app tests), and workspace Clippy pass
with warnings denied. Computer Use verified normal startup with all nine clips visible. No new
manual-only item remains beyond the existing Windows capture lifecycle acceptance scenario.

## Checkpoint (2026-07-18): generation-safe local Library refreshes

The combined audit's M-22 is fixed. Every local `list_clips` request now owns a monotonically newer
generation and may mutate `clipsCache`, the active review, or the gallery only while it remains the
latest request. Superseded successes and failures are ignored. Successful rename, delete, and export
mutations explicitly invalidate snapshots that began before their optimistic cache update, so an
older filesystem view cannot undo the mutation or close a newly updated review.

Saved and osu! enrichment events now use one fire-and-forget refresh wrapper that catches current
failures and reports them through the existing visible error surface. Awaited settings, upload, and
startup refreshes retain their existing propagation, while local/cloud source switching and the
separate cloud account-scoped request gate are unchanged.

Plan commit `1f05190`; implementation commit `9cebaf5`. The 71 UI contracts pin generation checks,
pre-mutation invalidation, and caught event refreshes; the existing request-gate unit tests cover
supersession and invalidation behavior. JavaScript syntax checks, fresh-cache app Clippy, CI-mode
workspace tests (393 app tests), and workspace Clippy pass with warnings denied. Computer Use
verified the nine-clip Library and opening a clip into review. No manual-only acceptance item remains
for this deterministic race.

## Checkpoint (2026-07-18): verified writable media-root fallback

The combined audit's M-21 is fixed. Recording now verifies a configured media directory by
atomically reserving a unique probe file, writing and syncing one byte, and removing the probe.
An existing but unwritable, disconnected, full, or otherwise unusable root therefore falls back to
the default `Videos\Clipline` directory instead of passing `create_dir_all` and failing later. The
fallback receives the same probe, and a double failure reports both paths and causes.

The recorder publishes its actual resolved root before normal status events. Shared Library state
and the WebView asset scope follow that root, so fallback clips appear and play immediately instead
of leaving the UI pointed at the unavailable configured folder. Settings saves apply the same
writable preflight before committing runtime or persisted changes. Routine Library reads do not
repeat the durable probe, avoiding a disk/network sync on every refresh.

Plan commit `4fe2d31`; implementation commit `410a7da`. The app suite now has 393 unit tests and 70
UI contracts, including injected existing-directory ACL denial, fallback failure diagnostics,
probe cleanup, and resolved-root state/scope propagation. Fresh-cache app Clippy, CI-mode workspace
tests, and workspace Clippy pass with warnings denied. Computer Use verified normal startup with all
nine clips visible and the Settings UI opening. A real unwritable/removable-volume scenario remains
on the final manual acceptance list.

## Checkpoint (2026-07-18): scoped built-in and custom game identities

The combined audit's M-20 is fixed. Built-in IDs now live in one reserved catalog and runtime game
identity is explicitly `BuiltInPlugin` or `Custom`; detection, event-source selection, osu! title
tracking, active-rule continuity, session metadata, and the osu! minimum-duration policy no longer
infer privileges from an unscoped string. A custom identity cannot become a plugin even if an
adversarial test gives it the text `osu` or `league_of_legends`.

Persisted custom IDs must use a bounded canonical `custom-` slug namespace. Settings normalization
deterministically migrates built-in collisions, empty IDs, and legacy/malformed IDs to unique
`custom-migrated-…` values before they reach runtime. Each migrated record retains a bounded legacy
ID alias alongside its name and embedded icon. Historical session metadata resolves that exact
alias plus name to the custom icon and is explicitly excluded from built-in plugin presentation.
New frontend IDs reserve the live built-in catalog as an additional defense.

Plan commit `0e07f88`; implementation commit `2d0a33f`. The app suite now has 390 unit tests and 69
UI contracts, including deterministic collision migration/idempotence, namespace validation,
custom-impostor event/title/duration isolation, and historical icon routing. Fresh-cache app
Clippy, CI-mode workspace tests, and workspace Clippy pass with warnings denied. Computer Use
verified the nine-clip library and Settings > Games with League of Legends and osu! isolated from
the empty custom-game list. No manual-only acceptance item remains for this finding.

## Checkpoint (2026-07-18): owned and retryable Windows file clipboard

The combined audit's M-18 is fixed. Clipboard file-copy commands now derive a real native owner
from the invoking Clipline webview window, retry a busy clipboard for a short bounded interval,
and call `EmptyClipboard` before publishing `CF_HDROP`. The movable allocation transfers to
Windows only after `SetClipboardData` succeeds; every failure path closes an opened clipboard and
frees the allocation exactly once.

Plan commit `b941c91`; implementation commit `68bbc82`. A deterministic transaction test covers
busy retries, exact open/wait/empty/set/close order, empty/set failures, and never closing a
clipboard that was not opened. The UI contract pins native-window injection and ownership setup.
Fresh-cache app Clippy, CI-mode workspace tests (386 app tests), and workspace Clippy pass with
warnings denied. Computer Use exercised Copy Clip from the real review UI and PowerShell verified
one existing `.mp4` in Windows' file-drop clipboard. Brief and persistent contention remain on the
final manual acceptance list because they require another desktop clipboard owner.

## Checkpoint (2026-07-18): lossless MP4 track timing and codec arrays

The combined audit's M-17 is fixed, along with the pending L-02/L-27/L-28 overlaps. The hybrid
writer now accepts checked absolute per-track decode times, emits those times in fragmented
`tfdt` boxes, and records presentation runs separately from contiguous media samples. Finalized
files use versioned edit lists for leading and internal silence/blank spans; the 720 kHz movie
clock exactly represents Clipline's 90 kHz video and 48 kHz Opus clocks. Track and movie durations
cover the real presentation end while `mdhd` continues to describe encoded media duration.

Finalized-file parsing maps supported version-0/1 edit lists back to integer presentation ticks and
rejects rate-adjusted, negative, overlapping, backward, or mid-sample edits. Trim snaps and selects
on integer/rational boundaries, rebases each retained track to the aligned video origin, and keeps
later gaps. All in-memory, file-backed, selected-audio, and mixed-audio remux paths write contiguous
runs at their original times. Replay segments now retain each audio track's first packet PTS in RAM
and disk storage; replay and full-session output use those stamps, including audio-empty GOPs and
later discontinuities. Cumulative endpoint quantization prevents per-frame rounding drift.

H.264 and HEVC configs now retain every SPS/PPS/VPS entry through `avcC`/`hvcC` parse, trim, and
remux while singleton encoder constructors stay ergonomic. Writer configuration is validated before
output mutation, scalar reads cannot borrow bytes from sibling boxes, reserved eight-layer HEVC
metadata is rejected, and malformed public sample metadata returns `InvalidData` instead of
panicking.

Plan commit `d694c69`; implementation commit `ec6f373`. Focused results: 109 MP4 tests, 17 buffer
tests, and 186 capture tests. CI-mode workspace tests (385 app tests) and fresh/workspace Clippy pass
with warnings denied. Deterministic fixtures cover delayed onset, an empty audio GOP, an internal
gap, replay/full-session edit lists, integer trim rebasing, malformed edits, complete multi-parameter
arrays, and Opus pre-skip continuity. One real playback acceptance item was added for delayed/gapped
audio export.

## Checkpoint (2026-07-18): bounded FFmpeg subprocess lifecycle

The combined audit's M-15 is fixed. Probe commands now start a named stdout reader immediately,
retain at most 4 MiB, and continue draining excess bytes through EOF while the parent polls the
child. One shared deadline primitive returns a real exit status or kills and reaps on timeout;
`try_wait` errors also trigger best-effort kill/reap cleanup. Probe spawn/reader setup failures no
longer leave a live child behind.

Encoder finish closes stdin, lets the existing stdout reader drain concurrently while FFmpeg gets
a documented 30-second flush grace, and waits for the process before joining the reader. A timeout
kills/reaps first, then joins/drains and reports that the encoded tail was discarded. `Drop` uses
the same finite cleanup and recognizes an encoder already cleaned by `finish`. Normal exit still
preserves tail packets and then applies reader, exit-status, and input/output-count validation.

Plan commit `75acdf6`; implementation commit `8ff611e`. The 185 capture unit tests include an
8 MiB probe burst retained at a 1 MiB test cap, bounded-reader exhaustion, wedged probe kill/reap,
wedged encoder kill-before-join, and a normal two-picture encoded tail. Fresh-cache capture Clippy,
CI-mode workspace tests (385 app tests), and workspace Clippy pass with warnings denied. The real
FFmpeg/mux integration self-skipped because no FFmpeg binary was discoverable on this machine.
Computer Use verified normal startup with all nine clips at 6.2 MB. No manual-only acceptance item
remains for the deterministic process lifecycle.

## Checkpoint (2026-07-18): Windows capture lifecycle contracts

The combined audit's M-14 is fixed. Per-process WASAPI loopback no longer requests event-callback
mode and then ignores the registered event. It now uses the supported shared pull model with
loopback/autoconversion flags and a one-second device buffer, matching Clipline's endpoint polling
headroom. The existing recorder cadence drains it every video step, including duplicate frames for
an idle WGC source. Unused event creation, registration, handle storage, and teardown are removed.

WGC now registers `GraphicsCaptureItem.Closed` and retains both the `Closed` and `FrameArrived`
tokens. Target closure atomically marks the bounded queue closed, discards queued stale textures,
wakes a blocked receiver, and rejects later frame callbacks even though their sender clones remain
alive. The handlers are revoked during teardown. `next_frame_timeout` reports the closed channel as
end-of-stream, which `CadencedCapture` propagates instead of manufacturing another frozen frame.

Plan commit `4a8112e`; implementation commit `e3190a0`. The 178 capture tests include pull-mode
configuration, a real process-loopback start/poll/drop smoke, explicit queue close with retained
callback senders, and blocked-receiver wakeup; the app suite adds cadence closure propagation for
385 tests. Fresh-cache capture/app Clippy, CI-mode workspace tests, and workspace Clippy pass with
warnings denied. Computer Use verified normal startup with all nine clips at 6.4 MB. Continuous
real process audio during a static image and closing a live captured window are on the final manual
acceptance list because they require actual Windows audio and capture-item events.

## Checkpoint (2026-07-18): bounded pending audio and clock discontinuities

The combined audit's M-13 is fixed. The recorder now reserves encoded payload bytes for every
pending audio track as well as the current video GOP and any pre-keyframe video. Lead-in removal
and each segment seal recalculate the retained audio reservation, so old tracks do not accumulate
against later GOPs. The shared pending ceiling remains the smaller of the replay budget and 64 MiB.
A broken encoder that fails to close a GOP for ten seconds now stops with an explicit keyframe/GOP
duration error even when its encoded payload remains small.

Large positive WASAPI timestamp gaps still allocate at most five seconds of silence, but the PCM
assembler now records a monotonic timeline anchor at the absolute stereo-pair boundary where the
source resumes. The bounded silence is shortened by at most one 20 ms frame to end on an Opus
packet boundary. The first resumed packet lands on the new source timestamp and subsequent packets
continue at 20 ms cadence instead of remaining permanently behind by the discarded clock gap.

Plan commit `d2e6517`; implementation commit `05152fd`. The 174 capture unit tests include
combined audio/video pressure, per-GOP reservation release, duration failure, one-hour clock jumps,
post-jump cadence, and a discontinuity after partial PCM. Fresh-cache capture Clippy, CI-mode
workspace tests (384 app tests), and workspace Clippy pass with warnings denied. Computer Use
verified normal startup with all nine clips at 6.4 MB. No manual-only acceptance item remains for
these deterministic resource and timeline state machines.

## Checkpoint (2026-07-18): bitstream-authored picture and sync boundaries

The combined audit's M-12 is fixed. H.264 and HEVC Annex-B framing now uses access-unit
delimiters plus the codecs' first-slice fields, so every standards-valid multi-slice picture stays
one MP4 sample. Parameter-set and SEI prefix NALs after a completed picture are held for the next
picture. The streaming classifier still works when any start code or slice header is divided
across stdout reads.

AV1 sync status now comes from the frame/frame-header OBU rather than configured GOP position;
reduced still-picture streams and `show_existing_frame` are handled explicitly, while malformed
or metadata-free temporal units fail the encoder. FFmpeg output consumes exactly one queued input
timestamp per encoded picture. Extra output and missing output at finish are encoder errors rather
than causes to synthesize timestamps and silently desynchronize a replay.

Plan commit `a8b92a9`; implementation commit `68c6606`. The 170 capture unit tests include new
multi-slice H.264/HEVC, AV1 frame-type, malformed-metadata, and timestamp-cardinality regressions.
The FFmpeg/mux integration now asserts exactly one packet per input frame, though it self-skipped
on this machine because FFmpeg was not on `PATH`. Fresh-cache capture Clippy, CI-mode workspace
tests (384 app tests), and workspace Clippy pass with warnings denied. Computer Use verified normal
startup with all nine clips at 6.5 MB. No manual-only acceptance item remains for the deterministic
bitstream rules; supported real encoder fixtures remain covered whenever the integration binary is
available.

## Checkpoint (2026-07-18): bounded incremental Annex-B framing

The combined audit's M-11 is fixed. `AnnexBFramer` no longer allocates a complete start-code list
or rescans its accumulated buffer on every FFmpeg stdout chunk. It retains one incremental scan
cursor, the current access-unit start, and the most recent incomplete NAL boundary. A NAL is
classified exactly once when the following start code arrives, and all offsets are adjusted when
emitted prefixes are drained.

The 32 MiB ceiling is checked with overflow-safe `current + incoming` arithmetic before extending
the buffer, including the no-start-code path that previously returned before its guard. Exceeding
the limit clears the entire framing generation and every cursor/boundary field; no suffix is kept,
so discarded zero bytes cannot combine with a future chunk into a synthetic delimiter. Valid
three- and four-byte start codes remain recognized across every reader split point.

Plan commit `1f8d1f4`; implementation commit `725a310`. All eight framing tests pass, including
incremental delimiter-free scanning, cap/reset, every four-byte-code split, and post-reset
non-merging. Fresh-cache capture Clippy, CI-mode workspace tests (384 app tests), and workspace
Clippy pass with warnings denied. Computer Use verified normal startup with all nine clips at
6.4 MB. No manual-only acceptance test remains for this pure byte-stream boundary.

## Checkpoint (2026-07-18): durable single-flight osu! enrichment

The combined audit's M-09 is fixed. Startup, library refresh, connection tests, and completed-save
triggers now acquire a process-wide lease keyed by the canonical configured media root. An
overlapping pass for that root coalesces instead of issuing duplicate API requests or racing queue
files; other roots remain independent and RAII releases the lease on every return/error path. The
save trigger now uses the configured root rather than treating its session folder as another key.

Persisted queue state now schedules work. New jobs run immediately; pending attempts back off from
one minute to a six-hour cap, and `Failed` legacy jobs re-enter after a six-hour delay capped at one
day. A pass fetches only for due jobs, and a failed shared API fetch atomically increments those
jobs so repeated refreshes cannot hammer the service. Malformed, unreadable, mismatched, or missing
jobs are logged and moved to unique `.invalid.<pid>.<counter>` siblings individually; valid jobs in
the same directory continue and quarantine files are never rediscovered.

All pending/retry/failed/marker JSON now publishes through unique create-new sibling temporaries,
file sync, and replace-existing/write-through rename. Owned temporaries clean themselves on every
failure, eliminating partial JSON and breaking any swapped link at publication rather than writing
through it.

Plan commit `0b72632`; implementation commit `16b20f1`. Eighteen focused enrichment tests plus
worker-lease and no-credential tests cover coalescing, independent roots, retry caps, failed-record
re-entry, atomic replacement, mixed malformed/valid discovery, and quarantine. Fresh-cache app
Clippy, CI-mode workspace tests (384 app tests), and workspace Clippy pass with warnings denied.
Computer Use verified normal startup with all nine clips at 6.4 MB. No manual-only acceptance test
remains for these deterministic worker and persistence guarantees.

## Checkpoint (2026-07-18): osu! enrichment filesystem boundary

The combined audit's M-08 is fixed. Discovery no longer returns bare deserialized enrichment
records whose embedded `clip_path` controls later I/O. It returns a path-bound job: the pending
sidecar is the actual regular file found under the canonical media root, and the MP4 is derived
from that sidecar's filename and directory. The serialized path remains only a schema-v1
consistency check and must canonicalize to that exact MP4.

Discovery accepts only an existing regular `.mp4` at the media root or one session directory
below it. It rejects mismatched/missing targets, sidecar or media reparse points, and linked session
directories. Marker publication, retry/failure rewrites, and completion deletion use only the
private bound paths, so crafted JSON cannot redirect a write or deletion. Clipline's existing
rename transaction continues rewriting the compatibility field when it moves a pending clip.

Plan commit `d1fdbf6`; implementation commit `d143dbc`. Fifteen focused enrichment tests cover
outside-path injection, missing MP4s, linked directories, safe retry targeting, discovery, and
score mapping. Fresh-cache app Clippy, CI-mode workspace tests (380 app tests), and workspace
Clippy pass with warnings denied. Computer Use verified normal startup with all nine clips at
6.5 MB. No manual-only acceptance test remains for this deterministic path boundary.

## Checkpoint (2026-07-18): League poller match continuity

The combined audit's M-07 is fixed. The League poller now owns one `EventTracker` for its whole
lifetime, so a failed Live Client request cannot discard the cumulative-event watermark. Each
successful batch compares both Riot's maximum event ID and game clock with the prior successful
batch. A rollback resets the watermark and emits the old-match/new-match boundary before the new
match's first event; small clock corrections do not reset it.

Polling failures receive bounded exponential backoff and a six-consecutive-failure grace window.
A brief outage emits no boundary, while sustained absence ends an active match once. `GameEnd`
still closes immediately, and an endpoint that lingers on its completed cumulative payload cannot
start a duplicate session. Tracker identity survives sustained absence, while the local player is
re-acquired when the endpoint returns. Heartbeats during unavailable-game waits and retry sleeps
make a dropped recorder receiver terminate the otherwise idle poller thread.

Plan commit `4af92c3`; implementation commit `905d976`. Six deterministic app lifecycle tests,
25 League unit tests, and five League HTTP/end-to-end tests pass, including a real mock-server
failure/recovery sequence that emits only the later event. Fresh-cache Clippy for both changed
crates, CI-mode workspace tests (376 app tests), and workspace Clippy pass with warnings denied.
Computer Use verified the rebuilt app renders all nine clips at 6.6 MB. A short real-match League
endpoint interruption and the following match remain on the final manual acceptance list.

## Checkpoint (2026-07-18): bounded remote HTTP operations

The combined audit's M-05 is fixed. Desktop control requests now share a client with a five-second
connect timeout, 15-second read-idle timeout, 30-second total deadline, and redirects disabled.
Authenticated media streams use the same connect boundary plus a 30-second read-idle deadline
without a short total cap; upload requests receive a size-aware deadline based on a 256 KiB/s
minimum rate (60-second floor, 24-hour ceiling). Token-free object uploads keep a separate client.

All Cloud and osu! success JSON is streamed through a 4 MiB bound, diagnostic/error bodies through
64 KiB, and avatars through their existing 2 MiB image bound. The reader rejects deceptive
`Content-Length` values before buffering and enforces the same cap chunk by chunk. Cloud connect,
identity, listing, clip status, visibility, upload controls, assets, and osu! token/user/score
requests no longer use fresh default clients or unbounded `json`/`text` reads. Cloud listing stops
at 100 pages / 10,000 unique clip ids and returns a visible truncation warning. The loopback League
client adds connect/read deadlines and rejects JSON over 4 MiB.

Plan commit `acb3326`; implementation commit `3a51d1b`. Three bounded-reader/deadline tests, 15
upload tests, 40 Cloud tests, five osu! tests, 22 League unit tests plus its HTTP integrations, and
the cloud-library UI contract pass. Fresh-cache Clippy for both changed crates, CI-mode workspace
tests (370 app tests), and workspace Clippy pass with warnings denied. Computer Use verified the
rebuilt app renders all nine clips at 6.5 MB. Real Cloud/osu!/League continuity remains on the
manual acceptance list because it requires live accounts and a running game.

## Checkpoint (2026-07-18): recoverable settings startup

The combined audit's M-03 is fixed. Startup now distinguishes a first-run missing file from an
unreadable path and structurally invalid JSON/settings. Every successful replacement first
publishes the prior valid bytes atomically as `settings.json.bak`. A missing or invalid primary
recovers that last-known-good copy; proven-invalid files are moved to unique `.corrupt.<pid>.<n>`
siblings, while unreadable paths are left untouched. If neither generation is usable, Clipline
uses safe defaults only with an explicit diagnostic naming the preserved/quarantined files.

Normal saves refuse to replace an existing primary that cannot first be read and validated, so a
transient sharing/permission problem cannot turn a later save into silent data loss. Field-level
legacy repair remains on the normal path. Recovery diagnostics are held until `frontend_ready`
and drained once into the persistent renderer error area, avoiding setup-time events emitted
before WebView listeners exist.

Plan commit `00cf25a`; implementation commit `63dca68`. All 63 focused settings tests, the startup
warning unit test, and the UI readiness contract pass. Fresh-cache app Clippy, CI-mode workspace
tests (including 367 app tests), and workspace Clippy pass with warnings denied. Computer Use
verified normal startup with all nine clips at 6.5 MB, then launched a disposable corrupt profile
and visibly confirmed both the safe-default warning and its quarantined file before restoring the
normal profile. No manual-only acceptance test remains for this finding.

## Checkpoint (2026-07-18): transactional settings and credentials

The combined audit's M-02 is fixed. Backend-owned Cloud and osu! settings now stage a normalized
copy, persist it, and publish it to live memory only after the write succeeds. The main settings
save applies global hotkeys, the low-level keyboard hook, tray labels, and release autostart as a
transaction: any later persistence or recorder-commit failure restores the old settings file and
rolls back every already-applied runtime/OS side effect. Partial hotkey registration failures also
restore earlier removals and surface any rollback failure instead of silently leaving a mixed
configuration.

Credential replacement now snapshots the previous Windows Credential Manager value, writes the
replacement, and compensates if settings persistence fails. Obsolete Cloud and osu! credential
targets are first recorded as durable pending cleanup, then deleted; failed cleanup is retried by
the next status check rather than losing ownership. Renderer saves preserve these backend-owned
cleanup fields, and no secret is written to `settings.json`.

Plan commit `1cec26b`; implementation commits `99d5e7d` and `fc647fb`. The 57 settings tests,
57 app command tests, 40 Cloud tests, five osu! tests, and four credential-transaction tests pass.
Fresh-cache app Clippy, CI-mode workspace tests, and workspace Clippy pass with warnings denied.
Computer Use verified an unchanged Settings save reports `saved` in the rebuilt native app while
all nine clips remain visible. Installed-release autostart/hotkey rollback and real Credential
Manager migration/cleanup remain on the final manual acceptance list.

## Checkpoint (2026-07-18): authenticated upload origin boundary

The combined audit's M-01 is fixed. Every server-provided URL that receives the Clipline Cloud
bearer token—single-PUT content, direct-S3 presign control, and direct-S3 acknowledgement—must now
match the configured cloud's normalized scheme, host, and port. Cross-origin URLs, port changes,
HTTPS-to-HTTP downgrades, and embedded URL credentials are rejected before a request is sent.

Authenticated upload requests use a dedicated HTTP client with redirects disabled, so the cloud
cannot redirect a token-bearing create/control request elsewhere. Token-free presigned object
storage PUTs retain a separate client and remain cross-origin capable; the existing two-server S3
test proves that intended path still works.

Plan commit `0d9561f`; implementation commit `716b3d3`. All 15 upload transport tests pass,
including a real redirect target that receives zero requests and same-origin/cross-origin/port/
scheme cases. Fresh-cache app Clippy, CI-mode workspace tests, and workspace Clippy pass with
warnings denied. Computer Use verified the rebuilt native app renders all nine clips and
Local/Cloud controls at 6.7 MB idle RAM. A normal upload against the real configured cloud remains
covered by the existing manual cloud-upload acceptance test.

## Checkpoint (2026-07-18): replay-cache lifecycle safety

The combined audit's M-06 is fixed. Disk replay segments now publish through owned temporary and
final-file guards, commit bookkeeping only after required eviction succeeds, and keep bookkeeping
consistent when an eviction fails partway through. Dropping a disk ring removes its entire unique
Clipline-owned run directory, including orphaned temporary files and its ownership record.

Each disk-cache run records the Windows process-instance identity (PID plus creation time) and its
creation timestamp. Startup scans only structurally valid Clipline run names, skips links/reparse
points, immediately removes definitively dead/reused instances, and gives missing, corrupt, or
unqueryable identities a 24-hour safety window. Bytes in every preserved run reduce the new ring's
quota. A prepared run remains under an RAII cleanup guard until recorder construction succeeds.

The periodic 2 GiB free-space check now passes through `finish_stream` and full-session
finalization before the recorder reports its primary low-space error; any secondary finish error
is retained in the report. Capture failures use the same path, and all fallible media-folder setup
now happens before recorder ownership begins.

Plan commit `c180bf2`; implementation commit `52eb9f4`. Sixteen buffer tests and 42 focused service
tests pass, including publication/eviction failures, live/stale/ambiguous run recovery, quota
accounting, constructor rollback, and low-space finalization. Fresh-cache Clippy for both changed
crates, CI-mode workspace tests, and workspace Clippy pass with warnings denied. Computer Use
verified the rebuilt native app renders all nine clips and Local/Cloud controls at 6.6 MB idle RAM.
Crossing the 2 GiB reserve during a real disk/full-session recording remains on the manual list.

## Checkpoint (2026-07-18): bounded cloud media cache

The combined audit's M-04 is fixed. Bulk cloud media now lives under LocalAppData rather than the
roaming settings tree. The first cache use migrates only valid 16-hex account namespace
directories from the legacy roaming root, skips reparse-linked directories, and leaves unrelated
legacy files untouched.

Cloud media is capped at 4 GiB per file, the cache at 10 GiB aggregate, and downloads reserve a
2 GiB free-space floor before allocating. Completed entries and their `.ok` markers are accounted
and evicted together in least-recently-used order. Cache hits refresh recency. In-flight and
returned playback targets receive 24-hour process leases; if only leased media could satisfy
pressure, the download fails clearly instead of invalidating playback.

Download temporaries use unique `create_new` paths and an ownership guard. Pruning deletes only
Clipline-patterned temps older than one day, never an active or arbitrary `.tmp`, and recursive
accounting refuses symlinks/reparse points. Publication and capacity accounting are serialized.

Plan commit `dddb9cd`; implementation commit `d54426b`. Forty focused cloud tests, fresh-cache app
Clippy, CI-mode workspace tests, and workspace Clippy pass with warnings denied. Computer Use
verified the rebuilt app renders all nine clips and Local/Cloud controls at 6.4 MB idle RAM. A real
multi-clip cloud eviction/playback run remains on the manual acceptance list.

## Checkpoint (2026-07-18): bounded large-file transforms and upload

The combined audit's H-05 and M-16 are fixed. File trim and audio-selection remux now load only a
bounded finalized `moov` box, retain the source file's absolute sample offsets, and copy media with
a 64 KiB buffer. Multi-track audio mixing decodes one Opus packet per selected track at a time,
spools encoded mixed packets to a unique file, and muxes source video plus spooled audio without
materializing the MP4. Clipboard sharing uses these file APIs instead of a source/output `Vec`.

Cloud upload now owns a path/size/checksum payload rather than bytes. SHA-256 is computed in a
streaming pass, single PUT uses a streaming request body, and resumable proxy/direct uploads seek
and read only one part at a time. Server part sizes above 64 MiB are rejected before allocation.
Original uploads use the source directly; selected-audio variants use reserved `.tmp` files that
are removed on every ordinary exit, while abandoned Clipline-owned temps older than one day are
reclaimed without touching unrelated or active files.

Every file transform rejects source/target identity through Windows file ids (so distinct hard
links are safe), writes to a unique `create_new` sibling, flushes/syncs, and publishes with an
atomic replace only after finalization. Injected late failures preserve the prior target and clean
the partial output.

Plan commit `aa6e177`; implementation commit `db86efe`. The 100-test MP4 unit suite, 12 cloud
transport tests, selected-payload/clipboard tests, CI-mode workspace tests, fresh-cache changed-
crate Clippy, and workspace Clippy all pass with warnings denied. Computer Use verified the rebuilt
app opens with all nine local clips, Local/Cloud controls, and 6.4 MB idle RAM. No real cloud upload
or multi-gigabyte user-file operation was performed; those remain on the manual acceptance list.

## Checkpoint (2026-07-18): remove unsafe full-application elevation

The combined audit's H-01 is fixed by removing the privilege boundary rather than partially
filtering subprocess paths. Clipline no longer exposes a `restart_as_administrator` command,
invokes `ShellExecuteW("runas")`, accepts a privileged handoff argument, waits for an unelevated
parent, or offers a UAC action in the renderer. This also closes L-23: there is no elevated restart
that can discard the original command-line behavior overrides.

Elevated-game detection remains read-only and preserves process-instance identity, so Clipline can
still explain once per game process why Windows blocks focused hotkeys. The dialog now recommends
running the game without administrator privileges and has only a dismiss action. Building a
protected signed broker remains a possible future product feature, but the current per-user app
does not cross the administrator boundary.

Plan commit `65d1bb1`; implementation commit `5d06c21`. All 68 UI contracts, focused elevation and
Windows identity tests, CI-mode workspace tests, fresh-cache app clippy, and workspace clippy pass
with warnings denied. Manual acceptance still needs an actually elevated game process to verify
the final warning copy and absence of any UAC/restart action.

## Checkpoint (2026-07-18): cloud upload durability boundary

The combined audit's H-03 is fixed. Post-upload polling no longer treats the first successful
metadata response as proof that the clip is usable. It continues through `processing`, accepts
only explicit `ready`, treats explicit `failed` as terminal, and preserves the local clip on poll
timeout, HTTP error, visibility-update error, or any unknown state. Every such outcome persists the
remote id/link plus a reconcilable status and error instead of escaping through IPC while leaving
the saved upload record stuck at `processing`.

When delete-local-after-upload is enabled, a ready metadata response is still insufficient:
Clipline makes a no-redirect, authenticated `Range: bytes=0-0` request with five-second connect and
15-second total deadlines and requires at least one returned media byte. Local cleanup runs only
after that probe. It deletes the MP4 first, never touches sidecars if primary deletion fails, and
returns/persists primary or sidecar cleanup errors rather than silently discarding them.

Plan commit `876a778`; implementation commit `5323174`. The focused cloud suite passes with 32
tests covering processing/ready/failed outcomes, bounded media success/empty/missing responses,
reconcilable state, and primary-first cleanup failures. CI-mode `cargo test --workspace` and both
fresh-cache app clippy and workspace clippy pass with warnings denied. Computer Use verified the
rebuilt native app opens with all nine local clips and the Local/Cloud library controls intact; no
real upload was attempted because that would transmit user media.

## Checkpoint (2026-07-18): full-session writer backpressure

The combined audit's M-10 is fixed. Full-session output no longer receives deep-cloned GOPs through
an unbounded channel. Sealed segments are immutable `Arc<Segment>` values shared with the memory
replay ring; disk replay serializes the same value by reference. The writer channel holds at most
eight messages and reserves at most 128 MiB of exact video-plus-audio payload, including the
segment currently blocked in the writer. Capture uses `try_send`, so a slow or stalled output can
never block the capture loop.

If either queue limit is reached, Clipline stops accepting only full-session segments, continues
replay capture, finalizes the segments already accepted when Stop arrives, and returns a clear
full-session error to the app. Failed sends release their byte reservation. Writer-thread spawn
failure now propagates from `start_full_session` instead of panicking.

Plan commit `350db09`; implementation commit `5c3b810`. Focused tests cover exact byte reservation,
shared allocation identity, an over-budget segment, and a deliberately stalled writer filling a
one-slot queue while all replay GOPs continue buffering. CI-mode `cargo test --workspace` and
fresh-cache changed-crate plus workspace clippy pass with warnings denied. The live primary-monitor
WGC smoke timed out twice waiting for a desktop frame in this automation session; the other live
WGC/DXGI/MFT/WASAPI device tests passed on the first non-CI workspace run. Computer Use verified
the rebuilt native app opens with the nine-item library, hotkey rail, and 6.8 MB idle RAM; this VM
still cannot start a recording because no video encoder can be opened.

## Checkpoint (2026-07-18): recorder control and hotkey readiness

The combined audit's H-04 and M-19 are fixed. Runtime state now records the user's desired
recording state independently from the currently installed service sender. Game-detection restarts
reserve a monotonically increasing generation, spawn outside the runtime mutex, and install only
when both desired state and generation still match. Stop advances the generation even during the
sender-less restart gap, so it cannot be undone by a late replacement. A manual Start or newer
game/settings restart supersedes older work, and every rejected service receives an immediate
non-announcing Stop. Option errors still preserve an installed working recorder while invalidating
an older replacement when no sender is installed.

The low-level keyboard hook now creates its Windows message queue, calls `SetWindowsHookExW`, and
reports the real thread id or installation error before global hook state is published. The hook
waits for installer acknowledgement, unhooks if startup is abandoned, and has stored thread
identity for partial-install teardown. Mouse-hook or singleton-publication failure also tears down
the ready keyboard hook. Later settings updates now fail explicitly if the singleton is absent
instead of silently accepting a nonfunctional fallback.

Plan commit `d3b2183`; implementation commit `820c68f`. Focused coverage passes with 52 runtime
state tests and 12 hotkey tests, including deterministic Stop/Start/newer-restart races plus hook
success, failure, disconnect, and timeout. CI-mode `cargo test --workspace` passes and fresh-cache
workspace clippy passes with warnings denied. Computer Use verified the native hook starts without
an error, the live UI shows `Alt+F10`, and saving unchanged settings reports `saved`, exercising the
new hook-required update path against the installed singleton.

## Checkpoint (2026-07-18): destructive storage ownership boundary

The combined codebase audit's H-02 is fixed. Storage status, quota GC, and abandoned-recording
recovery no longer adopt every MP4 merely because it is in the configured media directory or one
of its direct children. A `<clip>.clipline.json` metadata document is now the per-file ownership
proof for newly authored replays and full sessions. Clipline creates it atomically before writing,
keeps it with recoverable recordings, carries it through collision recovery, skips stale marker
names during reservation, and removes it when a save fails or a session is deliberately discarded.

Quota and recovery ignore ambiguous unmarked MP4 and `.mp4.recording` files, including files in
custom-folder child directories. Existing finalized clips with Clipline marker or osu! enrichment
sidecars remain conservatively recognized for legacy compatibility; poster caches alone are not
ownership proof. Recording recovery requires the explicit ownership document, handles mixed-case
`.MP4.RECORDING` suffixes, and moves the document when a recovered filename needs a collision
suffix. The library continues to display unmarked MP4s for compatibility, but background storage
maintenance cannot delete them.

This also closes combined finding L-04: recovery detects and removes the `.recording` suffix with
the same case-insensitive comparison while preserving the original MP4 stem. The dedicated
`recovery_handles_mixed_case_recording_suffixes` fixture proves `Session.MP4.RECORDING` recovers as
`Session.MP4` rather than aborting the pass.

Plan commit `7dfc10a`; implementation commit `234f6af`. The focused storage suite passes with 23
tests, focused service coverage passes with 37 tests, CI-mode `cargo test --workspace` passes, and
fresh-cache workspace clippy passes with warnings denied. Computer Use opened the rebuilt app and
confirmed the existing nine-clip library and quota status render normally. A new replay could not
be recorded on this VM because no video encoder can be opened; marker creation and unrelated-file
preservation are covered through controlled filesystem tests.

## Checkpoint (2026-07-18): MP4 untrusted-input hardening

The first `CODEBASE_AUDIT.md` remediation batch fixes H1, M19, and M20 in `clipline-mp4`.
Malformed extended-size boxes now stop the tolerant walker through checked offset arithmetic,
including forged parent ranges and trim-side box-end conversion. Sample-table entry counts are
validated against their containing boxes before allocation; per-track metadata is capped at four
million samples (more than 18 hours at 60 FPS); and compressed `stts` durations expand only to the
already-validated `stsz` count.

Fragment construction is now fallible when sample sizes, payload totals, sample counts, or signed
`trun` data offsets cannot be represented. In-memory fragments use the same 8/16-byte `mdat`
header selection as streaming writers, large-header offsets are included in `trun`, and ordinary
box construction rejects sizes that would previously truncate through `as u32`. The in-memory
builder also writes directly into the final allocation instead of creating a second `mdat` payload
copy.

Plan commit `5d2fdf6`; implementation commit `14d1f90`. The focused MP4 suite passes with 100
unit/integration tests, CI-mode `cargo test --workspace` passes, fresh-cache MP4 clippy and full
workspace clippy pass with warnings denied, formatting and diff checks pass. No multi-gigabyte
fixture is required: boundary tests use forged metadata and synthetic sample-size records.

Computer Use acceptance opened the known three-audio-track `clip_1784329112.mp4`, confirmed video
playback advanced past ten seconds with the expected `2/3 selected` audio state, exported the
default keyframe-aligned range, and reopened the resulting 33.4-second / 2,591,953-byte trim. The
trim exposed all three audio tracks and playback advanced past ten seconds. The acceptance artifact
is `2026-07-17 15-52/clip_1784329112_trim_001797_035204.mp4`. A fresh Save Replay could not be
exercised in this VM: the running app reports that no video encoder can be opened, and neither a
system nor local packaged FFmpeg binary is present to activate the software H.264 fallback.

## Checkpoint (2026-07-18): elevated-game Save Replay hotkeys

An Arknights: Endfield report said Save Replay worked only after tabbing out. The reporter's UAC
prompt identifies the boundary: Endfield runs elevated while Clipline normally runs at medium
integrity, so Windows UIPI prevents Clipline's low-level keyboard hook from observing input aimed
at the focused game. Running Clipline as administrator was confirmed as the user workaround.

Clipline remains `asInvoker` by default. Game-detection events now query the detected process token
through safe Win32 wrappers and flag the blocked state only when the game is elevated above
Clipline. The frontend shows one in-app explanation per game PID and offers an explicit Restart as
Administrator action, warning that the rolling buffer resets. Acceptance launches the same
executable through the `runas` verb with the current PID; the elevated child waits for the normal
instance to exit before starting Tauri, avoiding overlapping recorders and the single-instance
race. Clipline exits only after Windows successfully creates the replacement, so a denied or
cancelled UAC request leaves it running normally. Future launches remain non-elevated.

Focused elevation/Win32/UI tests, CI-mode `cargo test --workspace`, fresh-cache workspace clippy
with warnings denied, formatting, and diff checks pass. Computer Use could not attach because its
native pipe returned OS error 2. A live UAC attempt timed out without approval and verified the
normal PID remained alive with no replacement; accepting UAC and visually confirming the elevated
replacement/dialog remain the final native checks.

PR #87 review hardened the handoff further: only a confirmed-gone parent may skip the wait,
handoff failures abort before Tauri starts, protected-process token query failures warn
conservatively, and the frontend retries queued warnings while closing stale ones. Later
passes keep the elevation dialog open after UAC cancellation, block dismiss/Escape while the
restart is in flight, restore the warned PID if the dialog closed during that wait, reconcile
the dialog after in-flight clears (so a game that exited during UAC cannot leave a stale
modal), and re-enable controls when restart returns false.

The final PR review now binds both elevation handoff and frontend warning suppression to a Windows
process instance (PID plus kernel creation timestamp), rather than a reusable PID alone. An
elevated replacement verifies that identity on its owned parent handle before waiting, and the UI
keys its once-per-process warning cache with the same identity. PR #87 merged as `1bb1090`; Nightly
0.1.36 is the first updater build containing the elevated-game hotkey recovery.

## Checkpoint (2026-07-18): Nightly 0.1.35

Nightly 0.1.35 contains PR #86. It ships the Proxmox/Windows VM software H.264 fallback,
active-encoder status, safer Discord/output-audio defaults, long-session capture-cadence fixes,
and mixed-output selection preservation. The previous public nightly was 0.1.34, so the app and
Tauri versions were bumped for updater delivery. The standalone installer also advances its
pinned Microsoft WebView2 Fixed Version Runtime patch from 150.0.4078.48 to 150.0.4078.83.

## Checkpoint (2026-07-18): long-session burst timestamp fix

A 0.1.34 user report described long VOD playback occasionally jumping to 00:00 after an
arbitrary seek. The supplied `session_1783827199.markers.json` is internally consistent: 91
ordered, unique, in-range markers over 2022.944 seconds with a constant recording offset. The
matching 2,103,075,867-byte MP4 downloaded with SHA-256
`4A1DB0A25A8435443F7238D9985090D764407694C5BA52EA361F2412D2F68BAA`. FFprobe accepts its H.264
video and two Opus tracks, every video packet timestamp is strictly increasing, all sampled seeks
from 60 through 2000 seconds land on the expected preceding keyframe, the maximum keyframe gap is
0.65 seconds, and a full 33:43 video/audio decode completes without codec errors. Markers,
keyframes, sample indexes, and bitstream corruption are therefore ruled out for this artifact.

The artifact did expose a reproducible recorder defect. It contains 1,265 consecutive video-frame
gaps below one millisecond, all exactly 0.1 ms; several cluster around the reported 15-minute area.
`CadencedCapture` emitted a scheduled duplicate when WGC timed out, then accepted a real frame
whose presentation timestamp still belonged to that filled cadence slot and forced it to
`last_pts + 0.0001`. This produced extra near-zero-duration samples and an average frame rate above
the configured 60 FPS. `CadencedCapture` now retains an early real frame as the latest texture and
yields a bounded timeout to the service loop before reading again, so save/stop handling stays
responsive while a stale WGC queue drains. Its retry budget preserves the existing wall-clock
deadline; successful real frames advance the same wall anchor by their PTS delta; and overloaded
conversion/encoding skips missed cadence slots instead of letting video PTS drift behind wall time
and audio. Six focused tests cover idle duplication, stale-frame yielding/data reuse, delayed WGC
delivery, and time spent in the encoder between capture calls.

This timing defect is a plausible WebView2 stressor, especially because the supplied file has a
1.48 MB tail `moov` and Clipline plays it through Tauri's range-based asset protocol, but the exact
seek-to-zero chain is not yet proven. Computer Use could not attach in the final reproduction pass
because this thread's native pipe returned OS error 2. Do not claim the player reset itself was
visually reproduced or fully fixed until a fresh native session exercises this artifact. The
validated file is hard-linked without an extra 2 GB copy at
`C:\Users\dain9\Videos\Clipline\Imported seek repro 1783827199\session_1783827199.mp4`.

The bounded PR #86 review stopped cleanly after pass 3. It also fixed the split-audio helper that
normalized the new `output + microphone` default into microphone-only output. Review-fix commits:
`56f2339 docs: plan PR 86 review fixes`, `97dbd79 fix(capture): yield while dropping stale frames`,
`42a2744 fix(player): preserve mixed output selection`, and
`12201c3 fix(capture): keep cadence aligned with wall clock`.

Focused tests, the CI-mode full workspace suite, fresh-cache workspace clippy with warnings denied,
formatting, and diff checks pass. The unchanged live
`captures_monotonic_gpu_frames_from_primary_monitor` device test timed out twice waiting for a
desktop update after the app was stopped; other live WGC tests passed. Treat that as an environment
signal to rerun with an actively changing desktop, not as validation of this cadence patch.

## Checkpoint (2026-07-17): Discord audio safety-track default

A user report that Discord stopped recording after a recent update was reproduced as a playback-
selection regression, not loss from the mixed speaker capture. With Experimental app audio tracks
enabled, Clipline enumerates process audio sessions only when the recorder starts. A native
`ffplay` process started afterward was absent from the per-process marker metadata but remained
audible in the mixed Output Audio safety track. In the final five seconds of
`C:\Users\dain9\Videos\Clipline\2026-07-17 15-52\clip_1784329112.mp4`, mixed output measured
-33.1 dB mean/-30.0 dB peak while the stale startup Media Player track measured -91.0 dB
mean/-84.3 dB peak.

Nightly 0.1.34 commit `dc7250e` changed clip opening to prepare every default audio track. The
existing split-track default excluded mixed Output Audio whenever any startup process track
existed, so the review player could switch from audible stream zero to stale process tracks and
make late-start Discord appear unrecorded. Split-track clips now default to mixed Output Audio plus
non-process inputs such as the microphone; selecting individual app tracks remains available and
mutually exclusive with mixed output. Runtime process discovery is still a separate, larger
enhancement. The focused `player_core` regression test covers the safe default.

## Checkpoint (2026-07-17): Proxmox VM software H.264 fallback

Clipline can now record in Windows VMs that support WGC but expose neither a D3D11 video
processor nor a hardware video encoder. The existing hardware paths are unchanged and preferred.
The fallback reads WGC BGRA textures through a staging resource, performs deterministic limited-
range Rec.709 BGRA-to-NV12 crop/scale conversion in neutral Rust, and pipes NV12 to the LGPL
FFmpeg `h264_mf` encoder with `-hw_encoding 0`. `h264_mf` must pass a real one-frame probe before
the candidate is offered.

Verified live in this Proxmox Windows 11 VM on Microsoft Basic Display Adapter: Clipline ran at
1280×800/60 FPS, spawned `h264_mf` in forced software mode, saved three replays, populated their
Library thumbnails, and produced a validated 60.6-second H.264 MP4 with limited-range BT.709
metadata. The FFmpeg mux round-trip integration test exercised both SVT-AV1 and Media Foundation
software H.264. No Proxmox PCI passthrough, IOMMU, or virtual-GPU flag is required for this path;
its tradeoff is CPU usage, so reducing FPS/resolution is the first tuning lever.

Native Computer Use acceptance then saved and reviewed a fresh fourth replay at
`C:\Users\dain9\Videos\Clipline\2026-07-17 15-08\clip_1784326197.mp4`. Play/pause, click-seek,
playhead dragging, and post-scrub playback all worked without visible corruption. The 60.36-second
file is H.264 1280×800 limited-range BT.709 with two stereo Opus tracks and decodes cleanly; both
audio inputs were silent in this run. A five-second steady-state sample measured Clipline plus its
FFmpeg child at roughly 120% of one logical core (about 15% of this eight-logical-processor VM),
confirming the expected CPU cost rather than iGPU acceleration. Acceptance also caught that the
frontend discarded the backend's active encoder label, so Automatic mode could not identify the
selected fallback. The UI now retains the status event's encoder and exposes
`Stop recording · Software · H.264` on the active recorder control.

Implementation commits on `build-run-app` begin at
`5f354ab docs(capture): plan software VM encoder fallback`. The local ignored
`apps/clipline-app/ffmpeg/` directory contains the 2026-07-17 BtbN LGPL shared build used for live
acceptance. Keep distributing FFmpeg as a separate process and never add GPL encoders.

## Checkpoint (2026-07-16): repository simplification pass

Nightly 0.1.34 contains PRs #83 through #85. It ships the transactional reliability and long-MP4
fixes, resilient seeking with fast audio-only sidecar switching, continuous quiet-audio capture,
the dead-code/public-surface reduction, and the accepted arrow/J/L review-navigation remap. The
previous public nightly was 0.1.33, so the app and Tauri versions were bumped for updater delivery.

The primary checkout is on `main` at the same commit as `origin/main`. A conservative cleanup
removed unused preview readback, mixed-loopback audio, PCM mixing, MP4/buffer wrappers, generated
browser snapshots, and completed scratch notes. Internal buffer, event, League, and storage crates
now expose one root API instead of duplicate public module paths. No runtime behavior, dependency,
configuration, or persistence changes are intended.

Review-player navigation now uses left/right arrows for five-second seeks (Shift for one second)
and J/L for frame-aligned ten-frame steps. Automated contracts and manual acceptance pass. Local
capture data under `.gsi-spike/` remains untracked and must not be cleaned. `cargo test
--workspace`, fresh-cache workspace clippy with warnings denied, formatting, and diff validation
all pass on Windows.

## Checkpoint (2026-07-15): fast audio sidecar switching implemented

The whole-video review preview path has been replaced end to end. The original `<video>` now stays
loaded while selected audio tracks are extracted to reusable audio-only MP4 sidecars and played by
synchronized hidden audio elements. Manual acceptance on the reproduced 31-minute clip remains.

### Workspace and preservation constraints

- Active branch: `sidecar-sync-policy`
- Active worktree:
  `C:\Users\dain\.paseo\worktrees\1qv1k36q\friendly-sheep`
- The original checkout at `C:\Users\dain\Projects\clipline` has user-owned uncommitted changes in
  `apps/clipline-app/tests/player_core.rs`, `apps/clipline-app/tests/ui_contract.rs`,
  `apps/clipline-app/ui/index.html`, `apps/clipline-app/ui/player-core.js`, and
  `apps/clipline-app/ui/review-player.js`, plus untracked `.gsi-spike/`. Never overwrite, stage, or
  clean those changes. Continue only in the isolated worktree.

### User-visible state

- The rapid right-arrow/forward-seek reset was fixed by making the logical seek target
  authoritative across media events and source generations. The user manually confirmed this item
  appears fixed.
- Quiet WASAPI endpoints now synthesize timeline-continuous silence with one 20 ms capture-latency
  allowance. The real hardware sync test passed with approximately 11.7 ms maximum skew.
- Explicit audio switches are serialized/coalesced and no longer assign a preview to `video.src`.
  The directly playable first track stays on the original video; other non-empty selections use
  synchronized sidecars, and an empty selection is muted output.
- Every audible sidecar path is protected from the total 2 GiB LRU cache while active. The only
  known orchestration limitation is that an already-running FFmpeg extraction is not cancelled;
  its stale result may populate cache but cannot activate.

### Diagnosis and approved architecture

The reproduced 31:31, 1.88 GiB clip exposed the root cause: each uncached selection read the whole
source, rebuilt another full MP4 containing copied video, wrote roughly 1.9 GiB, and reloaded the
video element. That creates about 3.8 GiB of disk traffic, several GiB of live buffers, and cache
thrashing.

Live measurements with the packaged FFmpeg:

- one audio track copied to audio-only MP4: 1.87 s, 23.9 MB;
- two tracks copied in one FFmpeg process: 0.50 s, 47.7 MB total;
- two tracks decoded/mixed/re-encoded to one audio-only MP4: 15.0 s.

The user approved an approximately 0.5-to-2-second first uncached switch and near-instant cached
switches. The approved design keeps the original `<video>` loaded, caches one stream-copied
audio-only MP4 per embedded track, and plays selected tracks through synchronized hidden audio
elements. The video remains the authoritative clock with a 100 ms drift threshold.

Read these documents completely before continuing:

- `docs/superpowers/specs/2026-07-15-audio-sidecar-switching-design.md`
- `docs/superpowers/plans/2026-07-15-audio-sidecar-switching.md`

### Completed sidecar work

The design and all six implementation tasks are committed or ready in the current cleanup commit:

- `f4a08779` — `docs(player): design fast audio sidecar switching`
- `a53a83c8` — `docs(player): plan fast audio sidecar switching`
- `e1a947bf` — `feat(mp4): expose media track counts`
- `311dc21a` — `feat(player): prepare cached audio sidecars`
- `516aef21` — `fix(player): harden audio sidecar preparation`
- `7050c29b` — `fix(player): close audio sidecar publication boundaries`
- `4dd47e1` — `feat(player): define audio sidecar transport policy`
- `5a99b13` — `feat(player): add synchronized audio sidecar transport`
- `585553d` — `fix(player): switch audio without reloading video`

Completed behavior:

- `prepare_clip_audio_sidecars` accepts `{ path, audioTrackIds, protectedPreviewPaths }` and
  returns ordered `{ audioTrackId, path }` records.
- Per-track `audio-track-sidecar-v1` cache keys reuse a track across selection combinations.
- One FFmpeg process extracts all missing selected streams with explicit `0:a:N`, `-vn`, and
  `-c:a copy`; the new path never copies or maps video.
- Existing requested hits are protected before pruning, validated, touched, and reused.
- Outputs validate as exactly zero video tracks and one audio track before publication.
- Publication ownership remains armed across the blocking task and Tauri asset-scope calls. A
  failure removes only invocation-owned finals; collision winners and prior hits are never owned.
- Legacy clips without audio marker metadata use a bounded `Read + Seek` MP4 metadata reader that
  skips `mdat`. Finalized `moov` allocation is capped at 64 MiB, with malformed size/header/EOF
  coverage.
- The video is the authoritative clock. Sidecars force-align on activation and seek, mirror
  play/pause/rate, and correct ordinary drift only above 100 ms using one 500 ms timer while
  playing.
- User mute and volume are logical state independent of transport-level video muting. Original
  video audio is not silenced until every current-generation sidecar is playable and its play
  promise succeeds.
- Opening a clip selects every default review track, including the microphone, while the first
  embedded track starts immediately; the complete selection activates atomically after its
  sidecars are ready without reloading the video.
- Direct source playback follows audio stream index zero even when marker rows are reordered, and
  each source assignment keeps one removable error listener for its full lifetime.
- Validated sidecar cache hits retain their ordered result without a redundant second validation;
  validation/publication owns temporary-file cleanup on every failure path.
- Clip open/close, suspend, source release, replacement, and rename invalidate callbacks, stop the
  drift timer, pause sidecars, remove their sources, call `load()`, and release Windows file
  handles.
- The legacy `preview_clip_audio_tracks` command, whole-source reader/remuxer, combination cache
  key, preview-only writer, and FFmpeg video-copy/`amix` path have been removed. Old
  `audio-preview-*.mp4` files remain ordinary LRU eviction candidates.

Verification reported green at this checkpoint:

- `cargo test -p clipline-mp4 media_track_counts -- --nocapture`
- `cargo test -p clipline-mp4`
- `cargo test -p clipline-app audio_sidecar -- --nocapture`
- `cargo test -p clipline-app audio_preview_cache -- --nocapture`
- `cargo test -p clipline-app --test player_core audio_preview_queue -- --nocapture`
- `cargo test -p clipline-app --test player_core logical_seek -- --nocapture`
- `cargo test -p clipline-app --test ui_contract legacy_audio_preview -- --nocapture`
- `cargo test --workspace` — 775 listed tests, all green
- `cargo clean -p clipline-app`
- `cargo clippy -p clipline-app --all-targets -- -D warnings`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt --check`

### Exact next steps

1. Launch this worktree with
   `CLIPLINE_FFMPEG=C:\Users\dain\AppData\Local\Clipline\ffmpeg\ffmpeg.exe`.
2. On the reproduced 31-minute clip, verify uncached one/multi-track switches take approximately
   0.5–2 seconds, cached switches are nearly immediate, and rapid selection changes apply only the
   newest selection.
3. While sidecars are active, verify seeking/right-arrow spam never reloads or resets the video;
   also exercise play, pause, scrub, playback rate, mute, direct fallback, empty selection, clip
   changes, and rename.
4. Force an extraction/load failure and verify the previously audible selection continues, then
   restart once to confirm total preview-cache pruning still respects active protected files.

## What this project is

Clipline is an open-source, lightweight, ad-free game recorder for Windows (see `ddoc.md`):
ShadowPlay-style replay buffer, **no DLL injection ever** (anti-cheat safety is the core
architectural bet), automatic timeline event markers via the League of Legends Live Client
Data API, Hybrid MP4 output, Rust core + Tauri UI.

## Current state (2026-07-09): a working tray recorder with a first-party review player

Thirty-five milestones executed (plans in `docs/superpowers/plans/*.md` — plan docs are kept there, all
completed task-by-task with strict TDD; read any of them to see the conventions in action):

1. **WGC capture** — monitor + window, GPU-side frames, QPC-anchored pts
2. **MFT H.264 encoder** — async hardware MFT (AMF on the dev box), GPU NV12 path, AVCC out
3. **WASAPI loopback audio** — system audio → real Opus (`shiguredo_opus`), silence gap fill
4. **A/V sync hardening** — stamp-derived MP4 timeline, one shared clock, `avsync` validator
   (real-engine test: −8.3 ms total drift)
5. **Tauri shell** — `apps/clipline-app`: tray app, replay-buffer service thread, **Alt+F10**
   global hotkey → `Videos\Clipline\clip_<unix>.mp4`, smart no-overlap saves
6. **Event markers** — League poller (1 Hz, quiet retry outside matches) → `MarkerLog` →
   `<clip>.markers.json` sidecars re-based to clip time; mock-server verified end-to-end
7. **Library + marker timeline** — clip list (duration/size/age/marker badge), in-app playback
   (H.264+Opus `<video>` works in WebView2 via the asset protocol), marker ticks with
   click-to-seek, path-validated delete
8. **Disk quota + auto-GC** — neutral storage manager scans `Videos\Clipline`, counts MP4s plus
   marker sidecars, enforces a default 10 GiB oldest-first quota after saves, protects the
   just-saved clip, and surfaces usage/quota/clip count in the UI. `--disk-quota-gb 0` disables
   GC; any positive number sets the GiB cap.
9. **Settings** — `%APPDATA%\Clipline\settings.json` persists capture target, buffer/replay
   seconds, bitrate, FPS, disk quota, and save hotkey. The in-app Settings panel validates and
   saves changes, restarts the recorder service with new recording options, rebinds the global
   hotkey, updates the tray label, and keeps the storage row on the active quota.
10. **Trim/export editor** — the player overlay now has in/out controls and exports a sibling MP4
    without touching the source clip. `clipline-mp4::trim_keyframe_aligned` parses Clipline's
    finalized H.264/Opus MP4 tables, aligns start backward and end forward to video keyframes,
    stream-copies selected samples into a fresh finalized MP4, and crops marker sidecars.
11. **Review player v2** — clips open in a two-pane review player with no native video chrome:
    dimmed-outside-trim timeline with draggable in/out edges and amber marker ticks,
    transport row (marker prev/next, ±5 s, play/pause, tenths readout, rate, volume),
    keyboard-first review (`Space`/`K`, `←→`/`J`/`L` 5 s / `Shift` 1 s, `,`/`.` 0.1 s,
    `I`/`O` trim at playhead, `M`/`Shift+M` markers, `Esc`), and an export row that shows the
    kept range live. There are deliberately no trim number inputs — position the playhead,
    then mark. The UI is split into `index.html` / `styles.css` / `player-core.js` (pure,
    DOM-free logic) / `main.js` (wiring); `player-core.js` is unit-tested **from Rust** via
    `boa_engine` (`tests/player_core.rs`), and `tests/ui_contract.rs` guards the DOM contract.
    (An earlier externally-authored workspace, `bd1c84f`, was reverted and redone this way.)
12. **Review player polish** (Outplayed comparison-driven) — typed marker chips
    (kill ✕ / spree ★ / objective ◆ / structure ▣ / info •, kind-colored, unknown kinds
    degrade to info), labeled time ruler with nice-step gradations, transport reordered to
    sit under the stage, human-first library labels ("Jun 11 · 10:25 PM" + marker digest,
    filename in the tooltip), focus mode (`F` hides the sidebar), live scrubbing
    (seek-throttled via the `seeked` event so WebView2 keeps painting; trim-handle drags
    ride the playhead and pause/resume playback).
13. **Session folders** — saves land in `Videos\Clipline\<session>\`: one folder per recorder
    run (label `YYYY-MM-DD HH-MM`, local time, fixed at service start) plus a dedicated
    `… league` folder per detected LoL match (the poller now sends
    `MatchStarted`/`MatchEnded`; `GameEnd` events also end the match session). Folders are
    created lazily at save time; exports stay siblings so they inherit the folder; the
    library groups by session with legacy root clips under "Earlier"; `reveal_clip` opens
    Explorer with the clip selected; storage status/GC scan root + one level and delete
    emptied session folders. assetProtocol needed a second glob
    (`**/Videos/Clipline/**/*.mp4`) for subfolder playback.
14. **Stage overlay transport** — the transport row moved onto the video as a translucent
    hover bar (gradient scrim, hand-authored inline SVG icons, no icon font/npm): pins while
    paused, fades after 2 s idle while playing (`PlayerCore.overlayVisible`, evaluated from
    the playhead rAF loop — no timers), hides on pointer-leave, wakes on pointer/keyboard.
    Volume is an icon + hover-expanding slider. `ui_contract` now requires `<svg` inside
    every transport button.
15. **Sidebar rail + header cleanup** — the hamburger collapses the sidebar to a 52 px
    icon rail (status dot, save, gear; `F` toggles; rail state survives clip open/close)
    instead of the old full-collapse focus mode. Header is two icon buttons (folder reveal,
    trash delete); Copy Path is gone (the path in `#pmeta` is selectable text) and Close is
    gone (click the active library row again, or `Esc`). Export is a scissors-"Clip" primary
    button. Delete confirmation is an in-app `<dialog>` (Delete left / Cancel right, user
    preference) — `ui_contract` bans native `confirm()`/`alert()` and the removed header ids
    outright.
16. **Settings page** — settings left the sidebar fold for a full-bleed tabbed page in the
    main pane (Capture / Recording / Storage / Hotkeys; name + description rows; one Save
    footer). Reached via the sidebar Settings row or the rail gear; exits via ✕, `Esc`
    (priority over closing the clip; player shortcuts are inert behind the page), or opening
    a clip. The open clip pauses and survives the round-trip. Field ids and the
    validate/save/restart wiring are unchanged from milestone 9.
17. **Display-region capture** — Capture settings now include `display_region`, persisted as
    `{ display_id, x, y, width, height }`. The settings page renders a virtual desktop map with
    draggable/resizable region box, numeric pixel fields, and right-click menu actions
    (Align: left/right/top/bottom/center; Set to Display: enumerated Win32 displays). The
    recorder enumerates monitors with `EnumDisplayMonitors`, captures the selected monitor with
    WGC, derives a safe in-frame crop from virtual-desktop coordinates, and crops GPU-side in the
    D3D11 video processor before MFT encode. This is intentionally a single-display region crop;
    stitched regions spanning multiple monitors are still out of scope. Verified locally with
    `CARGO_TARGET_DIR=target\codex-test cargo test --workspace`,
    `CARGO_TARGET_DIR=target\codex-test cargo clippy --workspace --all-targets -- -D warnings`,
    and a static Chrome screenshot harness for the settings UI.
18. **Hotkey recorder** — Settings > Hotkeys no longer asks users to type shortcut strings.
    `#set-hotkey` is a read-only recorder: focus/click it, press F1-F11/F13-F24 with optional
    Ctrl/Alt/Shift, and the UI writes the normalized shortcut (`F10`, `Ctrl+Alt+F9`, etc.)
    through the same validate/save/rebind path. Modifier-only input prompts for an F-key,
    `Escape` cancels, F12 is rejected as debugger-reserved on Windows, and invalid keys stay in
    recorder mode with inline status. The pure formatter lives in `ui/player-core.js` and is
    covered by `tests/player_core.rs`; `ui_contract` requires the read-only recorder/status
    markup.
19. **Settings UX cleanup** — the display-region map no longer has its own internal scrollbars;
    it computes a static height from the virtual desktop shape and lets the settings page own any
    scrolling. Recording settings now read in user terms: replay history, save length, video
    quality, and smoothness. Recording controls use sliders with human summaries and visible scale
    markers, and quality snaps to Compact/Balanced/Sharp/Maximum preset stops. The underlying ids
    and persisted settings values are unchanged.
20. **Recording controls cleanup** — the user-facing Replay history control is gone; Clipline keeps
    the internal rolling buffer at two minutes and exposes only Save length, capped at 5 sec-2 min
    with 30 sec / 1 min / 2 min presets. Smoothness now has 30/60/90/120 FPS stops. The Settings
    page no longer has the top-right X button, so the bottom-left Settings control is the close
    affordance. The sidebar now shows a clickable capture status (`Capturing Desktop`, window, or
    display region), storage/quota/clip count, and Save Replay; it no longer shows buffered seconds,
    MB, or GOP diagnostics. The new `set_recording` Tauri command stops/starts the recorder from
    that status control. Stopping intentionally clears the rolling replay buffer, and internal
    settings restarts do not emit a stale stopped status.
21. **Audio device controls + mic capture** — Capture settings now include Audio output and
    Microphone controls. Users can keep system/output audio on or off, select default or explicit
    render/capture endpoints, set output and mic gain from 0-200%, enable microphone capture, and
    choose Mono mic handling with a checkbox. When output and mic are both enabled, the recorder
    mixes them into one normal Opus track so the in-app player and regular video players hear both;
    single-source output-only or mic-only captures still use the normal WASAPI Opus source. The mic
    path accepts common WASAPI float/PCM formats and resamples to Opus' 48 kHz timeline. Capture
    also has a live Test mic monitor: the button toggles to Stop testing, plays the selected mic
    back through Web Audio, and shows a live level meter. Output audio remains enabled by default;
    mic capture is opt-in for privacy.
22. **Media folder settings + Explorer fixes** — Storage settings now has a Media folder path.
    The recorder service, library listing, delete/export validation, storage quota/status, and
    folder-opening commands all use the same persisted root instead of independently assuming
    `Videos\Clipline`. The default is still `Videos\Clipline`; changing it restarts the recorder
    and creates the folder before saving settings. The review header's folder button opens the
    containing folder directly, and the Storage tab uses a native Choose Folder picker to set the
    media root.
23. **FFmpeg encoder matrix** (ddoc §4) — recording is no longer MFT-H.264-only. `clipline-mp4`
    is codec-aware (`VideoTrackConfig::{h264,hevc,av1}` → `avc1`/avcC, `hvc1`/hvcC, `av01`/av1C;
    HEVC PTL parsed from the SPS, AV1 profile/level/tier from the sequence-header OBU; trim is
    codec-agnostic). `clipline-capture` gained neutral `hevc`/`av1` bitstream modules and an
    FFmpeg **subprocess** encoder: `FfmpegVideoEncoder` spawns a bundled `ffmpeg.exe`, pipes NV12
    in (GPU frames are converted BGRA→NV12 on the GPU via the existing `VideoConverter` then read
    back through a staging texture), and a reader thread frames the elementary stream into access
    units (`framing.rs`: Annex B by VCL NAL for H.264/HEVC, IVF temporal units for AV1). The probe
    (`ffmpeg.rs`) locates `ffmpeg.exe` and reports `{h264,hevc,av1}_{nvenc,amf,qsv}` + `libsvtav1`
    by parsing `-encoders` and test-encoding each hardware encoder. `probe.rs` now carries an
    `EncoderApi` axis (Mft vs Ffmpeg) and `rank_encoders(caps, decodable, preference)` — backend
    merit, MFT preferred over FFmpeg for the same combo, Auto restricted to player-decodable codecs
    and now H.264-first for playback compatibility. The recorder walks the ranked candidates until one opens (behind
    `Box<dyn Encoder>`), reports the active encoder in the sidebar status, and warns on explicit
    fallback. Settings has one Encoder dropdown listing the machine's real backend×codec combos;
    the UI probes WebView2 (`canPlayType`) for HEVC/AV1, marks undecodable codecs "(limited
    playback)", and reports the decodable set so Automatic never records an unplayable clip.
    **The subprocess approach was chosen over linking libavcodec** (deliberate revision of the
    plan): zero unsafe FFI, version-robust, cleanest LGPL boundary. Decisions, sharp edges, and
    the not-yet-done parts are below.
24. **Custom game detection foundation** — Settings now has a Games tab with built-in profile
    placeholders and a custom game workflow: Add Custom Game scans visible top-level windows,
    records process path/exe/title metadata, and saves enabled custom rules under
    `%APPDATA%\Clipline\settings.json`. A background detector enumerates visible windows every
    2 seconds and, when a saved custom game is running, restarts the recorder onto that concrete
    WGC window handle; when it disappears, Clipline falls back to the normal Capture target. This
    remains no-injection/no-memory-read: only Win32 window/process metadata plus WGC window capture.
    The sidebar/status surface reports `Capturing Game: <name>` while a custom game override is
    active. Windowed game capture uses the HWND client rect, so title bars/borders are excluded
    from saved replays. The WGC frame pool now respects per-frame `ContentSize` and recreates on
    capture-item resize; the NV12 converter rebuilds its video processor when the client texture
    size changes, scaling resized windows into the fixed MP4 track instead of artifacting or
    clipping to the first size. The review player also renders clips inside an aspect-locked
    `#stage-frame`, so WebView's `<video>` element cannot add top/bottom letterboxing when the
    available stage area is slightly off from the clip's aspect ratio. Custom game detection now
    owns per-window capture selection in the UI, so the old manual "Window title" capture target
    was removed from Settings > Capture while backend/CLI compatibility remains. The fallback
    Capture target dropdown lists available displays first and keeps the editable `SET REGION`
    option at the bottom; display selections persist as full-monitor display-region captures.
    - Settings > Games now has a manual Detect Games workflow beside Add Custom Game. Both flows
      open modal dialogs instead of inline panels; Detect Games scans Steam manifests only, shows
      unchecked candidates, dedupes existing custom games, and appends selected rows as normal
      Custom games using the existing save-to-apply flow. Saved custom games render in a compact
      scrollable list with each row's recording-mode toggle on the right.
25. **Full-session game recording** — Each saved custom game persists its own recording-mode
    preference (`replays_only` default, `full_session` selectable). Games set to full session start
    a shared-encoder Hybrid MP4 sink when the detected window becomes the active capture target,
    while continuing to feed the replay ring so Save Replay still works. The session sink now runs
    on a dedicated writer thread: sealed GOPs are cloned once and queued after the replay ring push,
    so disk stalls or secondary file-write failures cannot abort primary replay capture. The MP4
    writer is initialized lazily on the first queued GOP so codec parameter sets discovered from
    the first HEVC/AV1/H.264 packets land in the final `hvcC`/`av1C`/`avcC`, and segment muxing uses
    borrowed sample slices instead of per-sample `Vec` copies. Full sessions finalize
    `session_<unix>.mp4` in the run's session folder on game disappearance, target switch, service
    stop, capture end, or clean shutdown; if encoder finish fails, the temp session is discarded
    with a warning rather than emitted as a complete recording. The on-disk file uses a temporary
    `.mp4.recording` suffix until finalized so the Library cannot open an in-progress fragmented
    recording. Non-empty orphaned `.mp4.recording` files are recovered to `.mp4` once per app
    process on launch, empty ones are removed, active recording bytes count toward storage usage,
    and GC avoids deleting the rest of the library when a protected full session alone exceeds
    quota. Recovery deliberately does not run on every recorder restart; custom-game target
    switches can overlap old/new service threads, and a repeated sweep can rename the active temp
    file before the old thread finalizes it. Finalization also treats "temp missing but final file
    already exists" as success so any session caught by that race is still emitted into the
    Library. Full sessions use the same marker sidecar, quota cleanup, library refresh, and
    saved-event path as manual replays, and the library labels them as "Full session".
26. **Game plugins + League auto-recording** — Game-specific behavior now sits behind a built-in
    plugin registry (`apps/clipline-app/src/game_plugins.rs`) instead of hardcoded UI/settings
    branches. Settings persist generic plugin state under `games.plugins.<plugin_id>` with
    enabled + recording-mode fields, and the frontend renders Settings > Games from the backend
    `list_game_plugins` catalog. The first plugin is `league_of_legends`: it matches only the
    real in-game `League of Legends.exe` top-level window, not `LeagueClientUx.exe` or Riot
    launcher windows, so champion select/client activity does not start full-session recording.
    League is enabled by default and defaults to `full_session`; when the match window appears,
    Clipline switches capture to that window and starts a shared-encoder session recording, then
    finalizes it when the window disappears. Custom games remain as the generic fallback layer
    beneath plugins.
27. **Plugin event sources + in-game hotkey fallback** — Built-in game plugins can now expose an
    optional event-source spawner in addition to their window matcher. The recorder carries the
    active built-in plugin id in `ServiceOptions` and asks that plugin for markers; League owns the
    Live Client Data API poller, while custom games record with no marker source unless a future
    plugin adds one. Save Replay now also has a Windows `WH_KEYBOARD_LL` fallback hook, kept in sync
    with the Settings > Hotkeys shortcut, so games that suppress Tauri/Win32 registered global
    shortcuts still reach the recorder. All save triggers share a short debounce to avoid double
    saves when both hotkey paths fire.
28. **Explicit SDR color metadata** — Desktop/game captures are no longer left to driver,
     encoder, or player color-range inference. The WGC BGRA path is treated as full-range RGB
     Rec.709 and the D3D11 video processor converts to limited-range NV12 Rec.709; MFT and FFmpeg
     encoders receive matching color attrs/flags, and `clipline-mp4` writes `colr`/`nclx` sample
     entry metadata. A real smoke recording now probes as `color_range=tv`,
     `color_space=bt709`, `color_transfer=bt709`, and `color_primaries=bt709`.
29. **Startup on Windows login** — Settings now has a General tab with an "Open on startup"
     toggle. When enabled, Clipline registers itself in the Windows Run registry key
     (`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`) via `tauri-plugin-autostart`,
     passing `--autostart` so launches from the registry start minimized to the tray instead
     of opening the main window.
30. **Audio track splitting v1** — Output audio is split by current Windows render-session
     process using process-loopback capture, so game/Discord/Spotify/browser audio can land in
     separate Opus tracks. Clipline keeps a mixed Output Audio track first as a playback/export
     safety track, then app/process tracks, then microphone when enabled; when the experimental
     "app audio tracks" Capture setting is off, only the mixed Output Audio track is recorded.
     That setting defaults off.
      Electron-style apps that emit
      multiple child-process audio sessions are grouped by same-executable root process before
      process-loopback capture, so Discord should appear once instead of as renderer/audio-service
      duplicates. Launcher parent sessions (for example Steam) are dropped when a child process
      also has its own audio session, because process-loopback captures the target process tree and
      otherwise records the game twice with a small offset. Clipline also filters its own
      `clipline-app` process out of split app-audio tracks so replay-save notification sounds are
      not selected as a separate default source. Mid-stream buffered replays advertise a one-frame
      (20 ms) Opus pre-skip so cold decoders discard the first-frame startup artifact instead of
      playing it as a short burst at clip start. The
      process-loopback activation path uses an agile completion handler and an owned `VT_BLOB`
      activation payload; the dev machine reproduced heap corruption when that blob pointed at
      stack memory. Saved
     replays and full-session recordings write `audio_tracks` metadata into marker sidecars, the
     review deck exposes an expandable track checklist, and the upload dialog lets users choose
     which tracks to include. Single-track and muted selections are stream-copy remuxed through
     `clipline-mp4::remux_with_selected_audio_tracks`; multi-track share/upload selections are
     exported through the native Opus mixer so external players receive one audio stream. New audio
     sessions that appear after recording starts are not discovered dynamically yet.
31. **Mouse hotkeys + selected-track uploads** — Settings > Hotkeys accepts middle mouse,
     Mouse4, and Mouse5 when combined with Ctrl/Alt/Shift, in addition to F1-F11/F13-F24.
     Keyboard F-key shortcuts still use Tauri's OS global-shortcut registration plus the
     low-level fallback; mouse-button shortcuts are hook-only through an on-demand Windows
     low-level mouse hook. The rail now shows the active Save Replay hotkey below RAM. Single-track
     and muted cloud uploads use lightweight selected-track remuxing; multi-track cloud/share
     exports now use native Opus mixing so external players hear one normal audio stream.
32. **Library multi-select + bulk actions** — the local gallery supports selecting multiple
     clips and acting on them in bulk. A filter-toolbar `#gallery-select-toggle` button labeled
     `Select multiple` flips the whole grid into selectable mode where clicking a tile toggles
     selection instead of opening it; the normal per-card trash affordance is hidden while this
     mode is active so selection and one-off deletion do not compete. A `#gallery-bulk-bar` appears
     inside the filter toolbar with `Select all` / `Clear` / `Delete` / `Cancel` and a live count.
     `Delete` runs the new
     `delete_clips` Tauri command (one round trip, validates every path up front via
     `validate_clip_path`, deletes mp4 + `markers.json` sidecar + cached poster, returns a
     `DeletedClipsReport { deleted, failed }` so partial success is surfaced rather than swallowed).
     `Esc` clears the selection then exits select mode; `Ctrl+A`
     (in select mode) selects all visible. Selection is keyed on `clip.path` (survives
     filter/sort/group/re-render), is **local-only** — the Cloud tab hides the Select toggle and
     clears/exits selection on entry. Backend work is
     split into a testable `delete_clips_impl` (no `tauri::State`) so the partial-success +
     sidecar/poster cleanup behavior is covered by a unit test; `tests/ui_contract.rs` gains
     `gallery_supports_multi_select_bulk_actions`.
33. **First-party supported game presentation** — the installable plugin direction was replaced
     with built-in supported game profiles. League remains the first profile, with declarative
     presentation data for marker styling, gallery cards, a playback-synced, pull-tab-collapsible
     right-side event rail, and a bottom metadata strip. Event ingestion stays core-owned behind
     the built-in `league_live_client` capability; game integration updates ship with normal
     Clipline releases instead of external plugin zips or Settings-driven package installs.
     `EventKind`, `GameId`, `is_review_event()`, and `is_timeline_marker()` remain core-owned:
     profiles style the closed marker vocabulary but cannot add event kinds or change persistence
     policy. The review player
     threads presentation into pure `player-core.js` marker helpers and `main.js` renders
     profile-driven gallery summaries, marker styling, the event rail, and metadata. League's Live
     Client summary keeps optional participant/team roster data so the event rail can render
     kill-feed-style actor/victim champion portraits from Data Dragon, actor/objective rows for
     turret/dragon/baron events, blue/red row treatment, restored first-party timeline marker
     icons, and a separate event-rail icon map using first-party kill/death silhouettes plus
     CommunityDragon objective icons. Gallery cards use the profile `gallery.card` policy for title
     and icon behavior; League keeps full-session cards titled by K/D/A plus CS/min when fresh
     sidecars have creep-score data, while replacing the generic League logo with the local
     champion portrait. League's metadata strip resolves the local champion portrait through the
     Riot Data Dragon champion-square provider, renders summoner spells beside the portrait, shows
     value-first K/D/A plus ratio, and appends a compact item-build row from fresh Live Client
     sidecar data; older clips fall back to whatever summary fields they already have. Settings >
     Games remains backend-driven for supported game rows but no longer exposes check/update/
     reinstall/reset package actions.
34. **osu! play-block foundation** — the desktop side now has a first-party `osu!` supported-game
     profile (`osu!.exe`, full-session focused), an Account/Plays settings dialog that plainly
     collects a user-provided osu! OAuth app Client ID, Client Secret, and user id/username, plus
     a question-mark setup guide that opens a local walkthrough. The client secret is stored in
     Windows Credential Manager, not `settings.json`; the desktop uses the client-credentials
     grant directly and sends `x-api-version: 20220705` when fetching recent scores so failed plays
     have real ids and `ended_at`. `ClipMarkers.plays` sidecars support interval play blocks.
     Full-session saves from osu!-tagged sessions write durable
     `.osu-enrichment.json` pending records; startup/library refresh retries are idempotent, and
     storage/delete cleanup tracks those pending sidecars with marker/poster files. The pure
     mapper accepts normalized osu! scores, keeps fails, requires `ended_at`, prefers
     `started_at`, derives estimated starts from beatmap length with DT/HT adjustment, clamps
     derived failed starts against the previous play, dedupes score ids, applies UTC/skew
     overlap, and reports when the 500-score fetch ceiling may leave plays missing.
     The review UI can render osu! intervals as timeline blocks, a right-side "Set plays" rail,
     hover/focus details, seek/highlight behavior, and osu! gallery summaries. A real spike
     confirmed client credentials with `public` scope can fetch Dain's recent osu!standard scores,
     including submitted failed plays, so there is no Clipline Cloud broker dependency.
35. **Reliability and playback hardening** — Full-session finalization now retains non-empty
    `.mp4.recording` files for startup recovery when writer finalization or the final rename fails.
    Settings changes plan recorder options without taking the active command sender and commit the
    restart only after persistence/tray/hook work succeeds. Cloud-library loads are account-scoped
    and generation-guarded, forced refreshes supersede in-flight requests, renamed clips carry and
    rewrite pending osu! enrichment, and all deletion/quota paths include markers, clip metadata,
    pending enrichment, and posters. Finalized MP4s switch `mvhd`/`tkhd`/`mdhd` to version 1 above
    `u32::MAX`, with `u128` duration rescaling. Multi-audio preview swaps resolve the playhead after
    generation completes, consume the latest queued seek, and rapid relative seeks accumulate.

Verification (2026-07-09): formatting, workspace Clippy, and fresh-cache Clippy for the three
changed crates passed. The first non-CI workspace test run had one transient real-clock device-test
failure; its exact rerun, a subsequent complete non-CI workspace rerun, and the CI-mode full
workspace test run passed. App launch and manual playback verification are deferred until this
branch is integrated.

> Claude handoff: the library clip-icon/labeling thread was paused at the user's request. If you
> resume it, the user wants no monitor/desktop icon and no tiny checkbox/corner badge. The desired
> shape is a full-size clapper icon on the left, only for videos that are actually user-created
> clips, likely after finishing a clearer labeling model.

Recent fixes (2026-07-06):
- Nightly 0.1.33 contains the profile-category review filter work from PR #80 and the library
  launch-surface fixes from PR #81. The previous public nightly metadata was 0.1.32, so the app
  and Tauri package versions were bumped to 0.1.33 for updater delivery. Review timeline and match
  event filters now key off profile-declared marker categories instead of League-only kind names;
  `InhibKilled` appears under Structures and `FirstBlood` is no longer double-counted as a kill.
  Library badges keep SESSION/TRIM/CLOUD text optically centered, fresh installs bundle the LGPL
  FFmpeg resource used for gallery posters, and the launch-time update dialog is draggable while
  leaving its action buttons clickable.

Recent fixes (2026-07-04):
- Settings > Recording now has an Advanced toggle for exact recording overrides. When enabled,
  `advanced_recording` supplies custom max output bounds (aspect-preserving, never stretching),
  exact bitrate Mbps, and exact FPS to the recorder while the normal preset controls remain the
  default path. Video-quality summaries now include the preset bitrate (for example,
  `Sharp quality - more detail. 24 Mbps.`), and the disk replay estimate follows the exact
  bitrate when Advanced is enabled.
  Verified with focused settings/UI/player-core tests, `cargo test --workspace`, and
  `cargo clean -p clipline-app; cargo clippy --workspace --all-targets -- -D warnings`.

Recent fixes (2026-07-03):
- Settings now opens as a popup over the current Library/Review view instead of replacing the
  main pane. Unsaved edits change `Close` to `Discard Changes`; the first discard attempt
  shakes the popup, shows `Careful--your changes aren't saved.` in red beside `Discard Changes`,
  and makes `Save Settings` glow. A second discard button press closes and restores the last
  saved settings. Backdrop clicks close only when the form is clean; with unsaved edits they
  warn/shake/glow repeatedly until the user presses `Save Settings` or `Discard Changes`.
  Rows with unsaved changes now get a blue glow, and tabs containing changed rows show a pip;
  indicators clear when edits are saved, discarded, or reverted.
  Verified with `cargo test --workspace` and
  `cargo clean -p clipline-app; cargo clippy --workspace --all-targets -- -D warnings`.

Recent fixes (2026-07-02):
- Nightly 0.1.28 contains the custom game detection workflow and review follow-ups from PRs
  #72 and #73. The previous public nightly metadata was 0.1.27, so the app and Tauri package
  versions were bumped to 0.1.28 for updater delivery. Custom games can now be added from a
  Steam-based detected-games modal with checkbox selection, the custom games list is compact and
  scroll-contained, and visible non-game windows are no longer added as standalone detection
  results.
- Nightly 0.1.27 contains the osu! play-block polish and CI review fixes from PR #71. The
  previous public nightly metadata was 0.1.26, so the app and Tauri package versions were bumped
  to 0.1.27 for updater delivery. osu! timeline bars now handle overlapping intervals cleanly,
  incomplete plays use their purple treatment, exported play clips keep the song title without
  intrusive marker metadata, account settings preserve saved API credentials, and the cross-platform
  UI contract tests declare their serde_json dependency explicitly.
- Nightly 0.1.26 contains the gallery hover/enrichment refresh-loop hotfix from PR #70. The
  previous public nightly metadata was 0.1.25, so the app and Tauri package versions were bumped
  to 0.1.26 for updater delivery. Library card hover no longer flickers from repeated refreshes,
  and osu! pending enrichment only emits a UI refresh when visible play metadata changed.
- Nightly 0.1.25 contains the osu! play-block release from PR #69. osu! is now a real
  supported-game profile with stable/cutting-edge detection, title-change play timing, optional
  direct API enrichment, Set plays metadata cards, interval blocks, and right-click play export
  without marker metadata in the exported clip.
- The osu! profile now detects the stable idle title `osu!`, stable map titles such as
  `osu!  - ginkiha - EOS [Lycoris]`, and cutting-edge build titles such as
  `osu!cuttingedge b20260624`, while explicitly rejecting updater-like titles from `osu!.exe`.
  osu!-tagged full sessions shorter than ten seconds are discarded as boot/update transients.
  Its empty Set plays rail copy now points users to the osu! API settings credentials instead of
  implying enrichment completed with no submitted plays.
- Added the osu! play-block implementation plan at
  `docs/superpowers/plans/2026-06-30-osu-play-blocks.md`, plus the desktop schema/UI/enrichment
  scaffolding and reusable API spike script. The shipped auth path is direct desktop
  client-credentials with a local setup guide, not the earlier Cloud broker/proxy.
- Supported-game rows now persist a nested `review` settings block. Each supported row has a
  Settings button that opens a grouped tabbed dialog: General controls Replays only vs Full session
  and whether to show League match details, Match events filters the right-side rail by your events,
  team fights, and map events, and Timeline markers filters your markers vs map markers. Fresh
  recordings keep broader review events (`is_review_event`) in marker sidecars so those filters can
  show ally/enemy events; older recordings only contain whatever marker data existed when they were
  captured.
- League local-player assists now normalize as `ChampionAssist`, survive the timeline-marker
  filter, and render with the new assist icon/category; the refreshed sword kill icon is used by
  both timeline markers and the right-side match events rail.
- Nightly 0.1.24 is a hotfix for the review timeline action row and League minion turret-kill
  presentation. The previous public nightly metadata was 0.1.23, so the app and Tauri package
  versions were bumped to 0.1.24 for updater eligibility.
- The review player's snip action now lives as an icon-only control at the far right of the
  below-timeline metadata row instead of taking its own row or appearing inside the timeline.
- League event rail rows using `actor_event` layout now map non-participant minion actor ids
  like `Minion_T200...` to CommunityDragon minion portraits, so minion turret kills render as a
  compact icon row instead of exposing the raw minion id text.
- Legacy/no-sidecar multi-audio MP4s now infer their audio track list from the finalized MP4 tables
  and use the same native preview mixer/upload selection paths as fresh split-audio clips. The
  inferred metadata is playback-only, so clip duration still comes only from real sidecar markers.
- The review player no longer has a session-wide "audio preview unavailable" latch; failed preview
  generation falls back for that attempt without blocking later multi-track preview retries.

Recent fixes (2026-06-29):
- Nightly 0.1.22 is a hotfix for local review playback of output+mic clips. The previous
  public nightly metadata was 0.1.21, so the app and Tauri package versions were bumped to
  0.1.22 for updater eligibility.
- Local review audio previews now use the native `clipline-mp4` Opus mixer before falling back
  to FFmpeg, so Clipline-authored multi-track output+mic recordings play back as one audible
  stream in WebView2 even when external FFmpeg is missing.
- Nightly 0.1.21 contains the simple timeline editor from PR #66. The previous public nightly
  metadata was 0.1.20, so the app and Tauri package versions were bumped to 0.1.21 for updater
  eligibility.
- The review deck now defaults to a simple Outplayed-style timeline: whole-clip browse view first,
  a scissors button enters local trim mode around the playhead, and `Create Clip` uses the existing
  keyframe-aligned export path. The previous navigator/zoom/snap editor is still available via the
  General setting `Legacy timeline editor` (`legacy_timeline_editor` in settings JSON). The simple
  timeline now keeps the scissors control above the track, layers event markers on the timeline band,
  and attaches a denser time ruler below it.
- Nightly 0.1.20 contains the League replay playback performance fix from PR #65. The previous
  public nightly metadata was 0.1.19, so the app and Tauri package versions were bumped to
  0.1.20 for updater eligibility.
- League review playback now avoids recomputing the event rail, marker metadata, and overlay
  digest work on every video time tick. The player throttles overlay detail refreshes while the
  video is running and keeps the event rail's active-row updates on a lighter schedule, reducing
  the frame stutter observed after the richer League presentation shipped.
- Nightly 0.1.19 contains the first-party supported game profile pivot and League presentation
  upgrade from PR #62. The previous public nightly metadata was 0.1.18, so the app and Tauri
  package versions were bumped to 0.1.19 for updater eligibility.
- League clips now have built-in supported-game presentation data for marker styling, gallery
  cards, a playback-synced right-side event rail, and richer bottom metadata driven by the
  first-party profile. The old standalone installable plugin package path is intentionally not
  part of this release; game presentation updates now ship through normal Clipline nightlies.

Recent fixes (2026-06-27):
- Nightly 0.1.18 contains the default multitrack playback fix and gallery thumbnail hardening
  from PR #63. The previous public nightly metadata was 0.1.17, so the app and Tauri package
  versions were bumped to 0.1.18 for updater eligibility.
- Review playback now mixes default output+mic multi-track captures for WebView2/share targets
  that only play the first audio stream, but falls back to source playback without a persistent
  error when ffmpeg audio mixing is unavailable. Local poster failures are cached for the app
  session and stay on the gradient placeholder instead of using per-card video elements that can
  keep Windows file handles open.
- Nightly 0.1.17 contains the local clip-library multi-select/bulk-delete workflow and the
  replay-audio fixes from PR #61. The previous public nightly metadata was 0.1.16, so the
  app and Tauri package versions were bumped to 0.1.17 for updater eligibility.
- Replay muxing now avoids carrying non-zero Opus pre-skip into freshly cut replay clips and
  selects the intended WASAPI loopback process tree, fixing the start-of-clip audio burst and
  the Steam-track tunnel/phase artifact observed in newly recorded clips.
- Nightly 0.1.16 contains the memory/duplicate-instance guard, close-to-tray playback suspension,
  settings-draft preservation, replay Opus pre-skip fix, and rustfmt drift cleanup. The previous
  public nightly metadata was 0.1.15, so the app and Tauri package versions were bumped to 0.1.16
  for updater eligibility.
- Close-to-tray now emits a frontend playback-suspend event before hiding the WebView, so review
  audio/video and pending preview work stop instead of continuing behind the tray session.
- Settings now keep an explicit unsaved draft while the settings page is open. Tab switches and
  async device/display/encoder refreshes read from that draft, so saving at the end preserves edits
  made across multiple settings tabs.
- Replay clips cut from the middle of an Opus stream now write audio tracks with zero `dOps`
  pre-skip, avoiding the tiny start-of-clip audio drop that only belongs at the original stream
  beginning.
- Runtime memory/duplicate-instance guard: Task Manager reports of many Clipline rows were partly
  WebView2 child process labeling, but duplicate top-level `clipline-app.exe` processes were also
  allowed. The Tauri shell now registers `tauri-plugin-single-instance` before autostart so normal
  duplicate launches reveal the existing window and `--autostart` duplicates stay quiet. The
  recorder also byte-budgets the pending GOP before ring insertion (capped at 64 MiB), drops
  leading non-keyframes until the first keyframe, and errors clearly if an encoder stops producing
  keyframes instead of accumulating packets indefinitely. Verified with focused `ui_contract` and
  `pipeline` regressions, `cargo test --workspace`, fresh-cache clippy, and a debug runtime
  duplicate-launch probe.

Recent fixes (2026-06-25):
- Nightly 0.1.15 contains the Cloud library tab/profile rail work, relaxed hotkey rules, and the
  PR #53 review follow-ups below. The previous public nightly metadata was 0.1.14, so the app and
  Tauri package versions were bumped to 0.1.15 for updater eligibility.
- Connected cloud identity in the rail: when `settings.cloud` has a stored credential target/user,
  the bottom-left rail shows a compact profile button above Settings. It refreshes the account from
  `/api/v1/auth/me`, prefers `display_name` over username, fetches `GET /api/v1/me/avatar` with the
  stored bearer token via the native `cloud_user_avatar` command, and opens the user's cloud profile
  at `/u/{username}`. A small in-process ETag cache handles avatar 304 responses; 404 or fetch errors
  keep an initials fallback and disconnect hides the rail identity entirely.
- Library cloud source tab: the Library header now has Local/Cloud tabs. The desktop pins
  `clipline-cloud-api` to Clipline Cloud `v1.2.18` and uses `CloudClient::list_clips` to fetch the
  authoritative server library (`GET /api/v1/clips`, paged newest-first). Cloud cards still merge
  local upload records by `client_clip_id` so they can show whether a local copy is present, and
  fall back to persisted `settings.cloud.uploads` rows while the server list is unavailable. Rows
  with a matching local file now render as normal playable local clip cards. Cloud-only rows fetch
  authenticated thumbnails and media through native commands, cache them under
  `%APPDATA%\Clipline\cloud-cache`, and play the cached MP4 through the existing review player;
  `Open page` still opens the owned cloud page externally. PR #53 review follow-up: disconnected
  Cloud tab rendering no longer recurses, fallback upload rows keep `remote_clip_id` so cloud-only
  history can play in-app, thumbnails lazy-load through the shared poster observer, transient list
  errors stay visible without latching the tab permanently loaded, cloud-cache files are
  account-namespaced/pruned/bounded by size, and cloud-only review playback hides local-file
  actions while rerouting the header cloud button to copy the cloud link. The Cloud list command
  still fetches every page before first render; convert it to first-page render + lazy pagination if
  large cloud libraries become sluggish.
- Recorder startup display recovery: startup primary-monitor capture now resolves the primary
  display through the same `EnumDisplayMonitors` path used by Settings instead of
  `MonitorFromPoint(0,0)`, which could bind to a ghost/wrong monitor on some Windows layouts.
  Display-region capture also recovers from a missing saved display id or stale region geometry by
  warning the user and falling back to the full current primary display when the saved display is
  gone. If the saved display still exists but the region only partially fits, the crop clamps to
  the visible part instead of silently recording the whole display. Full-display region selections
  are recognized by display size and re-based to the current monitor origin so Windows virtual
  desktop coordinate churn across reboot does not require opening Settings and saving again.
- Share/export audio compatibility follow-up: the 0.1.12/0.1.14 remux-only upload behavior could
  hand cloud/Discord a multi-audio-track MP4 where only the first stream was played, producing
  silent uploads or missing mic audio. Cloud uploads now replace two-or-more selected audio tracks
  with one native mixed Opus track while stream-copying video, and clipboard copy uses the same
  selected-audio compatibility export under `%APPDATA%\Clipline\share-exports` before setting
  CF_HDROP. This is native `shiguredo_opus` decode/mix/re-encode inside `clipline-mp4`; users do not
  need FFmpeg installed for multi-track upload/share audio. The mixer preserves the source Opus
  pre-skip, averages overlapping tracks to avoid hard clipping, and streams slot-by-slot instead of
  buffering all decoded PCM. Share-preview/export cache writes use unique sibling temp files and
  prune orphaned `.mp4.tmp` files.
- WebView2 compatibility follow-up for the Windows 10 tester whose Edge/WebView2 registry state
  was missing: Nightly 0.1.14 switches the normal NSIS installer from Tauri's WebView2
  `offlineInstaller` to the small embedded Evergreen bootstrapper, while keeping
  `minimumWebview2Version = 120.0.2210.55`. Fresh installs and updates can now fetch/repair the
  runtime from Microsoft during install instead of carrying the large offline runtime in every
  Clipline installer. This is not an air-gapped compatibility claim: offline or Microsoft-blocked
  machines may still need the WebView2 Runtime installed manually.
- The app now has a native already-broken-install recovery signal. `main.js` invokes
  `frontend_ready` once JavaScript boots and IPC works; the Rust shell logs `frontend_ready
  received`. When `open_main_window` reveals the UI, it also probes `is_visible()` explicitly and
  classifies Tauri's typed `Runtime(FailedToReceiveMessage)` as a dead WebView2 signal. If that
  getter probe fails or the frontend-ready watchdog expires, Clipline shows one native `rfd`
  repair dialog per process from a worker thread. This matters because a dead WebView2 frontend
  cannot trigger the in-app updater; already-broken users need reinstall/manual WebView2 repair.

Recent fixes (2026-06-24):
- Windows 10 follow-up from Nate's 0.1.12 logs: the recovery-window build also produced
  immediate `failed to receive message from webview` state calls, while Windows 11 works
  normally. Treat this as WebView2/runtime creation trouble, not a hidden-window bug. Nightly
  0.1.13 removed the `main-recovery-*` churn, kept revealing the existing `main` handle when
  getters fail, logged Microsoft Edge WebView2 runtime registry `pv` values at startup, and set
  `minimumWebview2Version = 120.0.2210.55` so Windows 10 installs repair/update stale runtimes.
- Published Nightly 0.1.12 with the mouse-hotkey, selected-audio-track upload remux, release
  diagnostics, and dead-window recovery work from PR #51.
- Added release-build diagnostics for the tray/open-window path. Clipline now appends
  single-line entries to `%APPDATA%\Clipline\clipline.log`, including startup args,
  tray menu/icon events, close-to-tray handling, window event summaries, WebView labels,
  and before/after window state around `Open Clipline` (`visible`, `minimized`, `focused`,
  position, and size). The log rotates to `clipline.old.log` after 1 MiB.
- Tray close now hides the app window instead of destroying it. A destroyed Tauri window can leave
  a `main` webview label behind whose state calls fail with `failed to receive message from
  webview`; 0.1.12 briefly tried recovery labels, but Windows 10 logs showed new recovery
  webviews failing the same way, so the recovery path was removed again in favor of WebView2
  runtime diagnostics and installer enforcement.
- Save Replay hotkeys now support middle mouse, Mouse4, and Mouse5 when combined with
  Ctrl/Alt/Shift. Mouse hotkeys skip the OS global-shortcut registration path and are handled by
  an on-demand low-level mouse hook; switching between keyboard and mouse hotkeys
  unregisters/registers only the keyboard shortcut side. The rail shows the current save hotkey
  below RAM.
- Cloud upload briefly remuxed explicit selected audio tracks instead of mixing multiple selections
  through FFmpeg, avoiding the old "ffmpeg is not available for audio track mixing" failure but
  exposing first-audio-stream playback problems in external players. The 2026-06-25 native-mix
  follow-up above supersedes that behavior for multi-track selections.

Recent fixes (2026-06-22):
- Tray "Open Clipline" now uses the same reveal path as a normal foreground launch:
  show the hidden WebView window, restore it if it is minimized, then focus it. This fixes
  tray-only sessions where recording/capture kept running but the interface did not come
  back from the tray.
- Startup now treats OS global-hotkey registration as best-effort. If `Alt+F10`
  is already owned by another recorder/overlay, Clipline continues launching,
  keeps the tray/menu path available, and still installs the low-level in-game
  hotkey fallback instead of aborting during Tauri setup with no visible UI.
  Settings rebinds now skip unregistering stale, never-registered shortcuts and
  retry an unchanged missing shortcut without blocking unrelated settings saves.
- Opening a cloud-uploaded clip now rechecks its remote Clipline Cloud state in the background:
  visibility/link changes refresh the local upload record, finalized remote deletions clear the
  local cloud badge/link, and temporary 404s for `uploaded_processing` records keep the local
  processing record.
- Cloud uploads briefly mixed multiple selected audio tracks into one Opus stream, this was
  replaced on 2026-06-24 with selected-track remuxing for every explicit upload selection, and the
  2026-06-25 native-mix follow-up restored single-stream multi-track uploads without requiring
  FFmpeg.
- Debug/Cargo builds now keep Windows startup registration disabled and clear stale debug Run-key
  entries on launch/status checks; installed release builds keep normal startup behavior.

Recent fixes (2026-06-21):
- Bug-scan app reliability slice: recorder restarts now build replacement service options before
  dropping the old command sender, settings saves go through a synced sibling temp file and atomic
  replace, cloud ready-poll timeouts preserve an `uploaded_processing` record with its remote link
  instead of stuck `processing`, cloud auto-delete removes poster sidecars, disk replay cache/media
  overlap checks are case-insensitive on Windows, split-output clips apply the default selected-track
  preview on open, and opening a new clip clears the previous playhead RAF/pending seek.
- Split-audio review/upload semantics: when per-process output tracks exist, the "Output Audio"
  checklist row is a master toggle for those process output tracks, not an extra mixed track to
  include alongside them. The mixed Output Audio stream remains in the file as a fallback/safety
  track, but selected previews omit it while process tracks are active to avoid doubled audio.
  Exact all-physical-track preview requests return the original clip path instead of generating a
  mixed preview.

Recent fixes (2026-06-19):
- Library rows now keep full title/context text visible, then fade the right edge on hover/focus
  to reveal a borderless trash affordance. League clip metadata intentionally wraps onto its own
  line, and the death skull marker is mask-scaled to visually match kill markers.
- Deleting a clip updates the local library cache and storage summary instead of doing a full app
  refresh, avoiding the visible lag spike after delete.
- Custom game detection treats saved process path/exe identity as authoritative. Legacy
  title-only custom rules ignore browser processes, so YouTube tabs with a game title do not start
  game recording or trigger save-on-return behavior.
- The native WebView/Chromium context menu is suppressed. Library rows own a small right-click
  menu with Upload, Rename, Rename file, and Delete actions.
- Library rows and the review header rename clips by saving a metadata-backed display title without
  moving the MP4. The secondary Rename file action still validates Windows-safe MP4 names, moves
  marker/poster/metadata sidecars with the source file, preserves the clip kind, and keeps matching
  cloud upload records pointed at the new local path.
- Upload buttons now open an in-app dialog for title, description, and visibility before upload.
  Nonblank descriptions are trimmed and sent on `POST /api/v1/uploads`; blank descriptions are
  omitted. New cloud uploads no longer include deprecated marker payloads in the create request.
- Rename/export no longer run heavy filesystem/media work on the UI path. Rename first tries to
  move the file without unloading the player, only releasing the video handle on a Windows lock
  retry; export returns enough metadata for the UI to insert the new clip row locally instead of
  rescanning every clip.
- Startup avoids the old library/probe burst: `list_clips` and `storage_status` run on the blocking
  pool, library listing uses marker-sidecar duration instead of reading whole MP4s, and display /
  audio / encoder probes are deferred until after first paint or Settings opens. Plain clips without
  a marker sidecar may have unknown duration in the library list; the UI now omits that value rather
  than showing `?`.
- Audio splitting v1 records output audio as per-process MP4 audio tracks when Windows process
  loopback is available, keeps microphone as a separate track, carries track labels in sidecars,
  shows review/upload checklists, and remuxes only selected tracks for cloud upload. It falls back
  to a mixed Output Audio track if no process tracks start or the experimental Capture setting is
  turned off; the setting defaults off. Duplicate child sessions from apps like Discord are grouped
  by same-executable root process before capture. The Windows process-loopback path was fixed after reproducing
  `STATUS_HEAP_CORRUPTION`: keep the activation payload as an owned
  `VT_BLOB`, keep it alive until `GetActivateResult`, and make the completion handler agile.
- Review audio-track checkboxes now affect playback as well as upload: WebView-native track toggles
  are used when available, otherwise Clipline stream-copies a temporary selected-audio preview MP4
  under `%APPDATA%\Clipline\audio-previews` and reloads the player at the same timestamp.
- PR review follow-ups: opening a multi-track clip no longer eagerly creates a full-length audio
  preview; preview generation starts only after the user changes track selection. Multi-track
  preview mixing now surfaces FFmpeg failures instead of falling through to an unmixed MP4, and
  the preview cache key was bumped to avoid reusing old fallback artifacts. If some process-loopback
  tracks start but others fail, Clipline appends the mixed Output Audio fallback so game/system
  audio is still preserved. Cloud upload records now supersede older records for the same clip
  path, so retrying with a different audio-track selection does not leave stale failed state in
  the library.
- Review playback now treats any source MP4 with more than one audio track as needing the selected
  audio preview/mix, even when every track is selected. This keeps default output+mic captures
  audible in WebView2 and common share targets that only play the first track; if ffmpeg-based
  mixing is unavailable, the app falls back to source playback without pinning a persistent error.
  Local gallery poster failures are cached for the app session and stay on the gradient placeholder
  instead of attaching per-card video elements that can hold Windows file locks.
- Review audio previews now try the native `clipline-mp4` Opus mixer before FFmpeg, so
  Clipline-authored output+mic clips get a one-stream local preview even when external FFmpeg is
  missing. The FFmpeg mixer remains a fallback for legacy/non-Opus files the native mixer cannot
  parse.

Run it: `cargo run -p clipline-app` (settings persist under `%APPDATA%\Clipline\settings.json`;
options still override startup behavior: `--window <title substring>` to capture one window
instead of the primary monitor, `--lol-url <url>` to point the marker poller at a mock, and
`--disk-quota-gb <n>` to override the saved quota for that launch). The media folder is now a
saved Storage setting; changing it affects future library scans, saves, exports, and quota checks.
Useful examples: `record_smoke -- --seconds 5 --window <w> --audio` (full pipeline + sync
report + ffprobe), `wgc_smoke` (capture only). Everything is verified live on this machine —
real clips with matching A/V durations, real marker sidecars, real in-app playback.

| Crate | What it does | Verified by |
|---|---|---|
| `clipline-events` | Event schema (ddoc §5), game-clock→recording anchor math, `MarkerLog`/`ClipMarkers` sidecars | unit tests |
| `clipline-lol` | League Live Client adapter: client, dedupe, normalization, `poll_once` | httpmock integration + `markers_e2e` |
| `clipline-buffer` | Replay ring of GOP segments (video + N audio tracks), byte eviction, `save_window` smart mode | unit tests |
| `clipline-storage` | Saved-clip inventory, sidecar-aware size accounting, oldest-first quota GC with protected fresh saves | unit tests |
| `clipline-mp4` | Hybrid MP4 muxer (frag→finalized in place), **codec-aware** (H.264/HEVC/AV1: avc1/hvc1/av01 + avcC/hvcC/av1C), Rec.709 limited `colr` metadata, multi-track + Opus, box walker, `movie_duration_s`, codec-agnostic keyframe-aligned stream-copy trim | ffprobe + unit tests |
| `clipline-capture` | Traits + mocks + `Recorder` (steppable, save-while-recording) + **all real Windows engines** under `src/windows/` (`wgc`, `mft`, `nv12`, `wasapi`, `mft_probe`, `d3d11`, `window`) + the **FFmpeg subprocess encoder** (`ffmpeg`, `ffmpeg_encoder`, `framing`) + explicit SDR Rec.709 limited-range conversion/encoder metadata + neutral `annexb`/`hevc`/`av1`/`opus`/`pcm`/`clock`/`avsync`/`probe`; WASAPI covers selectable mixed output loopback, per-process output loopback, mic capture, mic level testing, PCM decode, and resampling to 48 kHz; window helpers enumerate visible HWND/process metadata for custom game detection | mocks on CI; CI-skipped device + ffmpeg tests run real on the dev machine |
| `apps/clipline-app` | Tauri 2 shell: service thread, configurable hotkey, tray, status/library/settings plus the first-party review player; Settings > Games persists custom game rules and auto-switches capture to detected game windows | live e2e (screenshots in the session logs) + `player_core` (Boa) + `ui_contract` |

## Machine setup (already done on this machine; for a fresh clone elsewhere)

1. **Git identity** (repo-local, doesn't travel): `git config user.email "dain98@gmail.com"`,
   `git config user.name "Dain"` — commits are authored by the personal account.
2. **Remote/auth:** repo is `https://github.com/dain98/clipline.git` over **HTTPS** with gh as
   credential helper (`gh auth setup-git`, account `dain98`). Don't switch to SSH — the
   machine's agent key belongs to a different GitHub account.
3. **Rust** stable + clippy. `cargo test --workspace` must be green before starting.
4. **ffmpeg/ffprobe** (winget `Gyan.FFmpeg`) — the ffprobe e2e tests self-skip without it.
   On this machine the binaries live under
   `%LOCALAPPDATA%\Microsoft\WinGet\Packages\Gyan.FFmpeg_...\ffmpeg-8.1.1-full_build\bin`
   (fresh shells get them on PATH; long-lived shells may need the full path).

## Development conventions (unchanged since day one — keep them)

- **Plan-driven TDD.** Each milestone gets `docs/superpowers/plans/YYYY-MM-DD-<name>.md` with
  complete code and bite-sized steps; execute strictly failing-test-first. Plans are committed
  before execution; checkboxes stay unticked (repo convention).
- **Commits:** conventional style (`feat(capture): …`), one logical change, trailer
  `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` when Claude authors.
- **Quality gates per milestone:** workspace tests green, `cargo clippy --workspace
  --all-targets` zero warnings, push, **CI green on ubuntu + windows**, handoff updated.
- **Platform discipline:** neutral logic stays neutral (testable on both CI OSes); Windows
  code behind `#[cfg(windows)]`; trait changes happen neutral-side first with tests; all
  `unsafe` confined to `windows/` modules behind safe wrappers.

## Sharp edges (each of these cost real debugging time — read before touching)

**CI / testing**
- Device tests (WGC, MFT, WASAPI, real-clock sync) are **hard-skipped under `CI`**:
  windows-2025 runners report `IsSupported()==true` for WGC then access-violate inside the
  capture component; they have no hardware encoder or audio endpoint. Local runs exercise
  them for real — the dev machine (RX 6700 XT, 5120x1440 primary) is the test rig.
- CI clippy can fail on lints a **warm local cache hides** — `cargo clean -p <crate>` before
  trusting a local clippy pass on changed crates.
- `clipline-app` keeps ubuntu CI webkit-free by gating *all* Tauri deps under
  `[target.'cfg(windows)'.dependencies]` with a stub `main` elsewhere; `build.rs` gates
  `tauri_build::build()` on `CARGO_CFG_WINDOWS`.

**Media pipeline**
- `clipline-mp4` wants **4-byte length-prefixed NALs**; MFTs emit Annex B — `annexb.rs`
  converts (and strips AUD/SPS/PPS). B-frames must stay **disabled** (no ctts in the muxer).
- **Async audio previews replace the video source:** never restore a playhead captured before the
  preview await. Resolve and consume `pendingSeek` immediately before `video.src` changes, and base
  repeated relative seeks on the queued target rather than stale `video.currentTime`.
- **Long finalized MP4s need version-1 duration boxes:** `mvhd`, `tkhd`, and each `mdhd` must switch
  independently when its duration exceeds `u32::MAX`; use a `u128` intermediate when rescaling.
- MP4 sample tables keep encoded media contiguous, while per-track presentation gaps are explicit:
  fragments carry absolute `tfdt` values and finalized tracks use `elst` empty/media runs. The
  720 kHz movie clock exactly covers the 90 kHz video and 48 kHz Opus clocks. Video durations are
  re-derived from capture stamps and quantized by cumulative endpoints; each audio segment retains
  its first packet PTS. Audio before the first video packet remains engine-init lead-in and is
  dropped.
- WASAPI loopback requires a **48 kHz float mix format** (resampler is a follow-up); loopback
  goes quiet when nothing renders — that's why the gap fill exists.
- One D3D device and one `RelativeClock` must be shared across capture/encode/audio —
  the constructors force it (`WgcCapture::new_clock()`, `*_on(device, …, clock)`).
- H.264 hardware encoders cap near 4096 wide; the 5120-wide monitor scales to ≤2560
  (`even_dimensions` + scale in service/smokes).
- SDR color is explicit end-to-end: WGC BGRA is treated as full-range RGB Rec.709, the D3D11
  video processor outputs limited-range NV12 Rec.709, MFT/FFmpeg are given matching metadata,
  and MP4 sample entries write `colr`/`nclx`. If recordings look dark or oversaturated again,
  check this path before assuming a blue-light filter or player issue. HDR capture/display
  management remains separate future work.

**FFmpeg encoder tier (milestone 23)**
- It's a **subprocess**, never linked. `FfmpegVideoEncoder` spawns `ffmpeg.exe`; killing the
  recorder drops the child (Drop closes stdin + joins the reader). CI has no bundled ffmpeg, so
  `ffmpeg::probe()` returns empty and the live encoder test (`tests/ffmpeg_encode.rs`) self-skips;
  everything stays MFT-only there. The neutral bits (probe parsing, `framing.rs`, codec boxes)
  are fully unit-tested on both CI OSes.
- Ship the pinned **lgpl-shared** BtbN archive through `scripts/stage-ffmpeg-resource.ps1`; it has
  SVT-AV1 + GPU encoders but **no libx264/libx265**, so no software H.264/HEVC. The script verifies
  archive and per-file hashes, stages only the manifest allowlist into
  `apps/clipline-app/ffmpeg/`, and preserves license/provenance in the installer resource. The search
  order (`CLIPLINE_FFMPEG` override → bundled resource → exe dir → `%APPDATA%\Clipline\ffmpeg` →
  PATH) means the packaged LGPL build wins over any GPL PATH ffmpeg. Attribution:
  `THIRD-PARTY-NOTICES.md`.
- AMF **rejects tiny resolutions** (`Init() failed with error 5` at 128×72) — the probe
  test-encodes at 640×360. SVT-AV1 **errors on `-maxrate`/`-bufsize`** (exit -22): CBR capping is
  hardware-only; SVT-AV1 gets `-b:v` + `-preset 8` (VBR-ish; the ring evicts by bytes anyway).
- Access-unit framing recognizes first-slice and AUD boundaries so multi-slice H.264/HEVC pictures
  remain one sample; keyframes come from IDR/IRAP NALs. AV1 keyframe state comes from the encoded
  frame header rather than output position. Input/output timestamp cardinality is strict for every
  codec.
- `EncoderBackend::MfSoftware` uses `SoftwareMftH264Encoder`, which intentionally selects only the
  inbox Microsoft synchronous H.264 MFT. It has CPU NV12 input and no D3D manager; third-party
  synchronous transforms are not advertised under this backend. Its real integration test can
  skip on Windows Server images where the optional inbox encoder is absent, so keep a Windows
  client E2E in release acceptance.

**Tauri (v2)**
- The webview **silently no-ops** (no events, no invoke) without
  `capabilities/default.json` granting `core:default`.
- The assetProtocol scope **does not resolve `$VIDEO`** — use plain globs. With configurable
  media folders the scope is currently `**/*.mp4`; diagnose media errors via a `video.onerror`
  handler because error code 4 usually means the scope rejected the request, not a codec problem.
- H.264+Opus MP4 plays natively in WebView2 — no native decode path needed until AV1/HEVC.
- `tauri-build` requires `icons/icon.ico` (ours is ffmpeg-generated).

**Misc**
- League Live Client testing without a match: `--lol-url` + the httpmock pattern in
  `crates/clipline-lol/tests/markers_e2e.rs`; a tiny local mock server works against the
  real app (see plan 2026-06-11-clipline-event-markers.md).
- Storage GC is save-time only for now. Default cap is 10 GiB; `--disk-quota-gb <n>` overrides
  it and `0` disables it. GC deletes MP4s oldest-first with matching `.markers.json` sidecars,
  but intentionally refuses to delete the clip that was just saved even if that leaves the
  directory over budget.
- Settings saves restart the recorder service immediately. Bad window-capture titles pass
  validation if non-empty, then surface as service init errors. Hotkey support is intentionally
  limited to modifiers plus F-keys (`Alt+F10`, `Ctrl+Alt+F10`, `Ctrl+Shift+F9`, etc.). The Tauri
  global shortcut path remains registered, and a low-level Windows keyboard hook is installed as a
  fallback for focused games that do not deliver the registered shortcut.
- Trim/export is intentionally v1: finalized Clipline-authored MP4s only, H.264 video with optional
  Opus audio, one sample description per track, no frame-accurate boundary re-encode yet. Exports
  are keyframe-aligned: in snaps backward to the previous sync sample and out snaps forward to the
  next sync sample/EOF, so the exported range can be wider than the numeric in/out request.
- The main pane stacks `#review-empty` / `#review-viewer` / `#settings-page` on one grid cell.
  Any `display:` rule on those views **defeats the `[hidden]` attribute** — every stacked view
  needs an explicit `[hidden] { display: none }` restatement and an opaque background (the
  empty state once bled through the settings page).
- UI automation: occluded windows swallow synthesized clicks while `PrintWindow`
  (PW_RENDERFULLCONTENT) still captures the window content — reposition/topmost before
  clicking; `CopyFromScreen` shows black for accelerated webviews. If someone is at the
  machine, their live mouse/window-drags race synthesized input — coordinate with them
  instead of fighting for the cursor.
- Frontend logic is testable without Node: `ui/player-core.js` is pure (no DOM, no Tauri,
  exposed via `globalThis`) and `tests/player_core.rs` evaluates it in `boa_engine`
  (dev-dependency). Keep player math/formatting there, not in `main.js`, or it falls out of
  test coverage. `tests/ui_contract.rs` fails if anyone re-inlines styles/scripts into
  `index.html` or puts `controls` back on the video element.
- osu! play enrichment samples osu! window-title changes every 500 ms during game detection and
  stores them in the pending `.osu-enrichment.json` sidecar. When osu! omits `started_at`, the
  mapper prefers the latest matching title event before `ended_at`; failed plays without a match
  stay end-only, and passed plays still include 1 s of results-screen padding.
- osu! full-session saves now write title-only `ClipPlay` blocks immediately from window-title
  changes even without osu! API credentials; later API enrichment replaces those fallback plays
  with full score metadata. In Set plays, no `pp` plus rank other than `F` renders as
  `Incomplete`, and right-clicking an interval play exports that play via the same keyframe-aligned
  `export_clip` path as trims. Play exports request an `Artist - Title` filename and pass
  `includeMarkers: false`, so the resulting clip opens without the Set plays sidebar/timeline
  metadata.
- WebView2 layout: a CSS grid row only bounds its children if the track is sized — the
  `.app`/`.review-viewer` grids pin rows with `minmax(0, 1fr)` and shrink children carry
  `min-height: 0`. A content-sized row lets the video's intrinsic height push the control
  deck below the window (this exact bug shipped once and was fixed in review-player v2).
- `ddoc.md` Caveats section lists every externally-verified Windows API claim with nuance —
  check it before trusting API behavior.

## Checkpoint (2026-07-23): private bug reports and structured diagnostics

Clipline now initializes always-on structured JSONL diagnostics before settings and recorder
startup. First-party targets log at debug and dependencies at warn; a dedicated lossy 2,048-record
writer queue keeps capture work off disk I/O. Records are bounded to 16 KiB, rotate through five
4 MiB generations, expire after seven days, include session/process/thread/span identity, and
report dropped-event counts. A non-lossy writer command provides the bundle snapshot barrier.
Early panic capture writes a separately bounded forced backtrace, and release CI retains private
PDB symbols for 90 days.

Settings has a Support tab with a 10–4,000 character exact description, explicit disclosure,
prepare/file-and-size preview, separate send confirmation, cancel/retry/save/discard states, and
copyable private report ID. JavaScript errors and unhandled rejections enter the bounded native
diagnostic route. Support bundles contain only allowlisted structured/legacy/panic logs plus
manifest, system, safe-settings, and runtime JSON; logging-site hygiene and a second stable-alias
export redactor exclude paths, account/device identity, credentials, emails, and URL queries.
Recordings, screenshots, filenames, directory listings, raw settings, and Cloud/osu! secrets are
never bundled. The tray can open the actual diagnostics folder without the WebView.

The Support workflow now renders from one explicit `idle`/`preparing`/`prepared`/`uploading`/
`success` phase model. Its transient panels must keep explicit `[hidden] { display: none; }`
overrides because their grid display rules otherwise defeat WebView2's `hidden` presentation.
The description locks after preparation and upload failure/cancellation returns to the same
preview. Every build pins private submission to `https://support.dain.cafe/api/v1/reports`; build
configuration rejects attempts to substitute another destination. On Support, the settings footer
hides Save unless another tab is dirty. DOM-free phase tests plus CSS contracts guard the state
and visibility invariants.

The official intake lives in the separate sibling `clipline-support` repository. It streams
anonymous multipart uploads into bounded temporary files, validates ZIP central-directory and
manifest/hash constraints without filesystem extraction, uses SQLite/WAL plus private
S3-compatible encrypted objects, applies rotating HMAC source/global/storage quotas, retries
30-day cleanup in object-first order, backs up SQLite daily, and exposes only a server-rendered
GitHub OAuth/PKCE inbox for one immutable numeric administrator ID with server sessions, CSRF,
escaping, CSP, opaque downloads, notes/status, and immediate deletion. Clipline Cloud remains
untouched. The desktop uses the exact official HTTPS intake route in debug and release builds;
production health and readiness must remain green before shipping a client release.

## What's next (rough value order; each gets its own plan)

1. **Auto-clip on importance** (ddoc §5): `importance ≥ threshold` → auto-save; marker kinds
   already carry importance.
2. **Next supported game investigation:** CS2 is the cleanest candidate because Valve Game State
   Integration is official and maps naturally to Clipline's event rail. Apex LiveAPI is promising
   after a local normal-match smoke test. TFT likely needs OCR/synthetic round markers plus Riot
   postgame data. Valorant/Fortnite should wait until there is a safe official data source worth
   integrating.
3. **Frame-accurate trim polish** (ddoc §11): re-encode only boundary GOPs, keep the current
   stream-copy path as the instant/lossless mode.
4. **In-app HEVC/AV1 playback** (ddoc §11): the encoder matrix (milestone 23) can record HEVC/AV1,
   but WebView2 can't decode them without OS extensions — Automatic avoids them and explicit picks
   warn. A native FFmpeg decode path feeding frames to the review player would close that gap.
   Smaller follow-ups from milestone 23: bundle the lgpl-shared ffmpeg into the installer and
   revisit NVENC/QSV arg tuning (only AMF + SVT-AV1 were verified live on this RDNA2 box).
5. **Dynamic audio-session tracking** (ddoc §10): process audio is split at recorder start; new app sessions that appear mid-recording and multi-process grouping remain next.
6. **Polish toward release:** display-capture privacy warning (ddoc §9), borderless-fullscreen
   guidance (§8), WebView2-destroyed-when-minimized RAM trick (§4), installer/signing (§4).

Also worth knowing: the default `Videos\Clipline` folder on this machine holds test clips from the milestone
verifications (including `clip_1781160331.mp4` + sidecar — the marked test clip the library
demos nicely). The app may still be running in the tray from the last session.
