# Slint Milestone 5: Framework-neutral desktop controller

> Execution plan for Milestone 5 of `2026-08-01-slint-frontend-replacement.md`.
> Follow the repository convention: commit this plan before implementation, keep the checkboxes
> unticked, and execute each task test-first in its own logical commit.

## Goal

Make Clipline's application-facing UI boundary independent of Tauri without changing the shipping
Tauri/WebView behavior. A new neutral crate owns typed actions, typed events, generation fencing,
bounded/coalesced delivery, and a durable snapshot. Tauri and Slint are adapters over that contract;
neither framework is the application API.

This milestone does **not** port Library, Cloud, Settings, Games, microphone controls, or the full
Review UI. It establishes the state and event boundary those later milestones consume.

## Non-negotiable contracts

- `AppSettings` JSON, settings path, atomic persistence, normalization, credentials, cloud upload
  records, media paths, and recorder `Cmd` behavior remain unchanged.
- Existing Tauri command names, argument/result JSON, and event names/payload JSON remain compatible.
- Snapshot state is authoritative. A destroyed frontend rebuilds from one snapshot and never needs
  old transient events replayed in order.
- UI delivery is bounded. Coalescable state replaces older state; durable saved/error notices are
  never silently discarded. A full non-coalescable queue fails with a typed error.
- Recorder, microphone, detector, upload, enrichment, and lifecycle completions carry the producer
  generation whenever late work can otherwise overwrite current state.
- Framework callbacks own their data. Slint updates happen only through `invoke_from_event_loop`
  and weak component handles; Tauri emission exists only in its adapter.
- Neutral code builds and tests on Ubuntu and Windows. Tauri/Slint/Win32 imports stay out of the new
  crate.

## Task 1: Add the neutral contract crate, failing first

**Files**

- Modify: `Cargo.toml`
- Create: `crates/clipline-desktop/Cargo.toml`
- Create: `crates/clipline-desktop/src/lib.rs`
- Create: `crates/clipline-desktop/src/action.rs`
- Create: `crates/clipline-desktop/src/event.rs`
- Create: `crates/clipline-desktop/src/snapshot.rs`
- Create: `crates/clipline-desktop/tests/contracts.rs`

**Test first**

- Define serialization fixtures for `UiAction`, `UiEvent`, `WindowLifecycleSnapshot`, recorder
  status/save/error payloads, microphone monitor data, game detection, cloud upload progress, and
  enrichment invalidation.
- Pin explicit newtype generations/revisions and checked increment failure at `u64::MAX`; never use
  wrapping generations for stale-work ownership.
- Pin `UiAction::{SaveReplay, SetRecording, SetLifecycle, AcknowledgeNotice}` and typed effects. A
  controller dispatch returns an effect; it does not call Tauri or application services.
- Pin owned payloads: no references, `AppHandle`, Slint type, OS handle, or unbounded arbitrary map.

**Implement to green**

- Add `clipline-desktop` as a normal workspace member using only workspace `serde`, `serde_json`, and
  `thiserror`.
- Keep settings generic at the contract boundary: `DesktopController<S>` and
  `DesktopSnapshot<S>` carry the exact application settings type without duplicating or serializing
  it internally. Milestone 8 can relocate settings types later without changing this API.
- Keep event DTO field names compatible with current frontend JSON, including the recorder
  `kind` tag and optional save timestamps.

## Task 2: Implement the bounded/coalesced event port

**Files**

- Create: `crates/clipline-desktop/src/channel.rs`
- Create: `crates/clipline-desktop/tests/channel.rs`

**Test first**

- Capacity is a public constant and cannot grow at runtime.
- Recorder status, lifecycle, microphone samples, game detection, enrichment invalidation, and one
  cloud upload identity coalesce last-writer-wins without crossing a durable-event barrier.
- Saved replay and user-visible error events preserve order and never coalesce.
- Reserve capacity for a final lifecycle/background or terminal microphone event. If a queue full
  of non-coalescable durable events cannot accept another, return `Full { capacity }` and preserve
  the queue byte-for-byte.
- A disconnected consumer produces `Disconnected`; stale producer generations produce `Stale` and
  never enter the queue.
- Concurrent publishers cannot exceed the bound, duplicate sequence numbers, or reorder durable
  events.

**Implement to green**

- Provide a cloneable `UiEventSender` and single-consumer `UiEventReceiver` over one mutex/condvar
  state; do not use an unbounded standard channel.
- Sequence every accepted update monotonically and include its producer generation.
- Expose nonblocking `try_publish`/`try_recv` plus one bounded wait for adapter threads.
- Define `UiEventSink` as the framework-neutral injected producer interface.

## Task 3: Make the controller snapshot complete and rebuildable

**Files**

- Create: `crates/clipline-desktop/src/controller.rs`
- Create: `crates/clipline-desktop/tests/controller.rs`

**Test first**

- A fresh controller snapshot contains settings, lifecycle, recorder state, storage state, game
  state, microphone state, active upload state, library/enrichment revision, and startup/user
  notices.
- Applying every coalescable event mutates the snapshot and advances exactly one snapshot revision.
  An identical update is a no-op.
- Older lifecycle/microphone/game/upload/enrichment generations are rejected without mutation.
- Saved replay advances the durable library revision and stores the latest save summary; error and
  startup warnings become bounded acknowledgement-required notices.
- Notice and upload collections have hard limits and deterministic eviction. Never evict an
  unacknowledged error to make a result look successful; return a typed capacity error instead.
- `snapshot()` followed by construction of a new presentation model yields the same visible state
  as consuming the original accepted events. No event history is required.
- `AcknowledgeNotice` is idempotent and cannot acknowledge a notice from another generation.

**Implement to green**

- Store only durable/current state, not an event log.
- Publish immutable owned `DesktopSnapshot<S>` values. Keep `S: Clone`; do not require JSON as an
  internal representation.
- Separate snapshot revision from subsystem generations so a stale completion cannot become current
  merely because unrelated state advanced.

## Task 4: Introduce the Tauri adapter without changing frontend JSON

**Files**

- Create: `apps/clipline-app/src/desktop.rs`
- Create: `apps/clipline-app/src/desktop/tauri_sink.rs`
- Modify: `apps/clipline-app/src/main.rs`
- Modify: `apps/clipline-app/src/app.rs`
- Modify: `apps/clipline-app/Cargo.toml`
- Create: `apps/clipline-app/tests/desktop_contract.rs`

**Test first**

- Golden-serialize every new DTO and compare it with the current event payload shape for `status`,
  `saved`, `error`, `window-lifecycle`, `mic-test`, `mic-test-error`, `mic-test-stopped`,
  `game-detection`, `cloud-upload-progress`, and `osu-enrichment-updated`.
- Pin exact Tauri event-name mapping in one adapter table.
- Prove adapter publication updates the neutral snapshot before external emission, so reopening from
  a snapshot cannot lag an already-visible event.
- Add a repository contract that rejects direct emission of the migrated event names outside
  `desktop/tauri_sink.rs`.

**Implement to green**

- Manage a `DesktopState` containing `DesktopController<AppSettings>` and inject startup warnings at
  construction. Do not consume warnings with a one-shot `take()` during frontend bootstrap.
- Implement `TauriUiEventSink` as the only JSON/event-name adapter. It updates `DesktopState`, then
  emits the compatible payload.
- Keep shell-only Tauri operations such as reveal, updater, tray, dialogs, and quit in their current
  modules until their assigned milestones.

## Task 5: Extract recorder actions and events one vertical slice at a time

**Files**

- Modify: `apps/clipline-app/src/app.rs`
- Modify: `apps/clipline-app/src/service.rs` only where a conversion seam is needed
- Modify: existing recorder/app tests

**Test first**

- `UiAction::SaveReplay` preserves the 150 ms duplicate-trigger debounce, sender-unavailable result,
  and nonblocking service command.
- `UiAction::SetRecording` preserves games-only waiting state, generation invalidation, restart,
  rollback, and return value.
- Service generations fence late status/events after restart or stop.
- Status coalesces; save/error ordering and existing saved-sound/enrichment/storage side effects stay
  exact.

**Implement to green**

- Add one non-Tauri action dispatcher over the existing `RuntimeState` service ownership. Tauri
  command functions become signature/argument/result adapters only.
- Convert `service::Event` to neutral `UiEvent` once in the service pump and publish through the
  injected sink.
- Preserve media-root reconciliation and save side effects as application-domain handlers, not UI
  adapter behavior.

## Task 6: Replace the remaining direct producer emissions

**Files**

- Modify: `apps/clipline-app/src/app.rs`
- Modify: `apps/clipline-app/src/cloud.rs`
- Modify: `apps/clipline-app/src/osu_api.rs`
- Modify: relevant existing tests

**Test first**

- Lifecycle revisions are present in bootstrap and live updates; stale lifecycle updates are ignored.
- Microphone start/stop/device-loss events use one generation, bounded PCM samples, and terminal
  stop ordering. Background entry still stops monitoring synchronously.
- Game detector updates cannot overwrite a newer settings/detector generation.
- Cloud progress coalesces only within the same account/upload generation and local clip identity;
  completion/error remains represented in the snapshot.
- Enrichment invalidation increments one durable library revision and duplicate completion for the
  same generation is a no-op.
- User-visible errors from settings/recorder flows preserve current timing and string payloads.

**Implement to green**

- Pass `&dyn UiEventSink` or an owned cloneable sink into recorder, microphone, detector, upload,
  and enrichment producers. Producers must not know `AppHandle` for UI publication.
- Keep `AppHandle` parameters only where the same function genuinely performs a separate Tauri shell
  action; do not use that as a reason to emit directly.

## Task 7: Make bootstrap a single authoritative snapshot

**Files**

- Modify: `apps/clipline-app/src/app.rs`
- Modify: `apps/clipline-app/ui/main.js`
- Modify: `apps/clipline-app/tests/ui_contract.rs`
- Modify: `apps/clipline-app/tests/slint_migration_contract.rs`

**Test first**

- `frontend_ready` returns one versioned `DesktopSnapshot<AppSettings>` containing the current
  lifecycle and waiting/recording status even when those events happened before readiness.
- Calling bootstrap twice or after simulated window destruction returns current state without
  replaying/duplicating saved/error events.
- The JS adapter applies snapshot first, subscribes to later sequences, and reconciles a sequence gap
  by requesting a fresh snapshot.
- Existing frontend startup warning, lifecycle, recorder, and settings behavior stays green.

**Implement to green**

- Keep the legacy response fields for one migration release if existing JavaScript requires them;
  add the authoritative snapshot and sequence without changing current consumers atomically.
- Make `get_settings` read through the desktop snapshot so Tauri and future Slint observe the same
  settings revision.
- Remove `FRONTEND_READY` as application state; retain only diagnostic/watchdog bookkeeping that is
  truly WebView-specific.

## Task 8: Add the Slint event-loop adapter

**Files**

- Modify: `apps/clipline-slint-spike/Cargo.toml`
- Create: `apps/clipline-slint-spike/src/desktop.rs`
- Modify: `apps/clipline-slint-spike/src/lib.rs`
- Modify: `apps/clipline-slint-spike/src/main.rs`
- Create: `apps/clipline-slint-spike/tests/desktop_adapter.rs`
- Modify: `apps/clipline-slint-spike/tests/spike_contract.rs`

**Test first**

- A worker-thread event is accepted through the same bounded neutral channel and reaches Slint only
  through `invoke_from_event_loop`.
- Revision gating drops delayed closures after a newer snapshot.
- Closing/dropping the component makes weak-handle upgrade fail harmlessly and disconnects the
  consumer; no strong component is captured in a component callback or posted closure.
- `ModelRc` replacement happens on the UI thread and remains bounded to the visible model.

**Implement to green**

- Add only `clipline-desktop` to the spike; do not add Tauri, WebView, or app-binary dependencies.
- Adapt the Milestone 4 representative state through weak handles. Full Library/Settings models are
  deliberately deferred to their assigned milestones.

## Task 9: Document parity and validate the milestone

**Files**

- Modify: `docs/slint/parity-ledger.md`
- Modify: `handoff.md`

**Validation**

- Mark only the M5-owned command/event rows complete. For microphone/game/cloud/enrichment rows,
  record the event-sink infrastructure separately while leaving full surface parity pending for
  M7/M8.
- Run `cargo test -p clipline-desktop` and fresh-cache strict Clippy.
- Run the standalone Slint tests and strict Clippy.
- Run `cargo test --workspace` with device-test CI skips if the current display session cannot
  produce WGC frames, then strict workspace Clippy.
- Run `apps/clipline-app/tests/slint_migration_contract.rs` and the PowerShell helper tests.
- Launch the shipping Tauri app only after confirming no installed `clipline-app.exe` must be
  stopped. Because the user keeps the installed app open, do not terminate it; record the manual
  launch check pending if the build cannot safely replace or distinguish that process.
- Commit each extraction batch conventionally, push the branch, refresh draft PR #132, and require
  Ubuntu/Windows CI green before calling Milestone 5 complete.

## Stop conditions

- Stop the extraction batch if any Tauri payload/command behavior changes without an explicit parity
  decision.
- Stop if a supposedly neutral crate imports Tauri, Slint, Win32, credential, or filesystem shell
  APIs.
- Stop if a producer can block indefinitely on UI delivery, an event queue can grow without a hard
  bound, or a stale completion can update the durable snapshot.
- Keep Tauri shipping and rollback-capable until Milestone 10 cutover; never delete current frontend
  code during this milestone.
