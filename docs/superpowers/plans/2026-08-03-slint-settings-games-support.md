# Slint Milestone 8: Native Settings, Games, Microphone, and Support

> **For agentic workers:** Execute one task at a time with failing tests first. Commit this plan
> before implementation. Keep checkboxes unticked by repository convention.

**Program milestone:** Milestone 8 of `2026-08-01-slint-frontend-replacement.md`.

**Goal:** Move Clipline's Settings, recorder/runtime configuration, device and codec probes, Games,
osu! account work, microphone test, Cloud account editor, and Support workflow behind bounded,
framework-neutral Rust services, then present the complete experience through the native Slint
candidate. Keep the shipping Tauri frontend behaviorally compatible and rollback-capable until the
final Milestone 10 cutover.

**Baseline:** branch `agent/slint-frontend-replacement-plan` after Milestone 7 implementation
closeout `154f84b`. Milestone 7 acceptance remains NO-GO pending protocol-accepted quiet-host
evidence; that missing environmental evidence does not authorize parity or cutover, but no evaluated
absolute gate failed and engineering may continue. The user's installed Clipline PID 5548 remains
open. Never stop it, mutate its settings/credentials/media/Run entry, submit a real support report,
or install an internal candidate.

**Non-goals:** Porting the complete Review/editor, timeline, export, clipboard, or playback-rate UI
(Milestone 9); switching the production package, deleting Tauri/WebView/HTML/CSS/JavaScript/Boa,
changing settings schema or credential target compatibility without a migration, changing recorder
media bytes, weakening Cloud/support endpoints, publishing a candidate installer, linking FFmpeg,
or claiming performance/accessibility/real-hardware evidence that was not accepted.

## Architecture and hard contracts

### Ownership boundaries

`clipline-settings` owns the persisted document, repair/validation, a UI-owned
`SettingsPreferences` projection, a generation-fenced draft controller, preferences-specific
compare/exchange, and the pure transaction coordinator. A Settings UI never owns or replaces the
whole `AppSettings` document. Backend-owned Cloud account identity, credential targets, cleanup
targets, upload generations/records, and osu! credential state remain outside the generic draft.
Applying preferences must merge into the latest durable document under the existing profile-wide
commit gate and must tolerate unrelated Cloud/upload/osu! writes without overwriting them.

Add `clipline-recorder` as the framework-neutral application runtime crate. Move, do not copy,
`apps/clipline-app/src/service.rs` and its recorder-facing pure helpers into it. It owns the real
recorder service, encoder option derivation, settings-to-service conversion, restart preparation,
storage runtime, and joined microphone-test service. Tauri and Slint both construct the same
runtime. The Slint package must never depend on `clipline-app`.

Add `clipline-games` for game identity, first-party plugin manifests, visible-window matching,
installed/running-game discovery, bounded game projections, detector ownership, and neutral osu!
account/probe/enrichment services. Windows enumeration and icon extraction stay under
`clipline-games/src/windows/` behind safe wrappers; game matching remains pure and testable on both
CI platforms. Anti-cheat rules remain unchanged: visible process/window metadata only, no injection,
memory reads, or process hooks. Plugin manifests carry only declarative event-source capability IDs;
the recorder crate owns the League/osu! source spawners so `clipline-games` never depends back on
`clipline-recorder`.

Add `clipline-support` for the complete prepare/preview/save/discard/upload/cancel/retry workflow,
redaction, owned staging, and the bounded prepared-report registry. It must not import Tauri, Slint,
WebView, or an application binary. The shipping endpoint and privacy disclosure remain unchanged;
automated tests use an injected local transport and may never send a production report.

`clipline-library` gains the neutral Cloud connect/disconnect account-mutation service over its
existing protocol, credential, settings, cache, upload, and publication ports. `clipline-shell`
continues to own reviewed safe OS wrappers for hotkeys, autostart, folders, browser, Credential
Manager, and process/filesystem operations. `clipline-playback` owns native decoder-capability truth.
`clipline-desktop` carries only compact, bounded shell/session summaries and typed actions/events;
it never stores a whole settings document, password, credential, Cloud upload map, device catalog,
or unbounded game/support collection.

The Slint root composes dedicated `settings.slint`, `games.slint`, and `support.slint` components.
Slint models are window projections, not service ownership. All device activation, encoder probes,
filesystem enumeration, HTTP, ZIP creation, credential work, recorder restart, and microphone I/O
stay off the event loop. Every asynchronous result carries exact settings-session, attachment,
foreground, request-generation, and operation-kind ownership; Cloud/osu! work additionally carries
exact account/config ownership. Stale work releases resources without publishing or persisting.

### Settings semantics and transaction order

The Rust draft controller is the contract of record for baseline, draft, active tab, dirty fields,
dirty tabs, modal state, save state, and the two-step discard guard. The retained JavaScript/Boa
behavior remains an oracle until equivalent Rust vectors pass. Ephemeral Cloud uploads/account
metadata may reconcile into the view without clearing or manufacturing preference dirtiness.

Saving has a single old-or-new transaction boundary. Preflight normalization, validation, media
authorization, writable-root validation, quota calculation, and side-effect preparation happen
before mutation. User-visible mutations preserve the shipping order:

1. replace global Save Replay hotkeys;
2. replace the tray hotkey label;
3. update the per-user autostart registration where the build is allowed to mutate it;
4. prepare a replacement recorder/runtime behind a start latch;
5. compare/exchange and durably persist only UI-owned preferences;
6. commit the prepared recorder/runtime;
7. publish infallible storage/media-authorization/desktop snapshot reconciliation and warnings.

Every pre-persistence receipt rolls back in exact reverse order. Dropping a prepared recorder must
cancel and join it. The post-persistence recorder commit must be infallible; if implementation proves
that impossible, it must instead supply and test a complete rollback restoring durable bytes,
recorder sender/generation, capture resources, and every earlier side effect before this milestone
can proceed. A partial old/new runtime is a stop condition, not an acceptable warning.

### Pinned bounds

- settings result/command ports: 64 total entries with four reserved terminal/error slots;
  coalescing only within exact session/attachment/foreground/kind/account ownership, where a newer
  request generation replaces older pending work, and never across save/discard/account barriers;
- settings error/notice: 16 KiB UTF-8; status/label/field: 4 KiB unless an existing stricter schema
  bound applies; no password or secret may implement `Serialize` or reveal its value through `Debug`;
- active settings projection: eight top-level tabs, one active panel, at most 64 displays, 128 output
  plus 128 input audio endpoints, 32 encoder options, 16 first-party plugins, 128 custom games,
  256 detected/running candidates, and 60 visible game rows per page; at most 32 decoded game icons,
  each resized or rejected before publication at 256×256, 256 KiB encoded, and 256 KiB decoded RGBA,
  with an 8 MiB aggregate decoded-icon budget;
- probe executor: one active plus one latest pending request per kind, two worker threads total,
  64 bounded result slots, and at most one blocking encoder subprocess probe at a time;
- microphone monitor: at most 4,096 i16 samples per event, one coalesced pending level/sample update,
  no producer queue beyond fixed capture/renderer buffers, and one joined worker per process;
- support registry: at most four prepared reports, 10–4,000 Unicode-scalar description, 25 MiB ZIP,
  30-minute lifetime, one active upload, bounded progress/result delivery, and identity-owned staging;
- Cloud/osu! password or client secret: transient password input only, immediately copied into a
  zeroizing non-serializable command value and the UI field cleared on submit/close. Slint's own
  transient text buffer cannot promise allocator-level zeroization, so no durable model/snapshot/log
  may retain it and the limitation must be documented honestly;
- all lists validate aggregate bytes before publication; hostile existing settings fail closed or
  repair to a documented bounded form rather than allocating an unbounded Slint model.

### Stop conditions

Stop the milestone and record a no-go before broad UI work if preferences CAS can overwrite or
spuriously fail because of unrelated backend-owned state; if recorder commit is neither infallible
nor fully reversible; if background/detach returns while a microphone worker or endpoint remains
live; if a password/secret enters a snapshot/log/serde DTO; if probe/support/game queues or models
are unbounded; if Tauri and Slint use forked recorder, game, account, or support implementations; if
the Slint event loop performs blocking OS/network/filesystem/probe work; or if Settings/microphone
absolute memory, CPU, handle, worker, or lifecycle gates fail.

## Task 1: Freeze UI-owned preferences and the Rust draft contract

**Files**

- Create: `crates/clipline-settings/src/preferences.rs`
- Create: `crates/clipline-settings/src/draft.rs`
- Create: `crates/clipline-settings/src/capture_region.rs`
- Modify: `crates/clipline-settings/src/lib.rs`
- Create: `crates/clipline-settings/tests/preferences.rs`
- Create: `crates/clipline-settings/tests/draft.rs`
- Create: `crates/clipline-settings/tests/capture_region.rs`
- Create: `fixtures/slint/settings-draft-parity.json`
- Create: `apps/clipline-app/tests/settings_draft_parity.rs`
- Modify: `apps/clipline-app/tests/ui_contract.rs`
- Modify: `docs/slint/parity-ledger.md` only to record the frozen acceptance vectors, not status

**Test first**

- Define `SettingsPreferences` as every field the existing Settings form may edit. Cloud preferences
  include default visibility, delete-local, and auto-upload only; Cloud host/account/public URL,
  credential/cleanup targets, upload sequence/records, and the entire osu! credential profile remain
  backend-owned. The explicit UI set is capture mode/backend/window title/region; Games; audio;
  replay window (with the compatibility `buffer_seconds` mirror derived, never independently edited);
  basic/advanced quality, bitrate, FPS, encoder, and resolution; media/replay storage and quotas;
  primary/secondary hotkeys; startup/close/minimize/timeline/theme/update preferences; and the three
  Cloud upload preferences. `from_document` and `apply_to_document` round-trip that set while
  preserving every backend-owned field byte-for-byte.
- Pin `SettingsTab::{General,Capture,Recording,Storage,Hotkeys,Games,Cloud,Support}` order, cyclic
  Left/Right, Home/End, roving focus, active-panel projection, and checked session generation.
- Pin baseline/draft equality, normalized field updates, per-field/per-tab dirty summaries, Save
  visibility, successful-save rebase, discard rebase, stale result rejection, and generation
  exhaustion without partial mutation.
- Reproduce the current two-step dirty close: first explicit Close or Escape arms the warning;
  second explicit Close or Escape discards. Backdrop never discards dirty data. A field edit resets
  the armed warning. Opening/closing clean settings is one step.
- Support hides the generic Save only when Support is active and no non-Support preference is dirty.
  Switching tabs never drops edits. Opening Settings pauses Review intent but does not destroy the
  retained clip; closing restores the last saved baseline, not the abandoned draft.
- Durable Cloud/account/upload/osu! reconciliation updates display-only state without changing the
  preference baseline or dirty flags. A concurrent replacement account cannot inherit a dialog or
  probe owned by the prior account.
- Exercise the same representative JSON vectors through Rust and the retained JavaScript/Boa
  oracle. Preserve JavaScript and DOM contract tests until Milestone 10.
- Pure capture-region math handles negative monitor coordinates, display removal, clamping, minimum
  size, full-display selection, and logical/physical DPI conversion without Slint geometry types.

**Implement to green**

- Keep the controller framework-free and allocation-fallible for bounded collections. Do not add
  Slint/Tauri types or passwords to `clipline-settings` DTOs.
- Add explicit validation/bounds for custom games/plugin maps and existing settings repair. Preserve
  legacy IDs and schema compatibility; a repair warning is preferable to an unbounded model.

## Task 2: Add preferences-specific durable compare/exchange

**Files**

- Modify: `crates/clipline-settings/src/persistence.rs`
- Modify: `crates/clipline-settings/src/preferences.rs`
- Modify: `crates/clipline-settings/src/lib.rs`
- Modify: `crates/clipline-settings/tests/persistence.rs`
- Modify: `crates/clipline-settings/tests/preferences.rs`
- Modify: `apps/clipline-slint-spike/src/settings.rs`
- Modify: `apps/clipline-slint-spike/tests/settings_store.rs`

**Test first**

- `replace_preferences_if_unchanged(expected, replacement)` locks the profile-wide commit gate,
  reads current durable state, compares only normalized UI-owned preferences, merges into the latest
  document, validates, writes atomically, and returns the exact new settings/account revisions.
- Unrelated Cloud upload/profile/account-generation/cleanup and osu! state changes between draft
  open and save remain intact and do not cause a false conflict. A change to any expected UI-owned
  preference returns typed `StalePreferences` with no write or in-memory mutation.
- Two settings sessions changing disjoint or overlapping preferences have deterministic conflict
  behavior; no last-writer-wins whole-document overwrite is possible.
- External primary replacement, invalid repaired state, disk/write/sync/rename failure, stale file
  identity, revision exhaustion, and lock poison fail closed. Independently opened stores over the
  same profile serialize on the same gate.
- A Cloud account ABA, upload progress burst, and osu! credential update racing preference save are
  retained exactly. Test current and compatibility serialization bytes after each merge.

**Implement to green**

- Do not implement this as a retrying whole-document `ReplaceDocument`. One in-gate comparison and
  merge is the linearization point.
- `CandidateSettings` exposes the new operation without changing installed/isolated profile rules.

## Task 3: Move the recorder and game foundation into shared crates

**Files**

- Modify: `Cargo.toml`
- Create: `crates/clipline-recorder/Cargo.toml`
- Create: `crates/clipline-recorder/src/lib.rs`
- Create: `crates/clipline-recorder/src/service.rs`
- Create: `crates/clipline-recorder/src/media_root.rs`
- Create: `crates/clipline-recorder/src/marker_source.rs`
- Create: `crates/clipline-recorder/src/time.rs`
- Create: `crates/clipline-recorder/tests/runtime.rs`
- Create: `crates/clipline-games/Cargo.toml`
- Create: `crates/clipline-games/src/lib.rs`
- Create: `crates/clipline-games/src/identity.rs`
- Create: `crates/clipline-games/src/plugin.rs`
- Create: `crates/clipline-games/src/detection.rs`
- Create: `crates/clipline-games/src/discovery.rs`
- Create: `crates/clipline-games/src/windows/mod.rs`
- Create: `crates/clipline-games/src/windows/icon.rs`
- Create: `crates/clipline-games/tests/identity.rs`
- Create: `crates/clipline-games/tests/detection.rs`
- Create: `crates/clipline-games/tests/discovery.rs`
- Modify: `apps/clipline-app/Cargo.toml`
- Modify: `apps/clipline-app/src/service.rs`
- Modify: `apps/clipline-app/src/service/media_root.rs`
- Modify: `apps/clipline-app/src/game_identity.rs`
- Modify: `apps/clipline-app/src/game_plugins.rs`
- Modify: `apps/clipline-app/src/games.rs`
- Modify: `apps/clipline-app/src/game_discovery.rs`
- Modify: `apps/clipline-app/src/game_icon.rs`
- Modify: `apps/clipline-app/src/markers.rs`
- Modify: `apps/clipline-app/src/util.rs`
- Modify: `apps/clipline-app/src/windows/mod.rs`
- Modify: `apps/clipline-app/src/app.rs`
- Modify: `apps/clipline-app/tests/repository_security.rs`

**Test first**

- Move the existing recorder, encoder, media-root, marker-source, game identity/plugin/matching/
  discovery, icon, migration, and anti-cheat fixtures to shared crates without changing outputs.
- `clipline-recorder` has no Tauri/Slint/WebView import and no application-crate dependency.
  `clipline-games` keeps every unsafe/Win32 import under `src/windows/` and exposes safe DTOs.
- Convert app-local module files to thin re-exports/adapters, then delete them when no compatibility
  symbol needs them. Add repository tests rejecting duplicate algorithms and reverse dependencies.
- Build/test the shipping Tauri app after each moved vertical slice. Recorder output bytes, encoder
  selection order, game priority, custom-game migrations, event-source behavior, and settings JSON
  remain unchanged.

**Implement to green**

- Move, do not fork. Preserve module history where practical and keep app command signatures stable.
- Confine application wiring, Tauri state extraction, and event emission to the app adapter.
- Call `clipline-shell`'s existing safe process-instance and available-space wrappers directly; do
  not move or duplicate their Win32 implementations into the recorder crate.

## Task 4: Make settings application transactional across live runtime effects

**Files**

- Create: `crates/clipline-settings/src/coordinator.rs`
- Create: `crates/clipline-settings/tests/coordinator.rs`
- Modify: `crates/clipline-settings/src/lib.rs`
- Create: `crates/clipline-recorder/src/restart.rs`
- Create: `crates/clipline-recorder/tests/restart.rs`
- Modify: `crates/clipline-recorder/src/lib.rs`
- Modify: `crates/clipline-shell/src/hotkey.rs`
- Modify: `crates/clipline-shell/src/windows/autostart.rs`
- Modify: `apps/clipline-app/src/app.rs`
- Modify: `apps/clipline-app/src/settings/mod.rs`
- Modify: `apps/clipline-app/src/settings/tests.rs`
- Create: `apps/clipline-app/tests/settings_transaction.rs`

**Test first**

- Define narrow preflight/hotkey/tray/autostart/recorder/persistence/publication ports and owned
  rollback receipts. Exercise failures before and after every numbered transaction boundary.
- For every failure, assert settings bytes and revision, hotkey registrations, tray label, Run key
  abstraction, media authorization, quota/root state, recorder sender/generation/thread, desktop
  snapshot, warnings, and Cloud/account state are entirely old or entirely new.
- Prepared recorder restart validates/spawns behind a start latch; drop before commit cancels and
  joins without publishing a sender/generation. Commit cannot fail after durable preferences publish.
- Preserve debug/benchmark autostart non-mutation, warning publication, exact reverse rollback error
  aggregation, and the shipping `save_settings` JSON result/error behavior.
- A concurrent Cloud upload/account/profile/osu! transaction during every injected wait neither
  deadlocks nor disappears. Do not hold controller, Slint, Tauri, or recorder locks across disk/OS I/O.

**Implement to green**

- Migrate the Tauri command to the coordinator before exposing a Slint Save callback.
- Remove `preserve_backend_owned_settings_fields` and app-local rollback sequencing only after the
  preferences merge and coordinator tests prove exact compatibility.

## Task 5: Add the bounded settings probe contract and executor

**Files**

- Create: `crates/clipline-settings/src/probe.rs`
- Create: `crates/clipline-settings/tests/probe.rs`
- Modify: `crates/clipline-settings/src/lib.rs`
- Create: `crates/clipline-recorder/src/probe.rs`
- Create: `crates/clipline-recorder/tests/probe.rs`
- Modify: `crates/clipline-recorder/Cargo.toml`
- Modify: `crates/clipline-recorder/src/lib.rs`
- Modify: `crates/clipline-capture/src/windows/display.rs`
- Modify: `crates/clipline-capture/src/windows/wasapi.rs`
- Modify: `crates/clipline-desktop/src/action.rs`
- Modify: `crates/clipline-desktop/src/event.rs`
- Modify: `crates/clipline-desktop/src/channel.rs`
- Modify: `crates/clipline-desktop/src/controller.rs`
- Modify: `crates/clipline-desktop/src/snapshot.rs`
- Modify: `crates/clipline-desktop/tests/channel.rs`
- Modify: `crates/clipline-desktop/tests/controller.rs`
- Modify: `apps/clipline-app/src/app.rs`

**Test first**

- Reuse bounded domain DTOs owned by capture, recorder, games, storage, and playback for displays,
  audio endpoints, encoders, game windows/candidates/plugins, storage, and playback capabilities.
  The settings-session result port owns the full bounded collections; do not duplicate them in
  `clipline-desktop`. Validate count and aggregate UTF-8/byte bounds before allocation/publication.
- Token requests/results with settings session, attachment, foreground, probe kind, and checked
  request generation. Recheck before I/O, after OS/device activation, and before publication.
- The two-worker executor owns one active plus one latest pending request per kind. The coalesce key
  is exact settings session + attachment + foreground + kind and, where relevant, account/config
  owner; a newer checked request generation replaces older pending work within that key. Results
  still carry the exact request generation. Coalescing never crosses owners or terminal/error/save/
  discard/account barriers. Admission is nonblocking and reports Full/Disconnected/
  GenerationExhausted distinctly.
- Fake slow activation, out-of-order results, tab changes, background, detach/rebuild, default-device
  replacement, 10,000-request storms, huge OS labels, and executor shutdown. Assert no stale publish,
  one outstanding UI delivery, fixed queue memory, and joined workers.
- Only the active tab's probes run. Opening Settings reaches first paint before encoder/device/game
  enumeration. Probe errors are per-kind bounded state and never make unrelated tabs unusable.

**Implement to green**

- Reuse capture's safe display/audio enumeration and move encoder option derivation from the app
  service into `clipline-recorder`. Do not create parallel Windows enumerators.
- Tauri commands become adapters over the same DTOs/executor where asynchronous compatibility allows.
- `clipline-desktop` receives only compact settings-session probe phase/generation/error summaries;
  full device/game/capability catalogs remain in the settings-session/domain services and never enter
  `DesktopSnapshot`.

## Task 6: Replace WebView codec reporting with native playback capability truth

**Files**

- Create: `crates/clipline-playback/src/capability.rs`
- Create: `crates/clipline-playback/tests/capability.rs`
- Create: `crates/clipline-playback/src/windows/capability.rs`
- Modify: `crates/clipline-playback/src/windows/mod.rs`
- Modify: `crates/clipline-playback/src/lib.rs`
- Modify: `crates/clipline-recorder/Cargo.toml`
- Modify: `crates/clipline-recorder/src/probe.rs`
- Modify: `crates/clipline-recorder/src/service.rs`
- Modify: `crates/clipline-recorder/tests/probe.rs`
- Modify: `apps/clipline-app/src/app.rs`
- Modify: `apps/clipline-app/ui/settings.js`
- Modify: `apps/clipline-app/tests/ui_contract.rs`
- Modify: `docs/slint/parity-ledger.md`

**Test first**

- `PlaybackCapabilities` distinguishes probed hardware H.264, probed software H.264, unavailable,
  and intentionally unsupported/ungated HEVC/AV1. H.264 support requires real decoder configuration,
  not enum presence or a browser guess.
- Automatic encoder selection remains H.264-first and falls back according to the existing encoder
  matrix. Explicit HEVC/AV1 choices remain available where encoding is supported but display a typed
  limited-native-playback warning until those decoders pass the full native media gates.
- Probe failure, Basic Display Adapter, no hardware manager, software fallback, device loss, and
  capability refresh after device generation change are deterministic and bounded.
- Retain `report_decode_support` only as the shipping Tauri compatibility input while WebView review
  remains production. It must not influence the Slint runtime.

**Implement to green**

- Reuse the proven MFT candidate/configuration logic without retaining a decoder surface or starting
  a playback session. Balance COM/MF lifetime and keep unsafe under `src/windows/`.

## Task 7: Extract a joined, bounded native microphone-test service

**Files**

- Create: `crates/clipline-recorder/src/microphone.rs`
- Create: `crates/clipline-recorder/tests/microphone.rs`
- Create: `crates/clipline-recorder/tests/windows_microphone.rs`
- Modify: `crates/clipline-recorder/Cargo.toml`
- Modify: `crates/clipline-recorder/src/lib.rs`
- Modify: `crates/clipline-capture/src/pcm.rs`
- Modify: `crates/clipline-capture/src/windows/mod.rs`
- Modify: `crates/clipline-capture/src/windows/wasapi.rs`
- Modify: `crates/clipline-playback/src/windows/wasapi_render.rs`
- Modify: `crates/clipline-playback/tests/windows_audio.rs`
- Modify: `crates/clipline-desktop/src/action.rs`
- Modify: `crates/clipline-desktop/src/event.rs`
- Modify: `crates/clipline-desktop/src/channel.rs`
- Modify: `crates/clipline-desktop/src/controller.rs`
- Modify: `crates/clipline-desktop/src/lib.rs`
- Modify: `crates/clipline-desktop/tests/channel.rs`
- Modify: `crates/clipline-desktop/tests/controller.rs`
- Modify: `crates/clipline-desktop/tests/contracts.rs`
- Modify: `apps/clipline-app/src/app.rs`
- Modify: `apps/clipline-app/src/desktop/tauri_sink.rs`
- Modify: `apps/clipline-slint-spike/Cargo.toml`
- Modify: `apps/clipline-slint-spike/src/shell.rs`

**Test first**

- Service ownership is one worker, one source, optional native renderer, checked generation, and a
  join handle. Start atomically replaces and joins the prior generation; stop/background/detach/drop
  return only after source, renderer, worker, and pending samples are released.
- Fixed input chunks publish at most 4,096 i16 samples with validated finite RMS/peak and exact
  sample_count. One coalesced pending monitor update bounds slow-consumer memory; terminal Error then
  Stopped barriers preserve order and cannot be overwritten.
- Tauri mode emits compatibility PCM events without native playback. Slint mode feeds the bounded
  WASAPI renderer and publishes compact levels, never double-playing the microphone.
- Pin the source format and bounded conversion into the renderer's 48 kHz stereo-f32 contract.
  Device sample-rate/channel changes use one preallocated resampler/channel converter; no implicit
  unbounded queue or per-poll allocation is allowed.
- Test start failure, replacement during activation, stop during activation/read/write, stale samples,
  slow UI, renderer backpressure, device invalidation/reopen, default endpoint change, panic, 100
  start/stop cycles, and process shutdown. No sample/event may appear after joined stop.
- Device tests record endpoint/format/conversion/epoch and self-skip honestly when hardware is absent.

**Implement to green**

- Remove app-local `MicTestState` only after Tauri uses the shared service. Keep every COM/unsafe call
  inside existing Windows modules and preserve the current desktop event payload contract.

## Task 8: Complete shared Games, detector, and osu! services

**Files**

- Modify: `crates/clipline-games/Cargo.toml`
- Create: `crates/clipline-games/src/controller.rs`
- Create: `crates/clipline-games/src/presentation.rs`
- Create: `crates/clipline-games/src/channel.rs`
- Create: `crates/clipline-games/src/osu.rs`
- Create: `crates/clipline-games/src/osu_http.rs`
- Create: `crates/clipline-games/src/osu_enrichment.rs`
- Create: `crates/clipline-games/tests/controller.rs`
- Create: `crates/clipline-games/tests/presentation.rs`
- Create: `crates/clipline-games/tests/osu.rs`
- Create: `crates/clipline-games/tests/osu_enrichment.rs`
- Modify: `crates/clipline-games/src/lib.rs`
- Modify: `crates/clipline-games/src/identity.rs`
- Modify: `crates/clipline-games/tests/identity.rs`
- Modify: `crates/clipline-games/src/windows/icon.rs`
- Modify: `crates/clipline-settings/src/games.rs`
- Modify: `crates/clipline-settings/src/osu.rs`
- Modify: `crates/clipline-settings/src/coordinator.rs`
- Modify: `crates/clipline-settings/src/persistence.rs`
- Modify: `crates/clipline-settings/tests/capture_region.rs`
- Modify: `crates/clipline-settings/tests/coordinator.rs`
- Create: `crates/clipline-settings/tests/osu.rs`
- Modify: `crates/clipline-settings/tests/persistence.rs`
- Modify: `crates/clipline-shell/Cargo.toml`
- Modify: `crates/clipline-shell/src/hotkey.rs`
- Modify: `crates/clipline-shell/src/windows/credential.rs`
- Modify: `crates/clipline-shell/tests/hotkey.rs`
- Modify: `crates/clipline-desktop/src/action.rs`
- Modify: `crates/clipline-desktop/src/event.rs`
- Modify: `crates/clipline-desktop/src/snapshot.rs`
- Modify: `crates/clipline-desktop/src/channel.rs`
- Modify: `crates/clipline-desktop/src/controller.rs`
- Modify: `crates/clipline-desktop/src/lib.rs`
- Modify: `crates/clipline-desktop/tests/channel.rs`
- Modify: `crates/clipline-desktop/tests/contracts.rs`
- Modify: `crates/clipline-desktop/tests/controller.rs`
- Modify: `apps/clipline-app/Cargo.toml`
- Modify: `apps/clipline-app/src/settings_probe.rs`
- Modify: `apps/clipline-app/src/games.rs`
- Modify: `apps/clipline-app/src/game_discovery.rs`
- Modify: `apps/clipline-app/src/game_icon.rs`
- Modify: `apps/clipline-app/src/osu_api.rs`
- Modify: `apps/clipline-app/src/osu_enrichment.rs`
- Modify: `apps/clipline-app/src/app.rs`
- Modify: `apps/clipline-app/tests/player_core.rs`
- Modify: `apps/clipline-app/tests/ui_contract.rs`

**Test first**

- Controller consumes the existing Task 5 `GamePlugins`, `GameWindows`, and `InstalledGames` probe
  catalogs instead of launching duplicate enumeration. It retains bounded plugin/custom/candidate
  state in Rust and projects at most 60 rows. `GameItemIdentity` distinguishes plugin, custom, and
  candidate; candidates carry the exact `ProbeToken` plus a deterministic opaque id, never a row
  index, wall clock, or collision-prone raw `id_hint`. Catalog-owned builders ingest the real
  `DetectedGameCandidate` and `GameWindowInfo` payloads, derive ids from complete canonical source
  authority, reject duplicate/colliding handles, and retain the exact handle-to-source membership
  map used by every mutation. Refresh invalidates only identities owned by the replaced token. Pin
  combined ordering/dedupe, total/page count, PastEnd, selection/dialog invalidation, forged-handle
  rejection, and 0/60/61/128/256/400-row behavior.
- Rows carry typed icon ids and bounded loading state, not decoded RGBA. A separate token-fenced image
  cache owns at most 8 MiB and 32 maximum-size surfaces, checks encoded bytes and header dimensions
  before allocation, resizes within the 256×256/256 KiB RGBA per-icon cap, releases stale images, and
  falls back to a missing icon under admission pressure instead of failing a valid 60-row page. Test
  60 highly-compressed 256×256 icons, hostile dimensions, stale completion, and allocation failure.
- Keep game matching and candidate dedupe pure; capture-region/DPI geometry remains owned and tested
  by `clipline-settings::capture_region` from Task 1. Add an explicit 100/125/150/200% and negative
  half-tie table; do not move this geometry into Games.
- Hotkey capture is a pure bounded reducer over the shared `clipline-shell` grammar: modifier-only is
  Pending; valid key/mouse is Captured; Escape clears the active field; blur/detach cancels unchanged;
  invalid/reserved/duplicate input never mutates the draft; clearing the last configured key is
  rejected. OS registration occurs only in transactional Save and rollback restores the exact prior
  `HotkeySet`. Cross-run the shipping Boa vectors.
- Replace the detached detector with a process-owned, generation-fenced, joined service with one
  active plus one coalesced pending reconfiguration. Check cancellation before enumeration, after
  enumeration, and before event/recorder intent. Settings apply owns a rollback receipt. Test scan
  races, restart failure, stale completion, save storms, full/disconnected sink, spawn failure, and
  shutdown join; elevated warning suppression remains once per process.
- osu! status/save/test/disconnect uses injected credential/settings/HTTP/browser ports. The client
  secret and access token are non-cloneable/non-serializable/redacted zeroizing owners; every
  first-party secret/request/blob copy is zeroized on drop. Add a persisted checked
  `OsuAccountGeneration` that advances on save/test/disconnect so account ABA is rejected. Credential
  write precedes an exact osu-profile CAS and every failure restores or schedules cleanup of exact
  prior/new targets. No secret, credential target, or cleanup target enters a desktop snapshot or
  Slint model.
- Mock-server success, offline, timeout, malformed/oversized response, pagination ceiling, 401,
  account replacement, cancellation, stale completion, and enrichment single-flight. Tauri adapter
  results and existing enrichment sidecars remain byte-compatible. Cap each page at 100 and the total
  at 500 before append, bound every field/score/aggregate allocation, use one operation deadline,
  disable redirects, and checkpoint between pages and before every credential/settings/enrichment
  side effect. Sidecar publication is identity-fenced and atomic.

**Implement to green**

- Execute as independently committable slices: (1) identities/bounds/contracts, (2) pure
  pagination/presentation and icon cache, (3) controller/channel over existing probe tokens, (4)
  joined detector plus settings transaction receipt, (5) hotkey capture reducer, (6) osu secret
  domain and exact profile CAS, (7) bounded HTTP plus shared enrichment, and (8) thin Tauri cutover
  with exact command/JSON compatibility and duplicate-algorithm rejection tests. No network or
  process enumeration runs on the UI thread.

## Task 9: Extract exact Cloud connect/disconnect account mutation

**Files**

- Create: `crates/clipline-library/src/cloud/account.rs`
- Create: `crates/clipline-library/tests/cloud_account.rs`
- Modify: `crates/clipline-library/src/cloud.rs`
- Modify: `crates/clipline-library/src/cloud/ports.rs`
- Modify: `crates/clipline-library/src/cloud/settings.rs`
- Modify: `crates/clipline-library/src/cloud/http.rs`
- Modify: `crates/clipline-library/src/lib.rs`
- Modify: `crates/clipline-shell/src/windows/credential.rs`
- Modify: `apps/clipline-app/src/cloud.rs`
- Modify: `apps/clipline-app/tests/cloud_core.rs`

**Test first**

- Connect is a checked operation over host/security consent, discovery, token/login, profile fetch,
  credential publication, durable account CAS, and owner-change publication. Failure after credential
  write deletes by exact identity or records a bounded cleanup target; no partial connected account
  appears.
- Plain HTTP remains restricted to confirmed localhost/loopback/RFC1918 origins. Redirects, endpoint
  crossing, credential leakage, oversized responses, timeout, malformed discovery/profile, wrong
  account, settings contention, cancellation, and ABA fail closed.
- Disconnect durably clears the account owner before canceling uploads/cache/profile/catalog work and
  deleting exact credentials. Cleanup failure remains durable/retryable and cannot resurrect access.
- Account replacement invalidates catalog, thumbnail, media lease, profile, avatar, upload, and
  Settings dialog/probe ownership while preserving unrelated dirty preferences.
- Password payload has redacted `Debug`, no serde traits, zeroizes on drop, and never enters events,
  snapshots, diagnostics, persistence, or test failure output.

**Implement to green**

- Migrate Tauri's `cloud_connect`, `cloud_disconnect`, and status flow to the service before Slint.
- Reuse M7's shared Cloud runtime/cache/upload/account fences; do not create a Settings-only client.

## Task 10: Move Support into a bounded neutral service

**Files**

- Modify: `Cargo.toml`
- Create: `crates/clipline-support/Cargo.toml`
- Create: `crates/clipline-support/src/lib.rs`
- Create: `crates/clipline-support/src/contract.rs`
- Create: `crates/clipline-support/src/controller.rs`
- Create: `crates/clipline-support/src/bundle.rs`
- Create: `crates/clipline-support/src/transport.rs`
- Create: `crates/clipline-support/tests/contract.rs`
- Create: `crates/clipline-support/tests/controller.rs`
- Create: `crates/clipline-support/tests/bundle.rs`
- Create: `crates/clipline-support/tests/transport.rs`
- Modify: `apps/clipline-app/Cargo.toml`
- Modify: `apps/clipline-app/src/app/support.rs`
- Modify: `apps/clipline-app/tests/support_core.rs`
- Modify: `apps/clipline-app/tests/repository_security.rs`

**Test first**

- Port the retained `SupportCore` state vectors to typed Rust phases/actions/projections first:
  idle, preparing, prepared, uploading, canceled, failed/retryable, submitted/report-ID, saved, and
  discarded. Exact operation generations reject every stale completion.
- Registry retains at most four prepared reports. Fifth admission evicts only an expired/unreferenced
  owned entry or returns typed Capacity; expiry/drop/discard remove owned staging by identity and
  preserve foreign replacements. Startup janitor is age-, count-, root-, and identity-bounded.
- Preserve 10–4,000-character description, 25 MiB ZIP, 30-minute lifetime, disclosure, redaction,
  bundle manifest/hash, diagnostic bounds, official HTTPS endpoint, cancellation, and save-copy
  behavior. Bundle creation, hashing, save-copy, and upload stream from the owned file with bounded
  chunks; the registry retains metadata/path/identity only and never a 25 MiB in-memory payload. All
  input files/entries/bytes and error strings have explicit aggregate caps.
- Inject a local transport for success, partial body, 4xx/5xx, redirect, timeout, cancellation,
  retry, malformed/oversized response, and report-ID bounds. No test contacts production.
- Cross-run Rust projections against `support-core.js`; keep Tauri DTO/command names stable.
- Define a bounded `SupportSnapshotPort` for recorder/settings/device/build/diagnostic facts. Bundle
  construction consumes that plain-data snapshot and reviewed log/file ports; it may not reach into
  a Tauri `RuntimeState` or Slint component.
- Cover `diagnostics_location`, `support_capabilities`, rate-limited/redacted `log_frontend_event`,
  and the existing shared `open_diagnostics_folder` effect explicitly so parity rows are not lost.

**Implement to green**

- Tauri becomes a state/async adapter over `clipline-support`. Keep OS folder opening and picker
  execution behind `clipline-shell` or the app adapter, not in the neutral controller.

## Task 11: Build the native Settings shell and core panels

**Files**

- Create: `apps/clipline-slint-spike/ui/settings.slint`
- Create: `apps/clipline-slint-spike/src/settings_adapter.rs`
- Create: `apps/clipline-slint-spike/src/settings_runtime.rs`
- Create: `apps/clipline-slint-spike/tests/settings_adapter.rs`
- Create: `apps/clipline-slint-spike/tests/settings_runtime.rs`
- Modify: `apps/clipline-slint-spike/Cargo.toml`
- Modify: `apps/clipline-slint-spike/ui/app.slint`
- Modify: `apps/clipline-slint-spike/build.rs`
- Modify: `apps/clipline-slint-spike/src/lib.rs`
- Modify: `apps/clipline-slint-spike/src/shell.rs`
- Modify: `apps/clipline-slint-spike/src/controller.rs`
- Modify: `apps/clipline-slint-spike/tests/compile.rs`
- Modify: `apps/clipline-slint-spike/tests/keyboard.rs`
- Modify: `apps/clipline-slint-spike/tests/shell.rs`

**Test first**

- Compile/project General, Capture, Recording, Storage, and Hotkeys from the Rust draft. No whole
  settings JSON, hidden tab model, device work, or file I/O occurs in a Slint callback.
- Typed callbacks cover open/close/discard/save, tab navigation, every editable field, display/region
  manipulation, folder pickers, encoder/audio probes, two hotkey recorders, and update/manual-check.
- Opening Settings pauses Review and microphone ownership safely; close/restore preserves Review and
  catalog state. Background synchronously joins microphone work and invalidates probes/dialogs.
- Save runs the shared coordinator off-loop, publishes bounded saving/error/saved state, and rebases
  only on exact successful completion. Rebuild from tray recreates the exact controller-owned draft
  or baseline according to lifecycle policy.
- Every custom control exposes role, accessible name/description, value, checked/expanded/invalid/
  busy state, focus target, keyboard operation, and visible focus. Roving tabs implement
  Left/Right/Home/End; dialog-first Escape then dirty-close guard is exact.

**Implement to green**

- Project one active panel and bounded option models. Capture-region drawing uses pure Rust geometry
  and Slint primitives; it does not retain one object per physical pixel or monitor outside caps.

## Task 12: Build native Games and osu! panels

**Files**

- Create: `apps/clipline-slint-spike/ui/games.slint`
- Create: `apps/clipline-slint-spike/src/games_adapter.rs`
- Create: `apps/clipline-slint-spike/tests/games_adapter.rs`
- Modify: `apps/clipline-slint-spike/Cargo.toml`
- Modify: `apps/clipline-slint-spike/ui/settings.slint`
- Modify: `apps/clipline-slint-spike/ui/app.slint`
- Modify: `apps/clipline-slint-spike/src/settings_runtime.rs`
- Modify: `apps/clipline-slint-spike/src/shell.rs`
- Modify: `apps/clipline-slint-spike/tests/compile.rs`
- Modify: `apps/clipline-slint-spike/tests/keyboard.rs`

**Test first**

- Present supported/custom games, auto-detect/pause behavior, recording modes, plugin-specific event/
  review settings, detected-game and running-window dialogs, refresh/cancel/select, icon fallback,
  detector status, and elevation warning through bounded typed models.
- Candidate selection survives page refresh by identity, never by row index. At most 60 rows and 32
  decoded icons are retained; stale candidates/images release without state mutation.
- Present osu! disconnected/configured/test/busy/success/error state without a secret field in any
  snapshot. Password/client-secret input clears on submit/close and never reappears after rebuild.
- Keyboard/focus tests cover list paging, dialog traps, Enter/Space, Escape, tab order, and dynamic
  insertion/removal. Slow detector/probe/account results cannot steal focus or reopen a dialog.

**Implement to green**

- Reuse the shared game/osu! services and the Task 5 executor. No app module or direct Win32/HTTP
  call is allowed in the Slint adapter.

## Task 13: Build native Cloud-account and Support panels

**Files**

- Create: `apps/clipline-slint-spike/ui/support.slint`
- Create: `apps/clipline-slint-spike/src/support_adapter.rs`
- Create: `apps/clipline-slint-spike/tests/support_adapter.rs`
- Modify: `apps/clipline-slint-spike/Cargo.toml`
- Modify: `apps/clipline-slint-spike/ui/settings.slint`
- Modify: `apps/clipline-slint-spike/ui/app.slint`
- Modify: `apps/clipline-slint-spike/src/settings_runtime.rs`
- Modify: `apps/clipline-slint-spike/src/cloud.rs`
- Modify: `apps/clipline-slint-spike/src/shell.rs`
- Modify: `apps/clipline-slint-spike/tests/cloud_runtime.rs`
- Modify: `apps/clipline-slint-spike/tests/compile.rs`
- Modify: `apps/clipline-slint-spike/tests/keyboard.rs`

**Test first**

- Cloud panel covers disconnected host/username/password, plain-HTTP consent, connecting/error,
  connected identity, default visibility/delete-local/auto-upload preferences, disconnect/reconnect,
  and exact account replacement. Password is cleared immediately and never projected back.
- Account mutation and catalog/profile/avatar/cache/upload/media reconciliation share one owner-change
  path. Dirty non-account preferences survive connect/disconnect; stale account dialogs/probes close.
- Support covers disclosure, description validation, prepare/progress/preview, save-as, upload,
  cancel/retry, discard, report-ID, unavailable endpoint, and expiration through the shared service.
- Generic Settings Save remains visible on Support when another preference tab is dirty. Background,
  detach, rebuild, and quit cancel/join active UI-owned work and retain or discard prepared reports
  according to the controller contract.
- Focus trap, announcements, live progress semantics, error association, keyboard-only operation,
  and reduced-motion/high-contrast presentation are compile/snapshot tested where possible.

**Implement to green**

- Use injected fake transports in automation. Any real Cloud account test is non-mutating; any real
  support submission requires separate explicit user authorization and is not a milestone default.

## Task 14: Prove lifecycle, performance, accessibility, and parity gates

**Files**

- Create: `scripts/measure-slint-settings.ps1`
- Create: `apps/clipline-slint-spike/examples/settings_harness.rs`
- Create: `docs/slint/native-settings-protocol.md`
- Modify: `scripts/measure-frontend-baseline.ps1`
- Modify: `scripts/test-frontend-baseline-tools.ps1`
- Modify: `docs/slint/baseline-protocol.md`
- Modify: `docs/slint/parity-ledger.md`
- Modify: `handoff.md`
- Modify: `ddoc.md` only for durable architecture proven by accepted evidence

**Test first**

- Harness uses a disposable settings/profile/media/cache root and deterministic fake OS/account/
  support ports unless a scenario explicitly requires real hardware. Exact ready/panel-settled/
  probe-settled/microphone-started/stopped/error/stop markers and atomic telemetry fail closed.
- Scenarios: Settings idle on every tab; 100 reveal/close cycles; 10,000 probe requests; 100
  microphone start/stop/replacement cycles; device-loss/default-device change; game candidate churn;
  Cloud connect/disconnect replacement against a local server; Support prepare/save/discard/cancel/
  retry; and Settings save failure injection.
- Require zero live workers/endpoints/COM resources/dialogs/secret values after close, exact balanced
  model/image/probe/support-temp counters, zero stale publication, no unbounded queue/handle/thread
  growth, and no more than 10 MiB PWS growth across each 100-cycle soak.
- Absolute gates: first usable Settings panel at most 500 ms after window readiness excluding explicit
  cold encoder probe; panel/tab interaction p95 at most 100 ms; five-minute Settings-idle PWS p50 at
  most 100 MiB; microphone level-to-render p95 at most 100 ms; microphone CPU p95 no worse than one
  logical core; no sample after joined stop. Record p95/max and reject noisy runs per the baseline
  protocol. Run at least three accepted samples per absolute scenario before pass.
- Matched Tauri/Slint samples use the same machine, profile, devices/fakes, warmup, steady window,
  cadence, and noise rules. Slint Settings five-minute PWS p50 must be at most 65% of Tauri and
  private commit at least 25% lower; panel/probe interaction p95 may be at most 50 ms slower; idle
  and microphone CPU may be at most one percentage point higher. Never tune these after results.
- The exact accepted absolute matrix is three samples each for `settings-idle` with every one of the
  eight tabs active, `probe-storm`, `settings-reveal-close-100`, `microphone-monitor`, and
  `microphone-cycle-100`. The exact matched matrix is three paired Settings-idle samples per tab and
  three paired microphone-monitor samples. Other correctness/failure scenarios remain mandatory but
  do not substitute for those samples.
- Manual matrix: Narrator and UI Automation role/name/value/state/live announcements; keyboard-only
  every field/dialog; high contrast and reduced motion; 100/125/150/200% DPI; negative-coordinate
  monitors; device add/remove/default change; hotkeys/autostart; Windows 10 and 11; real GPU/encoders;
  real microphone. Record skips and blockers without promoting them.

**Implement to green**

- Reuse process identity, provenance, noise, create-new evidence, and installed-process exclusion from
  the existing samplers. Never kill PID 5548 or touch the installed profile.
- Run `cargo test --workspace`, strict workspace Clippy, fresh-cache changed-crate Clippy, and Slint
  default/`package-regular`/`package-standalone` all-target test/Clippy matrices.
- Run focused settings persistence/draft/coordinator, recorder/game/mic/probe, Cloud account, Support,
  Tauri UI/command, repository-security, Slint compile/keyboard/lifecycle, and PowerShell helper suites.
- Advance parity rows only for behavior proved. Keep Tauri/JS/Boa and rollback packaging intact.

## Required completion evidence

Milestone 8 is complete only when:

1. UI-owned settings preferences, draft/dirty/discard semantics, and preferences CAS are shared,
   bounded, and cannot overwrite backend-owned Cloud/upload/osu! state;
2. settings application is old-or-new across hotkeys, tray label, autostart, durable preferences,
   recorder runtime, storage/media authorization, and desktop reconciliation at every injected error;
3. Tauri and Slint use the same recorder, probe, microphone, Games/osu!, Cloud-account, and Support
   services, with no application/spike forks or reverse dependencies;
4. every Settings surface/dialog/shortcut in the parity ledger is implemented through bounded Slint
   projections with exact lifecycle/account/request fencing and no durable secret exposure;
5. background/detach/shutdown synchronously join microphone/probe/game/support workers and release all
   endpoints, models, images, temps, leases, and dialogs;
6. shipping Tauri behavior/tests and the production package remain green and rollback-capable;
7. absolute lifecycle/resource/performance gates pass or the milestone records the mandated no-go;
   matched/manual/environment-dependent gates may remain explicitly pending but never silently waived;
8. workspace, standalone feature-matrix, fresh-cache Clippy, protocol, ledger, handoff, and evidence
   are current, and no installed profile, real Cloud account, or production support endpoint was
   mutated by automation.
