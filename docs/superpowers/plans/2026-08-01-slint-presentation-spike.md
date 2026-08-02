# Slint Milestone 4: Native Presentation Spike

> **For implementers:** Follow plan-driven TDD. Commit this plan before code. Keep every checkbox
> unticked as the repository's historical execution record requires, and commit each logical green
> slice separately.

**Goal:** Prove that Slint 1.17.1 can host Clipline's bounded native playback path in a representative
desktop window without putting decoded 1080p60 frames through the UI scene graph or disturbing the
shipping Tauri application.

**Baseline:** branch `agent/slint-frontend-replacement-plan` at `030babf`. Milestone 3 supplies the
neutral worker, bounded MP4/Annex-B/Opus path, Windows MFT decoder, WASAPI renderer, playback-owned
NV12 textures, deterministic contract tests, and a diagnostic headless harness. Its absolute and
matched hardware gates remain pending because the only formal run was noise-rejected and this
console session exposes Microsoft Basic Display Adapter.

**Non-goals:** This is not a distributed executable, a shell cutover, or a Library/Cloud/Settings
port. It does not alter Tauri commands, WebView assets, installer manifests, product identity,
autostart, updater, registry state, or the user's running Clipline. Non-1x tempo processing remains
an explicit Milestone 9/final-parity requirement.

## Fixed architecture

- Pin `slint` and `slint-build` to exactly `1.17.1`. Keep the spike as a workspace-excluded,
  standalone Cargo package with its own committed lockfile: Slint's Parley stack requires ICU
  2.1+ while the shipping app's Boa 0.21.1 tests require ICU 2.0.x, and Cargo cannot unify those
  same-major requirements in one resolve. Disable default features and enable only
  `std`, `compat-1-2`, `accessibility`, `backend-winit`, `renderer-software`, `system-tray`, and
  `raw-window-handle-06`. Qt, Skia, live preview, MCP, and system-testing are excluded. A later
  opt-in comparison may enable `renderer-femtovg-wgpu`; it is not part of the default binary.
- Slint owns the top-level winit window and all controls. A safe Windows wrapper creates one
  non-activating child HWND exactly over the reserved video-stage rectangle. The child owns a
  two-buffer DXGI flip-model swap chain; Slint controls never overlap it.
- The playback thread owns indexing, `PlaybackWorker`, MFT, WASAPI, scheduling, and the D3D frame
  publisher. The Slint event-loop thread owns component/model mutation and the child-window host.
  UI commands cross a bounded transport; snapshots/events return through
  `slint::invoke_from_event_loop` using weak component handles.
- The D3D publisher obtains the decoder texture's device, converts playback-owned NV12 into the
  swap-chain back buffer with the D3D11 video processor, presents, then releases the frame. It
  retains no decoder surface after `publish` returns. Resize, occlusion, and device loss are typed
  state transitions, never unbounded retry loops.
- A latest-only CPU readback plus `SharedPixelBuffer` publisher is diagnostic fallback evidence,
  not the production fast path. At most one converted RGB frame may await the UI thread.
- All new Win32/DXGI/D3D11 calls and first-party `unsafe` stay below
  `crates/clipline-playback/src/windows/`. The spike calls safe wrappers using the raw HWND value
  obtained from Slint's `raw-window-handle-06` API.

## Task 1: Freeze package, feature, and UI contracts

**Files:**

- Modify: `Cargo.toml`
- Create: `apps/clipline-slint-spike/Cargo.toml`
- Create: `apps/clipline-slint-spike/build.rs`
- Create: `apps/clipline-slint-spike/src/lib.rs`
- Create: `apps/clipline-slint-spike/src/main.rs`
- Create: `apps/clipline-slint-spike/tests/spike_contract.rs`
- Create: `apps/clipline-slint-spike/ui/app.slint`

- [ ] Add a failing repository-contract test first. It requires the exact Slint pins and allowlisted
      features, rejects Tauri/WebView/Qt/Skia/GStreamer/linked-FFmpeg dependencies, verifies that the
      spike is explicitly excluded into its own workspace and absent from both shipping bundle
      manifests, and scans for first-party `unsafe` outside the playback crate's Windows module.
- [ ] Add the non-distributed package with `clipline-playback`, `clipline-mp4`, `serde`, and
      `serde_json`. Keep `raw-window-handle = "0.6"` direct and version-aligned with Slint. Commit
      the standalone lockfile and run it explicitly; root workspace gates deliberately exclude it.
- [ ] Make `slint-build` compile the single root UI file. The neutral library and compile-contract
      test must build on Ubuntu without instantiating a window; all Windows integration stays under
      `#[cfg(windows)]`.
- [ ] Implement the smallest window needed to make the contract green, then run the package tests
      and warning-denied Clippy before committing.

## Task 2: Add bounded presentation models and representative static UI

**Files:**

- Create: `apps/clipline-slint-spike/src/model.rs`
- Create: `apps/clipline-slint-spike/tests/presentation_model.rs`
- Modify: `apps/clipline-slint-spike/ui/app.slint`

- [ ] Write failing pure-Rust tests for exactly 24 visible Library rows, bounded poster/model data,
      stable marker ordering, selected-track state, transport labels, time formatting, and
      Library/Review view switching. No filesystem or Slint event loop belongs in these tests.
- [ ] Build a fixed 1200x760 production-colored shell using the current CSS color/font tokens. Show
      24 representative Library rows and a Review screen with a reserved 16:9 video stage, marker
      timeline, play/pause, seek, two audio-track toggles, volume, 1x rate label, and status/error
      text. Do not add Cloud or Settings behavior.
- [ ] Name interactive controls and set accessible role/label/value properties. Keyboard focus must
      remain visible and the static UI must compile at 100%, 125%, 150%, and 200% scale inputs.
- [ ] Add deterministic component-contract assertions for required callbacks/properties and commit
      only after the model tests, Slint compile, and Clippy pass.

## Task 3: Extract a reusable bounded live-session executor

**Files:**

- Create: `crates/clipline-playback/src/windows/session.rs`
- Modify: `crates/clipline-playback/src/windows/mod.rs`
- Modify: `crates/clipline-playback/src/sample_buffer.rs`
- Modify: `crates/clipline-playback/src/worker.rs`
- Modify: `crates/clipline-playback/src/state.rs`
- Modify: `crates/clipline-playback/examples/headless_playback.rs`
- Modify: `crates/clipline-playback/tests/annexb.rs`
- Modify: `crates/clipline-playback/tests/lifecycle.rs`
- Create: `crates/clipline-playback/tests/windows_session.rs`
- Modify: `crates/clipline-playback/tests/fixture_playback.rs`

- [ ] Before extraction, close the audited worker seams with failing neutral tests. Add an opaque,
      generation-fenced converted-video handle so Read/Convert/Decode remain distinct actions;
      install bounded default audio tracks as part of successful indexing; accept token-fenced
      steady-state backend failures into the existing recovery budget; and resolve signed Step to
      an exact clamped source-sample target. A live executor must not merely bypass the worker while
      claiming it as its authority.
- [ ] Write failing fake-backend tests proving that `PlaybackWorker` remains the authority for
      open/play/pause/seek/track/volume/close, only the final generation publishes, a full command
      inbox rejects non-fence work without losing Close, and every terminal path releases the file,
      pending frame, audio queue, decoder, and endpoint.
- [ ] Extract the reusable indexed read/decode/mix/schedule loop from the headless example into a
      Windows session executor parameterized by `FramePublisher<D3D11VideoSurface>`. The example
      becomes a thin scenario/telemetry client; do not fork a second playback algorithm.
- [ ] Give the executor one bounded command port with last-writer-wins transport/volume intent,
      coalesced seeks, ordered Open/Close fences, and typed full/disconnected results. It may sleep
      only to a bounded next-deadline and must observe Close promptly.
- [ ] Expose revisioned, owned snapshots/events and the existing honest metrics through a bounded
      non-blocking update sink. The session never executes an arbitrary user callback; panic
      isolation belongs in the later Slint event-loop adapter. Updates carry a monotonic sequence
      and exact pipeline token and may never retain a decoded frame.
- [ ] Re-run every Milestone 3 deterministic/live test and confirm the headless telemetry schema and
      fixture results remain compatible before committing the extraction.

## Task 4: Add neutral video-stage geometry and lifecycle

**Files:**

- Create: `crates/clipline-playback/src/presentation.rs`
- Modify: `crates/clipline-playback/src/lib.rs`
- Create: `crates/clipline-playback/tests/presentation.rs`

- [ ] Write failing table tests for logical-to-physical rectangle conversion at 100/125/150/200%,
      fractional rounding without gaps, zero/minimized bounds, 16:9 letterboxing, resize storms,
      fullscreen transitions, occlusion, and monotonic geometry revisions.
- [ ] Add a neutral `VideoStageGeometry`/`PresentationLifecycle` contract. Repeated identical updates
      are no-ops; zero-area/minimized/occluded state releases pending presentation work; restore
      requires the newest revision before publication.
- [ ] Bound pending geometry to one latest value and expose telemetry for resize/recreate/present/
      occlusion/device-loss counts. No OS or Slint types enter the neutral module.
- [ ] Run neutral tests on Windows and under the CI environment before committing.

## Task 5: Add the safe child-window and D3D11 publisher

**Files:**

- Create: `crates/clipline-playback/src/windows/presenter.rs`
- Modify: `crates/clipline-playback/src/windows/mod.rs`
- Modify: `crates/clipline-playback/Cargo.toml`
- Create: `crates/clipline-playback/tests/windows_presenter.rs`
- Modify: `apps/clipline-app/tests/repository_security.rs`

- [ ] Add pure validation/classification tests first, followed by device tests that self-skip only
      with a recorded reason. Cover invalid parent handles/bounds, child creation and destruction,
      two-buffer swap-chain bounds, repeated resize, occlusion, minimize/restore, stale geometry,
      source/destination aspect rectangles, `Present` outcomes, and DXGI device removal/reset/hung.
- [ ] Split responsibilities: a thread-affine `WindowsVideoHost` owns the non-activating child HWND;
      a playback-thread `WindowsD3D11Publisher` owns the swap chain/video processor for that HWND.
      Destroy the publisher before the host and make both close operations idempotent.
- [ ] Lazy-create the swap chain from the first frame's D3D device, use BGRA8 flip-discard buffers,
      clear letterbox bars, apply explicit Rec.709 limited-range NV12 color space, and present with
      bounded non-blocking behavior. Query the current child client rect before publication and
      resize only when its revision changes.
- [ ] Consume each move-only surface inside `publish`; no decoder surface survives the return edge.
      Reject mixed adapters/devices and stale revisions. Map occlusion to a paused presentation state
      and device loss to `RecreateComponent`, with a bounded retry budget owned by the session.
- [ ] Extend the security scan for unsafe/import confinement. Run the real device suite, fresh-cache
      playback Clippy, and an ownership audit before committing.

## Task 6: Add the diagnostic `SharedPixelBuffer` path

**Files:**

- Modify: `crates/clipline-playback/src/windows/presenter.rs`
- Create: `apps/clipline-slint-spike/src/cpu_frame.rs`
- Modify: `apps/clipline-slint-spike/ui/app.slint`
- Modify: `apps/clipline-slint-spike/tests/presentation_model.rs`

- [ ] Write failing tests for NV12 crop/color conversion vectors, odd/malformed dimensions,
      latest-only replacement, stale-token rejection, and a hard maximum of one pending RGB frame.
- [ ] Add a safe, bounded GPU-readback/CPU-conversion helper under the Windows module. It fills one
      caller-owned RGB buffer and reports readback/copy time; it never allocates per frame after the
      accepted dimensions are configured.
- [ ] Convert only the newest diagnostic frame into `SharedPixelBuffer<Rgb8Pixel>` and update the
      Slint image property on the event-loop thread through a weak handle. Dropping/replacing the
      mailbox item must immediately release its source surface.
- [ ] Label the active path in the UI and telemetry. The default/gate path remains D3D; automatic
      CPU fallback requires an explicit command-line flag so a D3D failure cannot masquerade as a
      fast-path pass.

## Task 7: Wire the spike and exercise window lifecycle

**Files:**

- Create: `apps/clipline-slint-spike/src/controller.rs`
- Create: `apps/clipline-slint-spike/src/windows.rs`
- Modify: `apps/clipline-slint-spike/src/main.rs`
- Modify: `apps/clipline-slint-spike/ui/app.slint`
- Create: `apps/clipline-slint-spike/tests/controller.rs`

- [ ] Add failing controller tests for callback-to-command mapping, revisioned snapshot delivery,
      stale UI update rejection, open/play/pause/seek/track/volume/close, error display, and shutdown
      ordering. Use a fake command port; UI tests never require media devices.
- [ ] After `show()` and at least one event-loop turn, obtain Slint's Win32 raw handle and attach the
      host. Feed newest logical stage geometry and `Window::scale_factor()` to the host without a
      polling thread or per-frame allocation.
- [ ] Run the reusable session on one named playback thread. Marshal snapshots/events to Slint with
      `invoke_from_event_loop`, weak handles, and generation checks. Closing the window first closes
      the session/publisher, then destroys the video host, then drops Slint components.
- [ ] Wire the representative controls and minimal spike-only tray Show/Quit actions. Tray behavior
      proves the feature/event-loop combination only and is not credited as Milestone 6 parity.
- [ ] Add command-line `--fixture`, `--renderer`, `--cpu-frame-diagnostic`, `--scenario`, and
      `--marker-path` inputs. The marker protocol must match `docs/slint/baseline-protocol.md` and
      never modify user settings, autostart, or installed Clipline state.
- [ ] Manually exercise move, continuous resize, 100/125/150/200% DPI, minimize/restore,
      fullscreen, occlusion, seek storms, track changes, endpoint recreation, close during seek,
      and 100 reveal/close cycles. Record SKIP rather than PASS for unavailable hardware injection.

## Task 8: Measure both paths and make the spike decision

**Files:**

- Create: `scripts/drive-slint-spike.ps1`
- Modify: `scripts/measure-frontend-baseline.ps1` only if a frontend-neutral contract defect is found
- Create: `docs/slint/slint-presentation-protocol.md`

- [ ] Extend the existing sampler through its Slint adapter/marker seam. Record exact Slint version,
      feature set, renderer, presentation path, GPU/driver/adapter LUID, decoder path, endpoint,
      fixture hash, executable hash, and per-process PWS/private-commit/CPU/handles/threads/GPU data.
- [ ] Run short correctness smokes with `winit-software` plus D3D child presentation and with the
      explicit CPU diagnostic. Compare one opt-in Direct3D-capable Slint renderer only after the D3D
      child path is green; never enable Qt or Skia merely to obtain a comparison.
- [ ] On a quiet real-GPU console session, run at least three five-minute review-playing and matched
      seek-storm samples, the lifecycle/DPI matrix, 100 cycles, and the Milestone 3 hardware media
      gates. Alternate matched Tauri/Slint order when the unrelated Clipline process can be closed.
- [ ] Gate the spike on the program's absolute 190 MiB PWS ceiling, 40 ms p95 A/V error, below 0.5%
      late/drop, 150 ms p95 exact seek settle, fixed queue/surface bounds, no handle/thread/capacity
      growth, reliable teardown, and functional device recovery. The 80%-of-Tauri PWS and within-one-
      percentage-point CPU gates remain pending until matched Tauri evidence exists.
- [ ] The current Microsoft Basic Display Adapter may validate software fallback and neutral/UI
      lifecycle only. It cannot pass D3D fast-path, hardware decode, driver reliability, or matched
      renderer gates. Record those as pending unless a real adapter becomes available; do not call
      missing evidence a pass or a failure.
- [ ] If correctness, bounds, teardown, or the absolute memory gate fails, stop and archive the spike
      before broad UI work. If implementation is green and only environment/matched evidence is
      unavailable, keep it non-distributed and explicitly pending while proceeding with reversible
      controller extraction.

## Task 9: Verify, review, commit, and hand off

**Files:**

- Modify: `handoff.md`
- Modify: `docs/slint/parity-ledger.md` only for behavior actually verified

- [ ] Run the spike package through `--manifest-path apps/clipline-slint-spike/Cargo.toml`, then
      playback, MP4, security, and migration-contract tests; then
      `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Run fresh-cache Clippy for both changed packages. Audit `cargo tree` and source for the exact
      Slint pin/features, no Qt/Skia/Tauri/WebView/linked FFmpeg/GStreamer/GPL runtime, and no new
      unsafe outside Windows safe-wrapper modules.
- [ ] Have an independent reviewer audit UI-thread boundaries, generation/cancellation, child-HWND
      lifetime, device identity, texture ownership, swap-chain bounds, resize/DPI/occlusion behavior,
      CPU mailbox bounds, recovery, and the honesty of recorded evidence.
- [ ] Update the handoff with stable APIs, exact caps, renderer/presentation selections, fixture and
      executable hashes, results, SKIPs, pending matched gates, and the Milestone 5 controller entry
      point. Do not advance full Review or tray parity for spike-only controls.
- [ ] Open or refresh the draft PR so Ubuntu and Windows CI run; ordinary branch pushes do not
      trigger this repository's workflow. Push every green logical commit after remote authorization.

## Primary implementation references

- Slint 1.17.1 Cargo features:
  <https://docs.slint.dev/latest/docs/rust/slint/docs/cargo_features/>
- Slint `Window`, raw handle, scale, fullscreen, and rendering notifier:
  <https://docs.slint.dev/latest/docs/rust/slint/struct.Window>
- Slint winit renderer selection:
  <https://docs.slint.dev/latest/docs/slint/guide/backends-and-renderers/backend_winit/>
- Slint `SystemTrayIcon`:
  <https://docs.slint.dev/latest/docs/slint/reference/window/systemtrayicon/>
- DXGI flip-model swap chains and D3D11 video processor APIs:
  <https://learn.microsoft.com/windows/win32/direct3ddxgi/dxgi-flip-model>
  and <https://learn.microsoft.com/windows/win32/api/d3d11/nf-d3d11-id3d11videocontext-videoprocessorblt>
