# Slim Core: Destroyable WebView + On-Demand FFmpeg

> **For agentic workers:** Execute this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking and remain unticked by repository convention.

**Goal:** Move Clipline back toward the original lightweight budgets without abandoning the capture
thesis or Cloud. Ship a tray-first shell that can live without a WebView2 process tree, and stop
shipping ~142 MB of LGPL FFmpeg inside every regular installer.

**Out of scope for this plan:**
- Changing the default replay storage to disk. Continuous encoded-segment writes while gaming are
  a real SSD-wear concern for long sessions; disk mode stays advanced/opt-in with the existing
  acknowledgement, folder, and quota gates.
- Removing Cloud. Cloud is Core product surface.
- Replacing Tauri/WebView2 with a native UI toolkit.
- Building a custom ultra-minimal FFmpeg fork (noted as a later lever only).
- Making shareable clipboard export itself FFmpeg-free. This plan only surfaces the dependency and
  install affordance; a later native share-export path is separate work.

## Review amendments (2026-08-04)

This revision incorporates an architecture review. Do **not** implement the earlier draft's
synchronous destroy assertion, process-global readiness atomics, `locate()`-as-verified-runtime
no-op, OnceLock “clear and re-probe” wording, progress-event-only FFmpeg install UX, or regular
SKU `beforeBundleCommand` that still stages FFmpeg.

**A5 gate amendment:** after a cold `--autostart` probe showed ~80 MiB WS / ~163 MiB commit with
zero WebViews, absolute commit ≤90 was rejected as the hard idle gate. Resident WS ≤90 + zero
WebViews is the hard success criterion; commit is relative to the no-WebView baseline (+15 MiB)
and across destroy cycles (+15 MiB rebound cap).

## Budgets and current baselines

| Metric | Design (`ddoc.md`) | Current nightly 0.1.45 | Target after this plan |
|---|---|---|---|
| Regular installer | &lt;15 MB | ~54 MB | ≤20 MB uncompressed payload intent; ship ≤25 MB setup as hard gate |
| Standalone installer | N/A | ~283 MB | Unchanged; remains the offline/Fixed-Version SKU |
| Tray-idle process tree | &lt;120 MB resident (`ddoc.md`) | ~155 MB WS after Low; cold destroy/autostart probe ~80 MiB WS / ~163 MiB commit | ≤90 MiB **private working set** after ≥90 s tray/autostart; **zero** WebView2 children; private commit is relative leak telemetry (not an absolute ≤90 ceiling) |
| Recording without FFmpeg | hardware path | MFT works today | Unchanged: H.264 MFT remains the default no-download path |

Measurement harnesses already exist:
- `scripts/measure-hidden-webview-memory.ps1` (must sample root/child private working set **and**
  private commit, with creation-time PID reuse protection).
- Release asset digests / staged FFmpeg allowlist in `apps/clipline-app/ffmpeg-runtime.json`
  (~142.1 MB staged).

## Product map: Core vs Optional

This map is the slimness contract. Implementation may land later for Optional modules, but new
work must not grow Core without an explicit budget exception.

### Core (ships in every regular build)

- WGC/DXGI capture, WASAPI audio, Hybrid MP4, Save Replay, full-session recording
- Hardware H.264 via Media Foundation (and other MFT hardware backends already probed)
- League Live Client markers + first-party game detection / custom games
- Tray, hotkeys, settings needed for capture/recording/library
- Local library + lossless keyframe trim + in-app H.264 review
- Cloud connect / upload / library / share URL flows
- Updater, diagnostics folder, private support reports
- On-demand **managed** FFmpeg runtime installer (not the bytes themselves)

### Optional / separately packaged

| Surface | Status after this plan | Rationale |
|---|---|---|
| Managed FFmpeg LGPL runtime | **Downloaded on demand** into `%LOCALAPPDATA%\Clipline\ffmpeg` | Needed for SVT-AV1, FFmpeg encoder backends, posters, audio sidecar extraction, **and shareable clipboard export** — not for default MFT H.264 recording or native H.264 review |
| Standalone Fixed Version WebView2 | Keep as separate SKU | ~300 MB Microsoft CAB; never the “lightweight” story |
| osu! API enrichment | Remains available, marked Optional in docs/settings copy | ~2.6k LOC; enrich-only; not required for Core recorder |
| Disk replay storage | Remains Advanced / explicit acknowledgement | SSD wear + cache management; do not default on |
| Native HEVC/AV1 in-app preview decode | Deferred | Would pull FFmpeg decode into Core preview path |
| FFmpeg-free shareable clipboard export | Deferred | Today `library.rs` always routes share exports through FFmpeg AAC/H.264 even after stream-copy remux |

Cloud stays Core. Do not feature-gate it behind a Lite SKU in this milestone.

## Why destroy-webview is still the right idle-RAM move

`ddoc.md` already calls destroy/recreate the stronger option after `MemoryUsageTargetLevel::Low`.
Nightly 0.1.42 proved Low: tray-idle tree ~335 MB → ~155 MB. That still misses the &lt;120 MB
budget, and Low only trims the resident set — private commit was not proven released.

Prior scare (handoff, ~0.1.12): destroying the Tauri window could leave a dead `main` label whose
IPC failed with `failed to receive message from webview`. Recovery labels made Windows 10 worse.
Tauri queues destruction asynchronously, and `open_main_window` currently treats any registered
label as live (`MainWindowOpenTarget::ExistingMain`). This plan must therefore model
`Destroying` → `Destroyed` natively, queue opens that arrive mid-destroy, and recreate only after
`WindowEvent::Destroyed` has cleared the label.

## Why on-demand FFmpeg is safe enough

`clipline_capture::ffmpeg::locate` already searches, in order:
`CLIPLINE_FFMPEG` → packaged resource → exe-adjacent → `%LOCALAPPDATA%\Clipline\ffmpeg` →
`%APPDATA%\Clipline\ffmpeg` → PATH. That discovery path only proves `-version` succeeds. It is
**not** a managed-runtime verifier: overrides, adjacent binaries, roaming installs, and PATH hits
are external/unmanaged.

Today the regular NSIS build stages the full allowlisted runtime via
`tauri.conf.json` `bundle.resources: ["ffmpeg/"]` (~70 MB `avcodec` alone) and
`beforeBundleCommand` runs `scripts/verify-ffmpeg-resource.ps1`. Default recording on this machine
resolves `EncoderApi::Mft`, so Core capture does not need the child process. Posters, some sidecar
extraction, and shareable clipboard export already fail with actionable “ffmpeg is not available”
errors.

## Invariants

- Recorder, global/mouse hotkeys, tray menu, and single-instance behavior keep working with **zero**
  live WebView2 children.
- Destroying the UI never stops an active recording or drops the replay ring.
- `open_main_window` never treats a label that is mid-destroy or dead as `ExistingMain`.
- Opens requested during `Destroying` are queued and satisfied exactly once after `Destroyed`.
- Frontend readiness and repair watchdogs are **per window generation**; an old timer cannot fail a
  newer window, and recreating the UI always re-arms readiness.
- Every `frontend_ready` replays durable recorder status plus durable warnings for that generation,
  not only a one-shot waiting status / drained startup warning list.
- FFmpeg remains a separate LGPL-replaceable program. Never link libavcodec. Never ship GPL x264/x265.
- Managed-runtime install verifies archive size, digest, allowlist hashes, and provenance before
  publish. External `locate()` hits are reported as unmanaged and do not satisfy “verified
  installed.”
- Encoder capability caching keeps MFT results stable for the process, but FFmpeg capabilities are
  versioned/replaceable after managed install or repair without restarting the app.
- FFmpeg ensure is native, queryable, single-flight, and recoverable across UI destroy/recreate.
- Regular installer must not embed `apps/clipline-app/ffmpeg/` resources or run the FFmpeg resource
  verifier as its `beforeBundleCommand`.
- Standalone SKU may still bundle FFmpeg for offline machines; document that clearly.
- Missing managed FFmpeg never blocks MFT H.264 recording or native H.264 library playback.
- Cloud commands and settings remain compiled into Core.

---

## Milestone A — Destroyable WebView shell

### Task A1: Failing lifecycle tests for destroy / recreate / race

- [ ] Extend `WindowLifecycleMode` with `Destroying` and `Destroyed` (distinct from `Tray` /
      `Taskbar` hide). `backgrounded` remains true for both.
- [ ] Add pure state-machine tests for:
  - close-to-tray enters `Destroying` immediately and only reaches `Destroyed` on a simulated
    `WindowEvent::Destroyed`
  - `open_main_window` during `Destroying` does **not** call `build_main_window`; it records a
    pending open
  - `Destroyed` with a pending open builds exactly one new window and clears the pending flag
  - a stale registered label while mode is `Destroying`/`Destroyed` is never `ExistingMain`
- [ ] Add UI-contract / Boa coverage: entering `Destroying`/`Destroyed` invalidates request
      generations, releases media/mic work, and does not expect gallery DOM to survive.
- [ ] Add an immediate close→open race regression (the dead-label bug): destroy requested, open
      requested before Destroyed, then Destroyed fires; assert one recreate and no reveal of the
      dying label.
- [ ] Run the focused tests and confirm they fail on current hide/Low + label-presence behavior.

### Task A2: Autostart creates no WebView (`create: false`)

- [ ] Set the configured main window to `"create": false` in `apps/clipline-app/tauri.conf.json`
      while retaining the window config as the `WebviewWindowBuilder::from_config` template. Pinned
      Tauri already supports this; do **not** retain a destroy-on-start fallback.
- [ ] Cold `--autostart` must build tray/hotkeys/recorder only — no `msedgewebview2.exe` children
      for the Clipline tree.
- [ ] Delete `hide_autostart_webviews` once `create: false` is proven; it is obsolete, not a
      temporary fallback.
- [ ] Single-instance secondary `--autostart` launches remain quiet (no reveal / no create).
- [ ] Normal launches and tray Open still create through `build_main_window`.

### Task A3: Close / tray destroy path

- [ ] Replace `send_main_window_to_tray`'s hide/Low sequence with an async destroy sequence when
      `close_to_tray` is enabled:
      1. stop mic test / invalidate UI generations
      2. publish `Destroying` lifecycle revision
      3. request `WebviewWindow` destroy for every app-labeled window
      4. on `WindowEvent::Destroyed` for the last app window, publish `Destroyed` and drain any
         pending open
- [ ] Do **not** assert the label is gone immediately after calling destroy.
- [ ] Minimize-to-taskbar may keep current hide/Low behavior for this milestone (restore latency
      matters there). Document that tray/close is the strong RAM path; taskbar remains soft-trim.
- [ ] Tray menu, hotkeys, recorder service, and elevation flows must not require a live webview.
- [ ] Preserve Quit App as a real process exit distinct from destroy-to-tray.

### Task A4: Per-generation readiness + recreate rehydrate

- [ ] Replace process-global `FRONTEND_READY` / `WEBVIEW_READY_WATCHDOG_ARMED` atomics with a
      managed per-window generation (monotonic counter on each successful `build_main_window`).
- [ ] `arm_frontend_ready_watchdog(generation)` captures that generation; expiry only fires a
      repair notice if the current generation still matches and that generation never became ready.
- [ ] `frontend_ready` marks readiness for the active generation only.
- [ ] Every `frontend_ready` response must include:
  - lifecycle snapshot
  - durable startup/runtime warnings for this UI generation (do not rely solely on a one-shot
    `StartupWarnings::take()` that empties after the first UI)
  - replay of durable recorder status (not only `current_waiting_status()`)
- [ ] Route tray Open / non-autostart secondary launch / restore through the destroy-aware open
      helper: queue if `Destroying`, build if `Destroyed`/absent, reveal only if a live
      non-destroying main exists.
- [ ] Recreate reveal order remains Normal → controller show → native show → unminimize → focus →
      Foreground publish, then arm the generation-scoped watchdog.
- [ ] Add diagnostics for destroy → recreate timings, generation ids, and child-process counts.
- [ ] Commit as `perf(app): destroy webview while trayed and recreate on open`.

### Task A5: Measure idle RAM gate

> **Gate amendment (2026-08-04):** `ddoc.md`'s &lt;120 MB idle metric is **resident** memory.
> Active replay memory scales with the buffer. Absolute private commit is useful as
> leak/reclamation telemetry, **not** as the same absolute idle ceiling. Do **not** raise an
> absolute commit hard limit to paper over the recorder baseline.

- [ ] Use `scripts/measure-destroy-webview-memory.ps1` (destroy/autostart absolute harness). Keep
      `scripts/measure-hidden-webview-memory.ps1` for historical hide/Low comparisons only.
- [ ] Measure:
  - cold `--autostart` after 90–120 s (no WebView expected)
  - one **recorder-stopped, no-WebView** control (telemetry only; do not expand Milestone A into
    recorder optimization)
  - destroy-to-tray after a visible library/review session
  - recreate → destroy across 3 cycles
  - immediate close→open race (no dead window / no dual WebView trees)
- [ ] **Hard gates:**
  - **Zero** Clipline-owned `msedgewebview2.exe` children after autostart and every destroy settle
  - Settled tree **private working set ≤ 90 MiB**
  - First/final post-destroy private commit ≤ cold no-WebView autostart baseline **+ 15 MiB**
  - Third-cycle commit ≤ first-cycle commit **+ 15 MiB** (no rebound growth)
  - Close→open race and all recreate cycles succeed
- [ ] Record absolute private commit as telemetry (including the recorder-stopped control), not as
      a hard active-recorder ceiling.
- [ ] Soft check: recreate to foreground completes and shows current recorder state within 2 s on
      the dev machine (record actual; do not fail CI on absolute latency yet).
- [ ] **Stop before Milestone B only if** commit grows across cycles **or** remains more than
      15 MiB above the cold no-WebView autostart baseline. WS≤90 + zero WebViews with stable
      commit near that baseline is enough to proceed.

---

## Milestone B — On-demand managed FFmpeg runtime

### Task B1: Capability matrix + failing UX contracts

- [ ] Document and test a pure capability helper:
  - `recording_without_ffmpeg_possible` when any MFT/hardware non-FFmpeg encoder exists
  - `ffmpeg_required_for` reasons:
    - `svt_av1`
    - `ffmpeg_backend_encoder`
    - `poster`
    - `audio_sidecar_extract` (only where still FFmpeg-backed)
    - `shareable_clipboard_export` (today always FFmpeg-backed in `library.rs`)
- [ ] Distinguish discovery kinds:
  - `ManagedVerified` — LOCALAPPDATA managed runtime passed full manifest verification
  - `ExternalUnmanaged` — `CLIPLINE_FFMPEG`, packaged/adjacent, `%APPDATA%`, or PATH binary that
    merely runs `-version`
  - `Missing`
- [ ] `ensure_ffmpeg_runtime` is a no-op only for `ManagedVerified`. External/unmanaged runtimes
      are reported distinctly and do not skip repair when the user asks to Install/Repair managed
      runtime.
- [ ] Add UI-contract fixtures for Library poster empty-state, Copy Clip / share export affordance,
      and Settings encoder rows when managed FFmpeg is absent vs present.
- [ ] Add failing app tests for status query + ensure no-op when managed verification already
      passes.

### Task B2: Managed-runtime verifier (separate from `locate()`)

- [ ] Introduce `verify_managed_ffmpeg_runtime(dir, manifest) -> Result<ManagedRuntimeInfo, _>`
      that checks:
  - every allowlisted file exists with exact size + sha256
  - `PROVENANCE.json` matches the committed/runtime manifest identity
  - no required file is missing; unexpected critical binaries may be ignored or rejected per
    documented policy, but tampered allowlisted DLLs must fail
- [ ] Keep `locate()` for subprocess execution discovery. Add a higher-level
      `ffmpeg_runtime_status()` used by UI/ensure that classifies managed vs external.
- [ ] Tests: happy managed tree; tampered DLL; stale/missing provenance; override/PATH reported as
      external; repair path rejects the bad tree before re-download publish.

### Task B3: Native single-flight ensure state + bounded download

- [ ] Define a native `FfmpegInstallState` owned by the app (not the WebView):
  `Idle | Checking | Downloading { bytes, total } | Verifying | Publishing | Ready |
  Failed { message } | Cancelled`.
- [ ] Expose `ffmpeg_runtime_status` / `ensure_ffmpeg_runtime` / `cancel_ffmpeg_runtime_install`
      commands. Status is queryable after UI recreate; progress events are notifications only.
- [ ] Single-flight: concurrent ensures coalesce on one job and observe the same state machine.
- [ ] Reuse `ffmpeg-runtime.json` immutable URL/sha256/allowlist. **Add exact `archive_size`
      bytes** to the manifest and enforce it.
- [ ] Before download: check free space ≥ `archive_size + staged allowlist total + margin`.
- [ ] Download to `%LOCALAPPDATA%\Clipline\ffmpeg-staging\` with a hard byte cap (`archive_size`);
      abort and delete partials on overflow, hash mismatch, cancel, or crash-recovery startup sweep.
- [ ] Verify archive digest, extract allowlisted files only, write `PROVENANCE.json`, verify the
      staged tree, then atomically publish to `%LOCALAPPDATA%\Clipline\ffmpeg\`.
- [ ] Refuse to execute downloaded bytes before verification (same L-13 invariants).
- [ ] No silent background download on cold start.
- [ ] Tests: concurrent ensure coalescing; cancel cleans partials; destroy→recreate mid-download
      recovers progress via status query; crash-recovery sweep removes abandoned staging; disk-space
      and overflow failures.

### Task B4: Replaceable FFmpeg capability cache

- [ ] Split `service.rs` encoder capability caching:
  - MFT capabilities may remain process-static
  - FFmpeg capabilities are stored in a replaceable/versioned slot keyed by managed runtime
    identity (path + provenance/version) or “external/missing”
- [ ] After managed publish/repair, refresh only the FFmpeg half and republish encoder options to
      the UI without requiring app restart.
- [ ] Do **not** claim `locate()` has a cache to clear; it does not.
- [ ] Test: probe before install sees no SVT/FFmpeg backends (or only external if present); complete
      managed install; probe/options update in-process; recorder can select a newly available FFmpeg
      encoder without restart.

### Task B5: Stop bundling FFmpeg in the regular installer / release path

- [ ] Remove `ffmpeg/` from `apps/clipline-app/tauri.conf.json` `bundle.resources`.
- [ ] Remove the regular `build.beforeBundleCommand` FFmpeg verifier from `tauri.conf.json`.
- [ ] Move FFmpeg staging/verify into the **standalone** overlay/workflow only
      (`tauri.standalone.conf.json` / `docs/release.workflow.yml` standalone job, and
      `docs/release-updates.md`).
- [ ] Regular release preflight must assert the NSIS package contains **none** of the allowlisted
      FFmpeg payload names from `ffmpeg-runtime.json` (not merely “no avcodec-*.dll”).
- [ ] Standalone may continue to stage/bundle the verified runtime for offline machines.
- [ ] Update README / install docs: Core installer size expectation; first-use managed download;
      `CLIPLINE_FFMPEG` remains an advanced external override.
- [ ] **Hard gate:** regular setup.exe ≤ 25 MB (record exact). Stretch goal ≤ 20 MB.

### Task B6: Product copy and graceful degradation

- [ ] Settings status: `FFmpeg runtime: not installed | managed (version) | external/unmanaged
      (path summary)` with Install / Repair / Cancel actions bound to the native state machine.
- [ ] Library posters keep gradient placeholder + one actionable install affordance rather than
      error spam.
- [ ] Copy Clip / share export surfaces the `shareable_clipboard_export` reason and the same
      install affordance when managed/external FFmpeg is unavailable.
- [ ] Encoder dropdown: MFT options available immediately; FFmpeg-backed options either hidden or
      marked “Requires FFmpeg runtime” and trigger ensure on select.
- [ ] Cloud uploads continue on the native Opus mix path; do not reintroduce an FFmpeg-required
      cloud mix regression.

### Task B7: Verify encode/review without download

- [ ] On a machine with AMF/NVENC/QSV or software MFT H.264: fresh profile, no LOCALAPPDATA managed
      ffmpeg, record 30 s, Save Replay, open in review, scrub, export trim — all green.
- [ ] Copy Clip without FFmpeg shows install affordance rather than a hard-to-parse failure.
- [ ] Install managed FFmpeg on demand; posters generate; share export works; SVT-AV1 appears only
      after replaceable probe refresh.
- [ ] Commit as `feat(app): install verified FFmpeg runtime on demand` and
      `build(app): omit FFmpeg from regular installer`.

---

## Milestone C — Docs, handoff, optional-module ledger

- [ ] Update `ddoc.md` success metrics / UI section:
  - tray destroy/recreate (`Destroying`/`Destroyed`, queued open) is the idle-RAM mechanism
  - regular installer omits FFmpeg; managed runtime is on-demand under LOCALAPPDATA
  - Cloud remains Core; osu enrichment and disk replay remain optional/advanced
- [ ] Update `docs/release-updates.md` / `docs/release.workflow.yml` so regular vs standalone FFmpeg
      preflights match Task B5.
- [ ] Update `handoff.md` checkpoint with measured installer bytes and tray RAM (WS + commit), plus
      the destroy-race and managed-runtime verifier notes.
- [ ] Add a short “Core vs Optional” section to handoff or docs so future milestones cannot silently
      re-bundle FFmpeg or resurrect a permanent tray WebView.
- [ ] Run `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Stop any running `clipline-app.exe`, `cargo run -p clipline-app`, leave it open for acceptance.

---

## Manual acceptance checklist

1. Enable Open on startup; reboot or simulate `--autostart`; confirm tray presence, Save Replay
   hotkey works, and Task Manager shows **no** Clipline WebView2 children while idle.
2. Open Clipline from tray; library/recorder state appears with current recorder status/warnings;
   close to tray; WebView2 children leave again; repeat three times with no leaked processes.
3. From a visible UI, close to tray and immediately choose Open Clipline; exactly one healthy window
   appears (destroy race).
4. Fresh install (or wiped `%LOCALAPPDATA%\Clipline\ffmpeg`): record via MFT H.264; review plays;
   posters and Copy Clip show install affordances rather than hard-failing recording.
5. Start Install FFmpeg from Settings, destroy the UI mid-download, recreate, confirm progress /
   completion is recoverable from native status; then cancel once and confirm staging leftovers are
   removed.
6. After managed install: posters generate; share export works; new FFmpeg encoder options appear
   without restart; provenance/allowlist files exist; tamper a DLL and confirm status becomes failed
   / Repair re-downloads cleanly.
7. Point `CLIPLINE_FFMPEG` at some other working binary and confirm UI reports external/unmanaged
   rather than managed-verified.
8. Connect Cloud, upload a clip with output+mic — still works without regressing to FFmpeg-required
   mix.
9. Confirm disk replay remains off by default and still requires acknowledgement if enabled.
10. Inspect regular setup contents / preflight logs: no allowlisted FFmpeg payload names present.

## Suggested commits

- `docs(plan): slim core via destroyable webview and on-demand ffmpeg`
- `test(app): cover async webview destroy, queued open, and readiness generations`
- `perf(app): destroy webview while trayed and recreate on open`
- `test(app): cover managed ffmpeg verifier, single-flight ensure, and capability refresh`
- `feat(app): install verified FFmpeg runtime on demand`
- `build(app): omit FFmpeg from regular installer`
- `docs: record slim-core budgets, core/optional map, and measurements`

## Non-goals / later levers (do not sneak into this PR train)

- Defaulting replay storage to disk
- Service-process split (`clipline-service` + ephemeral UI)
- egui/WinUI tray shell
- Custom minimal FFmpeg build
- Feature-flagging Cloud out of Core
- Native FFmpeg-free shareable clipboard export
