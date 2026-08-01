# Slint Frontend Replacement Program

> **For agentic workers:** Execute this program one milestone at a time. Each implementation
> milestone gets its own narrower plan and a separate plan commit before code. Checkboxes remain
> unticked by repository convention.

**Planning baseline:** `origin/develop` at `5eea6c3` (Clipline 0.1.43). Refresh `develop` before
starting each implementation milestone so the extraction work follows the current shipping code.

**Goal:** Replace the Tauri/WebView2 desktop frontend with Slint while preserving Clipline's
recorder, library, cloud, editor, Windows integration, update security, and file-format behavior.
The rewrite is successful only if it materially lowers the always-open process-tree memory and
does not trade that saving for playback copies, CPU load, regressions, or a weaker license boundary.

**Non-goals:** Rewriting the capture/encode pipeline, changing settings or media locations,
redesigning the product, linking FFmpeg libraries, adopting GPL code, adding macOS/Linux capture,
or changing the cloud protocol.

## Decision summary

Proceed as a gated parallel implementation, not a big-bang rewrite. Prove native playback and
rendering first. Keep the existing Tauri app as the shipping implementation until the Slint binary
passes the media, memory, shell, packaging, and parity gates below.

Slint 1.17.1 is the initial pinned evaluation version. It supplies a Windows-capable winit backend,
software and GPU renderers, raw window-handle access, accessibility properties, and first-party
`SystemTrayIcon` support. It does **not** supply a production Windows video player or a supported
one-call D3D11 texture-to-`Image` bridge. Slint's official FFmpeg example converts every decoded
frame to a CPU RGB buffer and marks an OpenGL bridge as TODO; its GStreamer example uses CPU-accessible
buffers on Windows. Neither is an acceptable production media architecture for Clipline.

Use Slint under the royalty-free desktop license and add the required attribution before any
distributed build. Do not change Clipline's MIT OR Apache-2.0 first-party license and do not select
Slint's GPL distribution option.

## Current replacement surface

The current frontend is not merely markup. The migration covers:

- roughly 600 KiB of HTML, CSS, and JavaScript across Library, Cloud, Review, Settings, and Support;
- 60 Tauri commands and the `status`, `saved`, `error`, microphone, lifecycle, game-detection,
  enrichment, and upload-progress event streams;
- 225 frontend behavior tests, including the 86 KiB DOM-free `player-core.js` and large DOM
  contract suite;
- native `<video>` playback, codec probing, six playback rates, accurate seeking/frame stepping,
  full screen, volume, and separately synchronized Opus sidecars;
- tray, close-to-tray, taskbar lifecycle, global keyboard/mouse hotkeys, autostart, single-instance
  activation, native dialogs, updater, NSIS packaging, and signed-update verification;
- local/cloud gallery paging, posters, upload state, settings drafts, display-region editing,
  game detection, microphone monitoring, diagnostics, and support-report submission.

## Target architecture

```text
main thread
  Slint event loop
    SystemTrayIcon (always alive)
    MainWindow (created lazily, dropped on tray close)
    UI models/callbacks (presentation only)
          |
          v
framework-neutral DesktopController
  UiAction requests  <---------------- Slint callbacks
  UiEvent snapshots  ----------------> Weak::upgrade_in_event_loop
          |
          +-- existing recorder service / capture / game detector
          +-- existing library, cloud, settings, support, and updater use-cases
          +-- native playback worker
                clipline-mp4 indexed demux
                Media Foundation video decode
                shiguredo_opus decode + bounded mix/tempo stage
                WASAPI playback clock
                D3D11 presentation surface

Windows shell modules
  Slint tray | global hotkey | autostart | single instance | installer/updater helper
```

The Slint thread owns every component and `ModelRc`. Existing recorder/network/file work remains on
workers. Workers send small owned `UiEvent` values through one bounded/coalescing event sink; the
Slint adapter uses `Weak::upgrade_in_event_loop` to update models. No capture, decode, filesystem,
HTTP, or poster generation may block the UI thread.

The durable application state belongs to `DesktopController`, not the Slint component. Destroying
and recreating `MainWindow` must preserve the selected page, settings draft policy, active upload,
and recorder state without keeping decoded images or playback resources alive.

## Program gates

Measure the Tauri and Slint builds on the same machine, settings, media library, clip, renderer,
window state, and sampling tool. Record process-tree private working set, private commit, ordinary
working set, GPU local/non-local allocation, CPU, handles, and first-usable latency. Never compare
Task Manager's grouped headline with a single-process counter.

The program may proceed beyond the media spike only when all of these hold:

- **Open Library:** five-minute median process-tree private working set is at most 140 MiB and at
  most 65% of the matched Tauri baseline. Private commit also falls by at least 25%.
- **Tray/autostart:** no `MainWindow` or renderer is created until requested; recording, hotkeys,
  saves, game detection, and cloud completion remain live. Repeated open/close does not retain more
  than 10 MiB after 100 cycles.
- **H.264 review:** Clipline-authored H.264 High + Opus MP4 plays at 1080p60 with selected/mixed
  audio, p95 A/V error at or below 40 ms, fewer than 0.5% late/dropped presented frames, and CPU no
  worse than the matched WebView2 run by more than one percentage point.
- **Review memory:** five-minute playback private working set is at most 190 MiB and at most 80% of
  the matched WebView2 review baseline. Decode and presentation queues stay bounded.
- **Seeking:** p95 pointer-release-to-correct-frame is at most 150 ms; displayed time is within one
  source frame; stale seeks are canceled/coalesced and never replay later.
- **Lifecycle:** closing Review releases the file, decoder, audio endpoint, D3D surfaces, posters,
  and worker threads. Tray save and reveal work through 100 cycles without stale UI events.
- **Compliance:** no linked FFmpeg, GStreamer runtime, GPL component, or unreviewed native codec
  DLL. FFmpeg remains a separately spawned LGPL executable for the existing encode/export paths.

If the media gate fails, stop the full rewrite. A Slint Library/Settings shell with an on-demand,
out-of-process WebView review helper may be evaluated separately, but it is a different product
architecture and must not be smuggled in as a temporary assumption.

## Milestone 1: Freeze a matched baseline and parity ledger

- [ ] Branch from current `develop`; record the exact commit, Slint version, renderer feature set,
      Windows version, GPU/driver, display scale, and test media hashes.
- [ ] Extend the existing memory harness rather than inventing a new metric. Add scenarios for cold
      autostart tray, open Local Library, 50/500/2,000-clip libraries, Settings, H.264 review idle,
      H.264 review playing, scrub storm, close-to-tray, and 100 reveal/close cycles.
- [ ] Record private working set and private commit separately for root and descendants, plus GPU
      allocation, CPU, handles, and first-usable latency.
- [ ] Create first-party deterministic fixtures: H.264 High with one Opus track, H.264 High with
      output+microphone tracks and markers, a long-GOP clip, a variable-frame-content clip, and
      optional HEVC/AV1 capability fixtures. Keep fixture media small and license-clean.
- [ ] Create a parity ledger for every Tauri command, emitted event, page, dialog, shortcut, tray
      action, updater action, and review-player gesture. Give each row an owner milestone and an
      automated/manual acceptance test.
- [ ] Capture reference screenshots and keyboard flows at 100%, 125%, 150%, and 200% scaling.
- [ ] Commit the baseline report and parity ledger before adding Slint dependencies.

## Milestone 2: Add a read-only playback index to `clipline-mp4`

- [ ] Add failing tests for a public, bounded file-backed movie index exposing track codec/config,
      dimensions, timescale, duration, ordered sample offsets/sizes, DTS/PTS, duration, and sync
      sample flags without loading `mdat` payloads.
- [ ] Reuse the hardened parser currently private to `trim.rs`; do not build a second MP4 parser.
- [ ] Add a seek query returning the video sync sample at-or-before a requested time and the audio
      sample ranges needed to restart each selected track.
- [ ] Expose H.264 SPS/PPS and length-size data needed to convert Clipline's length-prefixed MP4
      samples back to Annex B. Add equivalent typed configuration for HEVC/AV1 without promising
      decoder availability.
- [ ] Reject truncated, overlapping, out-of-file, oversized, and malformed tables before playback.
      Preserve all existing untrusted-input limits.
- [ ] Add streaming sample reads with one caller-owned bounded buffer; no whole-file or whole-track
      materialization.
- [ ] Run `cargo test -p clipline-mp4` and fresh-cache Clippy.

## Milestone 3: Prove headless native H.264 + multi-track Opus playback

- [ ] Add `crates/clipline-playback`, neutral state-machine tests first, with `windows/` containing
      all Media Foundation, WASAPI, D3D11, and unsafe code behind safe wrappers.
- [ ] Define `PlaybackCommand::{Open, Play, Pause, Seek, Step, SetTracks, SetVolume, SetRate, Close}`
      and coalescing semantics. A newer open/seek generation invalidates every older completion.
- [ ] Feed Annex-B H.264 samples to the Windows H.264 decoder MFT. Configure the existing protected
      D3D11 device through `IMFDXGIDeviceManager`; request NV12 surfaces and keep software fallback
      explicit and measurable.
- [ ] Extract/reuse the existing `shiguredo_opus` decode and mixing logic. Decode only a bounded
      packet window per selected track and mix into one bounded stereo queue.
- [ ] Use an audio-device-backed monotonic playback clock. Present the newest video frame due at
      that clock and drop superseded frames rather than growing a queue.
- [ ] Seek by flushing the decoder, restarting at the prior sync sample, and decoding/discarding to
      the target. Prove cancellation under rapid alternating seeks.
- [ ] Initially gate correctness at 1x. Before final parity, support 0.5/0.75/1/1.25/1.5/2x with a
      bounded pitch-preserving tempo stage, or obtain an explicit product decision changing the
      current behavior. Silent or pitch-shifted non-1x playback is not implicit parity.
- [ ] Add deterministic tests for end-of-audio video tails, delayed/gapped tracks, track switching,
      pause/resume, EOF, short clips, corrupt packets, device loss, and close during seek.
- [ ] Run live 1080p60 H.264/Opus playback and the A/V, seek, CPU, memory, and handle gates.

## Milestone 4: Prove Slint presentation without broad UI work

- [ ] Add a non-distributed `apps/clipline-slint-spike` package pinned to Slint 1.17.1 with only the
      required winit, software renderer, system tray, and raw-window-handle features. Do not enable
      Qt or Skia by default.
- [ ] Build a representative 1200x760 Library shell and one Review screen using production colors,
      fonts, 24 visible rows, posters, timeline markers, and transport controls.
- [ ] Keep a CPU `SharedPixelBuffer` frame path only as a correctness/fallback diagnostic. Slint's
      own example copies RGB frames this way; it is not the production fast path.
- [ ] Use Slint's raw Windows handle to prototype a bounded D3D11 presentation surface for the
      reserved video-stage rectangle. Validate move, resize, DPI changes, minimize, fullscreen,
      occlusion, device loss, and teardown. Keep Slint controls outside the native child surface;
      draw any required in-video overlay in the video renderer.
- [ ] Compare `winit-software` with one Direct3D-capable Slint renderer only after the D3D surface
      works. Select on measured RAM, CPU, frame pacing, text quality, and driver reliability—not
      the framework default.
- [ ] Implement the marker timeline, play/pause, seek, selected audio tracks, volume, and 1x clock
      in the spike; do not port Cloud or Settings yet.
- [ ] Run every program gate. Record raw samples and a written go/no-go decision.
- [ ] Stop here and archive the spike if the gates fail.

## Milestone 5: Extract a framework-neutral desktop controller

- [ ] Add failing tests for typed `UiAction`, `UiEvent`, and complete bootstrap snapshots. Events
      must be bounded/coalesced and carry generation identifiers where stale completion matters.
- [ ] Move command bodies out of Tauri entry points a vertical slice at a time. Tauri commands
      become thin argument/state adapters; they do not remain the application API.
- [ ] Replace direct `AppHandle::emit` dependencies in recorder status, save/error reporting,
      microphone monitoring, game detection, enrichment, and cloud upload with an injected
      `UiEventSink`.
- [ ] Keep persisted `AppSettings`, media paths, credential targets, cloud records, and service
      commands unchanged.
- [ ] Make `DesktopController::snapshot()` sufficient to rebuild a destroyed window without
      replaying old events.
- [ ] Keep the Tauri app behavior and all existing tests green after every extraction batch.
- [ ] Add a Slint adapter that updates `ModelRc` values only on the event-loop thread via weak
      component handles. Never capture a strong component in its own callback.

## Milestone 6: Replace the native shell dependencies

- [ ] Use Slint 1.17's `SystemTrayIcon` for Open, Save Replay, Diagnostics, and Quit. Explicitly
      synchronize tray properties because tray and window instances do not share globals.
- [ ] Start autostart in tray-only mode without constructing `MainWindow`; Slint documents that a
      visible tray icon can keep the event loop alive without a window adapter.
- [ ] Create `MainWindow` lazily on activation and drop it on tray close after publishing durable
      state. Reopen from a controller snapshot.
- [ ] Replace the Tauri global-shortcut plugin with a safe Windows wrapper around the current
      keyboard/mouse hotkey behavior. Preserve elevated-game diagnostics and rollback semantics.
- [ ] Replace autostart with a transactional per-user Windows implementation preserving
      `--autostart` and release/debug separation.
- [ ] Replace single-instance handling with a named per-user instance guard plus a bounded local
      activation channel that can reveal a tray-only primary instance. Authenticate the peer as
      the same user and reject oversized/malformed messages.
- [ ] Preserve native folder pickers, Explorer reveal, clipboard, Credential Manager, and browser
      opening through existing safe Windows/application helpers.
- [ ] Build a signed-update helper that consumes the existing HTTPS manifest, verifies the existing
      embedded public-key signature before launch, starts the passive installer, and exits cleanly.
      Preserve cancellation, channel selection, diagnostics, and rollback behavior.
- [ ] Prove an NSIS/standalone packaging path with the same product identity, install scope,
      resources, shortcuts, settings, and upgrade behavior before removing Tauri bundling.
- [ ] Test install, update, downgrade refusal, tampered manifest/artifact, offline failure,
      autostart, secondary launch, tray-only update, and uninstall on Windows 10 and 11.

## Milestone 7: Port Library and Cloud

- [ ] Port gallery/window/cloud core behavior to Rust using the existing fixtures before deleting
      any JavaScript tests. Preserve Windows path identity and stale-request generation rules.
- [ ] Model only the active bounded page/window in Slint. Keep selection and active-clip identity in
      the controller, not in row instances.
- [ ] Load poster files directly as bounded native images; do not convert them to base64/data URLs.
      Release images for rows outside the active window and preserve the backend extraction
      semaphore/cache bounds.
- [ ] Reproduce Local/Cloud tabs, filtering, grouping, pagination, multi-select, context menus,
      rename/delete, folder reveal, upload badges/progress, public share links, and cloud-media open.
- [ ] Preserve upload completion, Windows-equivalent path reconciliation, deferred foreground
      feedback, and active-file lease behavior from 0.1.43.
- [ ] Add presentation-model tests, `.slint` compile checks, deterministic snapshots, keyboard
      navigation tests, and manual Narrator/UI Automation checks.
- [ ] Meet the 50/500/2,000-clip memory, latency, and poster-process gates.

## Milestone 8: Port Settings, Games, microphone test, and Support

- [ ] Port settings validation/normalization completely to Rust before wiring controls. Slint holds
      a view of the draft; the controller owns baseline, dirty state, save transaction, and rollback.
- [ ] Reproduce Capture, Recording, Storage, Hotkeys, Games, Cloud, and Support surfaces, including
      display-region map manipulation, encoder probing, device lists, custom-game detection,
      recording-mode controls, folder pickers, account flows, and diagnostics submission.
- [ ] Replace Web Audio microphone monitoring with the existing native monitor samples and a
      bounded native playback/level path. Background entry must stop it synchronously.
- [ ] Replace WebView `canPlayType` reporting with `clipline-playback` capability probing. Automatic
      stays H.264-first; explicit HEVC/AV1 remain marked limited until native decoding passes the
      same gates on supported machines.
- [ ] Preserve settings transaction ordering for hotkeys, tray label, autostart, persistence, and
      recorder restart, including complete compensation on failure.
- [ ] Give every custom control an accessibility role, name, value, checked/expanded state, and
      keyboard operation. Verify focus order and high-DPI layout.

## Milestone 9: Port the complete Review/editor experience

- [ ] Port `player-core.js` behavior to a framework-neutral Rust review model using the existing
      test vectors: trim bounds, nice ruler steps, marker filtering/navigation, zoom/pan, frame
      stepping, keyboard parsing, overlay state, audio selection, and presentation summaries.
- [ ] Replace tests only after equivalent Rust assertions pass. Keep a cross-implementation fixture
      runner during the transition to detect semantic drift.
- [ ] Reproduce timeline zoom/scrub, draggable trim handles, marker ticks/chips, game event/play
      rails, metadata, title/file rename, delete, reveal, copy/share, export, upload, full screen,
      volume/mute, six playback rates, and all documented shortcuts.
- [ ] Preserve selected/mixed audio semantics without sidecar files in the native player. Existing
      sidecar generation may remain only for external share/export compatibility where needed.
- [ ] Ensure trim/export remains keyframe-aligned and continues using bounded streaming writers;
      the UI rewrite must not alter media output bytes except where an existing test permits it.
- [ ] Test file replacement/rename while paused and playing, selected-track switches, clips whose
      audio ends early, rapid navigation, corrupt media, decoder/device loss, and background entry.
- [ ] Run the full media, memory, CPU, seek, lifecycle, accessibility, and keyboard gates.

## Milestone 10: Cut over, package, and remove WebView2

- [ ] Run a Slint nightly channel first. Prevent simultaneous Tauri/Slint writers against one
      settings profile and keep a documented one-command rollback build during evaluation.
- [ ] Exercise real recording, Save Replay, full-session recording, League/osu! enrichment,
      Library/Cloud, update, restart, autostart, and 24-hour tray/open soaks on supported Windows.
- [ ] Add Slint attribution in an About surface or the project download webpage exactly as required
      by the selected royalty-free license; record the chosen license version in third-party notices.
- [ ] Switch the production binary/package only after parity-ledger sign-off and installer/update
      smoke tests. Preserve `io.clipline.app`, settings, credentials, media, updater channel, and
      user shortcuts.
- [ ] Remove Tauri, WebView2 COM pins, asset protocol/capabilities, HTML/CSS/JS, Boa, DOM contracts,
      bundled WebView2 runtime, and repair diagnostics only after the corresponding native paths
      are proven and shipped.
- [ ] Update `ddoc.md`, `handoff.md`, security/repository checks, installer manifests, license
      notices, and quick-reference commands.
- [ ] Run `cargo test --workspace`, fresh-cache changed-crate Clippy, then
      `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Publish final before/after raw measurements for tray, open Library, Settings, H.264 review,
      scrub storm, large library, repeated Save Replay, and reveal/close soak.

## Recommended first implementation slice

Do Milestones 1–4 only. The first user-visible UI port is deliberately postponed until Clipline can
play one of its own H.264 + two-track Opus files through the proposed native path, seek it accurately,
present it without an unbounded CPU RGB-copy pipeline, and pass the memory/CPU gates. That slice
retires the largest uncertainty while touching none of the shipping Tauri behavior.

## Primary references checked for this plan

- Slint 1.17 release and system tray: <https://slint.dev/blog/slint-1.17-released>
- Slint `SystemTrayIcon`: <https://docs.slint.dev/latest/docs/slint/reference/window/systemtrayicon/>
- Slint winit renderers: <https://docs.slint.dev/latest/docs/slint/guide/backends-and-renderers/backend_winit/>
- Slint window/raw handle/event-loop API: <https://docs.slint.dev/latest/docs/rust/slint/struct.Window>
- Slint models and worker-thread updates: <https://docs.slint.dev/latest/docs/rust/slint/struct.ModelRc>
- Slint accessibility properties: <https://docs.slint.dev/latest/docs/slint/reference/common/>
- Slint royalty-free desktop license: <https://slint.dev/agreements/slint-royalty-free-license.pdf>
- Slint FFmpeg example (CPU RGB frame transfer): <https://github.com/slint-ui/slint/tree/master/examples/ffmpeg>
- Slint GStreamer example (Windows CPU transfer): <https://github.com/slint-ui/slint/tree/master/examples/gstreamer-player>
- Microsoft H.264 decoder MFT: <https://learn.microsoft.com/en-us/windows/win32/medfound/h-264-video-decoder>
- Microsoft D3D11 Media Foundation decode integration:
  <https://learn.microsoft.com/en-us/windows/win32/medfound/supporting-direct3d-11-video-decoding-in-media-foundation>
