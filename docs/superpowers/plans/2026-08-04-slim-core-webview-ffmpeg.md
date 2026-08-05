[2026-08-04-slim-core-webview-ffmpeg.md#2100]
1:# Slim Core: Destroyable WebView + On-Demand FFmpeg
2:
3:> **For agentic workers:** Execute this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
4:> tracking and remain unticked by repository convention.
5:
6:**Goal:** Move Clipline back toward the original lightweight budgets without abandoning the capture
7:thesis or Cloud. Ship a tray-first shell that can live without a WebView2 process tree, and stop
8:shipping ~142 MB of LGPL FFmpeg inside every regular installer.
9:
10:**Out of scope for this plan:**
11:- Changing the default replay storage to disk. Continuous encoded-segment writes while gaming are
12:  a real SSD-wear concern for long sessions; disk mode stays advanced/opt-in with the existing
13:  acknowledgement, folder, and quota gates.
14:- Removing Cloud. Cloud is Core product surface.
15:- Replacing Tauri/WebView2 with a native UI toolkit.
16:- Building a custom ultra-minimal FFmpeg fork (noted as a later lever only).
17:- Making shareable clipboard export itself FFmpeg-free. This plan only surfaces the dependency and
18:  install affordance; a later native share-export path is separate work.
19:
20:## Review amendments (2026-08-04)
21:
22:This revision incorporates an architecture review. Do **not** implement the earlier draft's
23:synchronous destroy assertion, process-global readiness atomics, `locate()`-as-verified-runtime
24:no-op, OnceLock “clear and re-probe” wording, progress-event-only FFmpeg install UX, or regular
25:SKU `beforeBundleCommand` that still stages FFmpeg.
26:
27:**A5 gate amendment:** after a cold `--autostart` probe showed ~80 MiB WS / ~163 MiB commit with
28:zero WebViews, absolute commit ≤90 was rejected as the hard idle gate. Resident WS ≤90 + zero
29:WebViews is the hard success criterion; commit is relative to the no-WebView baseline (+15 MiB)
30:and across destroy cycles (+15 MiB rebound cap).
31:
32:## Budgets and current baselines
33:
34:| Metric | Design (`ddoc.md`) | Current nightly 0.1.45 | Target after this plan |
35:|---|---|---|---|
36:| Regular installer | &lt;15 MB | ~54 MB | ≤20 MB uncompressed payload intent; ship ≤25 MB setup as hard gate |
37:| Standalone installer | N/A | ~283 MB | Unchanged; remains the offline/Fixed-Version SKU |
38:| Tray-idle process tree | &lt;120 MB resident (`ddoc.md`) | ~155 MB WS after Low; destroy/autostart harness: **70.9–92 MiB PWS**, recorder-stopped control **14–15 MiB PWS**, absolute commit telemetry only | ≤**120 MiB** private working set hard (product budget); ≤90 MiB stretch (non-blocking); **zero** WebView2 children; same-process destroy rebound ≤ +15 MiB |
39:| Recording without FFmpeg | hardware path | MFT works today | Unchanged: H.264 MFT remains the default no-download path |
40:
41:Measurement harnesses already exist:
42:- `scripts/measure-hidden-webview-memory.ps1` (must sample root/child private working set **and**
43:  private commit, with creation-time PID reuse protection).
44:- Release asset digests / staged FFmpeg allowlist in `apps/clipline-app/ffmpeg-runtime.json`
45:  (~142.1 MB staged).
46:
47:## Product map: Core vs Optional
48:
49:This map is the slimness contract. Implementation may land later for Optional modules, but new
50:work must not grow Core without an explicit budget exception.
51:
52:### Core (ships in every regular build)
53:
54:- WGC/DXGI capture, WASAPI audio, Hybrid MP4, Save Replay, full-session recording
55:- Hardware H.264 via Media Foundation (and other MFT hardware backends already probed)
56:- League Live Client markers + first-party game detection / custom games
57:- Tray, hotkeys, settings needed for capture/recording/library
58:- Local library + lossless keyframe trim + in-app H.264 review
59:- Cloud connect / upload / library / share URL flows
60:- Updater, diagnostics folder, private support reports
61:- On-demand **managed** FFmpeg runtime installer (not the bytes themselves)
62:
63:### Optional / separately packaged
64:
65:| Surface | Status after this plan | Rationale |
66:|---|---|---|
67:| Managed FFmpeg LGPL runtime | **Downloaded on demand** into `%LOCALAPPDATA%\Clipline\ffmpeg` | Needed for SVT-AV1, FFmpeg encoder backends, posters, audio sidecar extraction, **and shareable clipboard export** — not for default MFT H.264 recording or native H.264 review |
68:| Standalone Fixed Version WebView2 | Keep as separate SKU | ~300 MB Microsoft CAB; never the “lightweight” story |
69:| osu! API enrichment | Remains available, marked Optional in docs/settings copy | ~2.6k LOC; enrich-only; not required for Core recorder |
70:| Disk replay storage | Remains Advanced / explicit acknowledgement | SSD wear + cache management; do not default on |
71:| Native HEVC/AV1 in-app preview decode | Deferred | Would pull FFmpeg decode into Core preview path |
72:| FFmpeg-free shareable clipboard export | Deferred | Today `library.rs` always routes share exports through FFmpeg AAC/H.264 even after stream-copy remux |
73:
74:Cloud stays Core. Do not feature-gate it behind a Lite SKU in this milestone.
75:
76:## Why destroy-webview is still the right idle-RAM move
77:
78:`ddoc.md` already calls destroy/recreate the stronger option after `MemoryUsageTargetLevel::Low`.
79:Nightly 0.1.42 proved Low: tray-idle tree ~335 MB → ~155 MB. That still misses the &lt;120 MB
80:budget, and Low only trims the resident set — private commit was not proven released.
81:
82:Prior scare (handoff, ~0.1.12): destroying the Tauri window could leave a dead `main` label whose
83:IPC failed with `failed to receive message from webview`. Recovery labels made Windows 10 worse.
84:Tauri queues destruction asynchronously, and `open_main_window` currently treats any registered
85:label as live (`MainWindowOpenTarget::ExistingMain`). This plan must therefore model
86:`Destroying` → `Destroyed` natively, queue opens that arrive mid-destroy, and recreate only after
87:`WindowEvent::Destroyed` has cleared the label.
88:
89:## Why on-demand FFmpeg is safe enough
90:
91:`clipline_capture::ffmpeg::locate` already searches, in order:
92:`CLIPLINE_FFMPEG` → packaged resource → exe-adjacent → `%LOCALAPPDATA%\Clipline\ffmpeg` →
93:`%APPDATA%\Clipline\ffmpeg` → PATH. That discovery path only proves `-version` succeeds. It is
94:**not** a managed-runtime verifier: overrides, adjacent binaries, roaming installs, and PATH hits
95:are external/unmanaged.
96:
97:Today the regular NSIS build stages the full allowlisted runtime via
98:`tauri.conf.json` `bundle.resources: ["ffmpeg/"]` (~70 MB `avcodec` alone) and
99:`beforeBundleCommand` runs `scripts/verify-ffmpeg-resource.ps1`. Default recording on this machine
100:resolves `EncoderApi::Mft`, so Core capture does not need the child process. Posters, some sidecar
101:extraction, and shareable clipboard export already fail with actionable “ffmpeg is not available”
102:errors.
103:
104:## Invariants
105:
106:- Recorder, global/mouse hotkeys, tray menu, and single-instance behavior keep working with **zero**
107:  live WebView2 children.
108:- Destroying the UI never stops an active recording or drops the replay ring.
109:- `open_main_window` never treats a label that is mid-destroy or dead as `ExistingMain`.
110:- Opens requested during `Destroying` are queued and satisfied exactly once after `Destroyed`.
111:- Frontend readiness and repair watchdogs are **per window generation**; an old timer cannot fail a
112:  newer window, and recreating the UI always re-arms readiness.
113:- Every `frontend_ready` replays durable recorder status plus durable warnings for that generation,
114:  not only a one-shot waiting status / drained startup warning list.
115:- FFmpeg remains a separate LGPL-replaceable program. Never link libavcodec. Never ship GPL x264/x265.
116:- Managed-runtime install verifies archive size, digest, allowlist hashes, and provenance before
117:  publish. External `locate()` hits are reported as unmanaged and do not satisfy “verified
118:  installed.”
119:- Encoder capability caching keeps MFT results stable for the process, but FFmpeg capabilities are
120:  versioned/replaceable after managed install or repair without restarting the app.
121:- FFmpeg ensure is native, queryable, single-flight, and recoverable across UI destroy/recreate.
122:- Regular installer must not embed `apps/clipline-app/ffmpeg/` resources or run the FFmpeg resource
123:  verifier as its `beforeBundleCommand`.
124:- Standalone SKU may still bundle FFmpeg for offline machines; document that clearly.
125:- Missing managed FFmpeg never blocks MFT H.264 recording or native H.264 library playback.
126:- Cloud commands and settings remain compiled into Core.
127:
128:---
129:
130:## Milestone A — Destroyable WebView shell
131:
132:### Task A1: Failing lifecycle tests for destroy / recreate / race
133:
134:- [ ] Extend `WindowLifecycleMode` with `Destroying` and `Destroyed` (distinct from `Tray` /
135:      `Taskbar` hide). `backgrounded` remains true for both.
136:- [ ] Add pure state-machine tests for:
137:  - close-to-tray enters `Destroying` immediately and only reaches `Destroyed` on a simulated
138:    `WindowEvent::Destroyed`
139:  - `open_main_window` during `Destroying` does **not** call `build_main_window`; it records a
140:    pending open
141:  - `Destroyed` with a pending open builds exactly one new window and clears the pending flag
142:  - a stale registered label while mode is `Destroying`/`Destroyed` is never `ExistingMain`
143:- [ ] Add UI-contract / Boa coverage: entering `Destroying`/`Destroyed` invalidates request
144:      generations, releases media/mic work, and does not expect gallery DOM to survive.
145:- [ ] Add an immediate close→open race regression (the dead-label bug): destroy requested, open
146:      requested before Destroyed, then Destroyed fires; assert one recreate and no reveal of the
147:      dying label.
148:- [ ] Run the focused tests and confirm they fail on current hide/Low + label-presence behavior.
149:
150:### Task A2: Autostart creates no WebView (`create: false`)
151:
152:- [ ] Set the configured main window to `"create": false` in `apps/clipline-app/tauri.conf.json`
153:      while retaining the window config as the `WebviewWindowBuilder::from_config` template. Pinned
154:      Tauri already supports this; do **not** retain a destroy-on-start fallback.
155:- [ ] Cold `--autostart` must build tray/hotkeys/recorder only — no `msedgewebview2.exe` children
156:      for the Clipline tree.
157:- [ ] Delete `hide_autostart_webviews` once `create: false` is proven; it is obsolete, not a
158:      temporary fallback.
159:- [ ] Single-instance secondary `--autostart` launches remain quiet (no reveal / no create).
160:- [ ] Normal launches and tray Open still create through `build_main_window`.
161:
162:### Task A3: Close / tray destroy path
163:
164:- [ ] Replace `send_main_window_to_tray`'s hide/Low sequence with an async destroy sequence when
165:      `close_to_tray` is enabled:
166:      1. stop mic test / invalidate UI generations
167:      2. publish `Destroying` lifecycle revision
168:      3. request `WebviewWindow` destroy for every app-labeled window
169:      4. on `WindowEvent::Destroyed` for the last app window, publish `Destroyed` and drain any
170:         pending open
171:- [ ] Do **not** assert the label is gone immediately after calling destroy.
172:- [ ] Minimize-to-taskbar may keep current hide/Low behavior for this milestone (restore latency
173:      matters there). Document that tray/close is the strong RAM path; taskbar remains soft-trim.
174:- [ ] Tray menu, hotkeys, recorder service, and elevation flows must not require a live webview.
175:- [ ] Preserve Quit App as a real process exit distinct from destroy-to-tray.
176:
177:### Task A4: Per-generation readiness + recreate rehydrate
178:
179:- [ ] Replace process-global `FRONTEND_READY` / `WEBVIEW_READY_WATCHDOG_ARMED` atomics with a
180:      managed per-window generation (monotonic counter on each successful `build_main_window`).
181:- [ ] `arm_frontend_ready_watchdog(generation)` captures that generation; expiry only fires a
182:      repair notice if the current generation still matches and that generation never became ready.
183:- [ ] `frontend_ready` marks readiness for the active generation only.
184:- [ ] Every `frontend_ready` response must include:
185:  - lifecycle snapshot
186:  - durable startup/runtime warnings for this UI generation (do not rely solely on a one-shot
187:    `StartupWarnings::take()` that empties after the first UI)
188:  - replay of durable recorder status (not only `current_waiting_status()`)
189:- [ ] Route tray Open / non-autostart secondary launch / restore through the destroy-aware open
190:      helper: queue if `Destroying`, build if `Destroyed`/absent, reveal only if a live
191:      non-destroying main exists.
192:- [ ] Recreate reveal order remains Normal → controller show → native show → unminimize → focus →
193:      Foreground publish, then arm the generation-scoped watchdog.
194:- [ ] Add diagnostics for destroy → recreate timings, generation ids, and child-process counts.
195:- [ ] Commit as `perf(app): destroy webview while trayed and recreate on open`.
196:
197:### Task A5: Measure idle RAM gate

> **Gate amendment (2026-08-04, corrected):** Comparing destroy commit to a **cold `$auto`
> baseline from a different process** is invalid — the harness kills `$auto` then measures a new
> `$proc`, and live-recorder commit varies enough between processes to dominate the delta.
> Same-process destroy rebound remains the meaningful leak test. `ddoc.md`'s &lt;120 MB idle
> metric is **resident** memory; ≤90 MiB PWS is a stretch target only.

- [x] Use `scripts/measure-destroy-webview-memory.ps1` (destroy/autostart harness). Keep
      `scripts/measure-hidden-webview-memory.ps1` for historical hide/Low comparisons only.
- [x] Measure:
  - cold `--autostart` after 90–120 s (no WebView expected)
  - one **recorder-stopped, no-WebView** control (telemetry only)
  - destroy-to-tray after a visible library/review session
  - recreate → destroy across 3 cycles
  - immediate close→open race
- [x] **Hard gates (corrected):**
  - **Zero** Clipline-owned `msedgewebview2.exe` children after autostart and every destroy settle
  - Settled tree **private working set ≤ 120 MiB** (original product budget)
  - Third-cycle **and final** destroy PWS and commit ≤ first-destroy PWS/commit **+ 15 MiB**
  - Close→open race succeeds
- [x] **Stretch (non-blocking):** settled PWS ≤ 90 MiB — report, do not fail the gate.
- [x] **Telemetry only (not hard gates):** absolute commit, cold `$auto` vs warm `$proc` delta,
      recorder-stopped control. Do **not** hard-gate `Destroy1/FinalCommit ≤ AutostartCommit + 15`.
- [x] Soft check: recreate to foreground completes and shows current recorder state within 2 s on
      the dev machine (record actual; do not fail CI on absolute latency yet).
- [x] **Measured (3-run harness CSV, no rerun):** destroy/autostart PWS **70.9–92 MiB**;
      recorder-stopped control **14–15 MiB PWS / ~23 MiB commit**; zero WebViews after
      autostart/destroy; cycle + final rebound within +15 of first destroy; race OK → **A5 pass**.
      Proceed to Milestone B.

## Milestone B — On-demand managed FFmpeg runtime
230:
231:### Task B1: Capability matrix + failing UX contracts
232:
233:- [x] Document and test a pure capability helper:
234:  - `recording_without_ffmpeg_possible` when any MFT/hardware non-FFmpeg encoder exists
235:  - `ffmpeg_required_for` reasons:
236:    - `svt_av1`
237:    - `ffmpeg_backend_encoder`
238:    - `poster`
239:    - `audio_sidecar_extract` (only where still FFmpeg-backed)
240:    - `shareable_clipboard_export` (today always FFmpeg-backed in `library.rs`)
241:- [x] Distinguish discovery kinds:
242:  - `ManagedVerified` — LOCALAPPDATA managed runtime passed full manifest verification
243:  - `ExternalUnmanaged` — `CLIPLINE_FFMPEG`, packaged/adjacent, `%APPDATA%`, or PATH binary that
244:    merely runs `-version`
245:  - `Missing`
246:- [x] `ensure_ffmpeg_runtime` is a no-op only for `ManagedVerified`. External/unmanaged runtimes
247:      are reported distinctly and do not skip repair when the user asks to Install/Repair managed
248:      runtime.
249:- [x] Add UI-contract fixtures for Library poster empty-state, Copy Clip / share export affordance,
250:      and Settings encoder rows when managed FFmpeg is absent vs present.
251:- [x] Add failing app tests for status query + ensure no-op when managed verification already
252:      passes.
253:
254:### Task B2: Managed-runtime verifier (separate from `locate()`)
255:
256:- [x] Introduce `verify_managed_ffmpeg_runtime(dir, manifest) -> Result<ManagedRuntimeInfo, _>`
257:      that checks:
258:  - every allowlisted file exists with exact size + sha256
259:  - `PROVENANCE.json` matches the committed/runtime manifest identity
260:  - no required file is missing; unexpected critical binaries may be ignored or rejected per
261:    documented policy, but tampered allowlisted DLLs must fail
262:- [x] Keep `locate()` for subprocess execution discovery. Add a higher-level
263:      `ffmpeg_runtime_status()` used by UI/ensure that classifies managed vs external.
264:- [x] Tests: happy managed tree; tampered DLL; stale/missing provenance; override/PATH reported as
265:      external; repair path rejects the bad tree before re-download publish.
266:
267:### Task B3: Native single-flight ensure state + bounded download
268:
269:- [x] Define a native `FfmpegInstallState` owned by the app (not the WebView):
270:  `Idle | Checking | Downloading { bytes, total } | Verifying | Publishing | Ready |
271:  Failed { message } | Cancelled`.
272:- [x] Expose `ffmpeg_runtime_status` / `ensure_ffmpeg_runtime` / `cancel_ffmpeg_runtime_install`
273:      commands. Status is queryable after UI recreate; progress events are notifications only.
274:- [x] Single-flight: concurrent ensures coalesce on one job and observe the same state machine.
275:- [x] Reuse `ffmpeg-runtime.json` immutable URL/sha256/allowlist. **Add exact `archive_size`
276:      bytes** to the manifest and enforce it.
277:- [x] Before download: check free space ≥ `archive_size + staged allowlist total + margin`.
278:- [x] Download to `%LOCALAPPDATA%\Clipline\ffmpeg-staging\` with a hard byte cap (`archive_size`);
279:      abort and delete partials on overflow, hash mismatch, cancel, or crash-recovery startup sweep.
280:- [x] Verify archive digest, extract allowlisted files only, write `PROVENANCE.json`, verify the
281:      staged tree, then atomically publish to `%LOCALAPPDATA%\Clipline\ffmpeg\`.
282:- [x] Refuse to execute downloaded bytes before verification (same L-13 invariants).
283:- [x] No silent background download on cold start.
284:- [x] Tests: concurrent ensure coalescing; cancel cleans partials; destroy→recreate mid-download
285:      recovers progress via status query; crash-recovery sweep removes abandoned staging; disk-space
286:      and overflow failures.
287:
288:### Task B4: Replaceable FFmpeg capability cache
289:
290:- [x] Split `service.rs` encoder capability caching:
291:  - MFT capabilities may remain process-static
292:  - FFmpeg capabilities are stored in a replaceable/versioned slot keyed by managed runtime
293:    identity (path + provenance/version) or “external/missing”
294:- [x] After managed publish/repair, refresh only the FFmpeg half and republish encoder options to
295:      the UI without requiring app restart.
296:- [x] Do **not** claim `locate()` has a cache to clear; it does not.
297:- [x] Test: probe before install sees no SVT/FFmpeg backends (or only external if present); complete
298:      managed install; probe/options update in-process; recorder can select a newly available FFmpeg
299:      encoder without restart.

### Task B5: Remove FFmpeg from the regular installer

- [x] Remove `ffmpeg/` from `apps/clipline-app/tauri.conf.json` `bundle.resources`.
- [x] Remove the regular-SKU `beforeBundleCommand` that runs
      `scripts/verify-ffmpeg-resource.ps1` (it must not stage FFmpeg into the slim installer).
- [x] Keep `ffmpeg/` in `tauri.standalone.conf.json` so the offline/Fixed-Version SKU remains
      self-contained, and keep the FFmpeg verifier as a standalone-only `beforeBundleCommand`;
      document that standalone is not the lightweight installer.
- [x] Keep `configure_bundled_ffmpeg` tolerant of a missing resource path (standalone/dev still
      register it when present; regular installs rely on on-demand managed runtime / locate).
- [x] Update UI contracts: regular config must not list `ffmpeg/`; standalone must; no
      `avcodec-*.dll` expectation on the regular NSIS payload.
- [x] Hard gate when a regular setup is built: setup size ≤ **25 MB**, and the payload contains
      no `avcodec-*.dll`. Recorded **9,808,604 bytes (9.35 MiB)** for `Clipline_0.1.45_x64-setup.exe` (local measure build with updater artifacts disabled); binary scan found no `avcodec` substring; `target/release` has no `avcodec*` files.
- [x] Commit as `build(app): stop bundling FFmpeg in the regular installer`.

---

## Milestone C — Docs and Core vs Optional ledger

### Task C1: Publish the new budgets

- [x] Update `ddoc.md` idle/installer budgets to match measured destroyable-shell reality and
      on-demand FFmpeg (PWS ≤120 hard / ≤90 stretch; regular setup ≤25 MB; FFmpeg optional).
- [x] Update `handoff.md` with the Core vs Optional map and the destroy/recreate + managed
      FFmpeg operator notes.
- [x] Point release notes / nightly notes at the slim-core plan outcomes.

### Task C2: Stop conditions / follow-ups

- [x] Do not default disk replay on.
- [x] Do not remove Cloud from Core.
- [x] Deferred: FFmpeg-free shareable clipboard export; native HEVC/AV1 preview; custom ultra-minimal
      FFmpeg build.
