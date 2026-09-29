# Reliable playback audio sources

> **For agentic workers:** Execute this plan task-by-task. Checkboxes remain
> unticked by repository convention.

## Goal

Delete Experimental app audio tracks and replace the single playback-device
setting with a supported ordered list of Windows playback endpoints. Each
configured endpoint records as its own Opus track, and a clip remembers which
tracks the user chose for playback and publishing.

This is two ordered milestones: remove process-loopback capture first, then add
multi-endpoint loopback. Keep the existing microphone as one separate input.

## Product decisions

- Record playback **endpoints**, not drivers or process trees.
- Keep `AudioSource`, the replay ring, Hybrid MP4, sidecars, native Opus mixing,
  and the existing review track list; they already support N audio tracks.
- Keep a top-level output on/off switch. When enabled, record every configured
  playback source; per-clip checkboxes decide what is heard or published later.
- Support at most 16 configured playback sources. This bounds malformed settings
  and accidental resource exhaustion while leaving ample room for virtual mixers.
- A lone source may follow the Windows default output. Multiple sources must all
  select explicit endpoints, preventing an implicit default from duplicating one
  of them.
- Explicit endpoints never fall back to the default device. A source unavailable
  at startup owns a silent track and retries the same endpoint, so track indices
  stay fixed and later recovery cannot capture a mislabeled device.
- Applying source changes uses the existing recorder restart transaction. Do not
  hot-add or remove MP4 tracks inside a running recorder.
- Track metadata uses ordered local ids (`playback:0`, `playback:1`, ...), kind
  `playback_endpoint`, and the saved device label. IDs are clip-local; no new UUID
  or source registry is needed.
- Preserve review behavior for old `process_output` clips, but never create new
  process tracks.
- Persist a clip's selected track ids. `None` means legacy/default selection;
  `Some([])` means intentionally muted.

## Non-goals

- Per-process capture, dynamic Windows audio-session discovery, or a mixed-output
  safety track.
- A Sonar/Voicemeeter replacement, virtual audio driver, application routing,
  stream/personal mixes, EQ, spatial processing, or microphone DSP.
- Multiple microphone sources, per-source monitoring, live meters, or source
  changes without restarting the replay buffer.
- A Windows 11-only stable endpoint identity layer. Preserve opaque endpoint ids,
  show unavailable saved devices, and let the user relink them explicitly.
- Redesigning the existing selected-track Opus mix gain policy.

## Milestone A: remove Experimental app audio tracks

### Task A1: Replace accepting tests with removal contracts

- [ ] Update settings tests so legacy `split_output_by_process` input is ignored,
      is not present in defaults or service options, and is not serialized again.
- [ ] Update UI contracts to require that Settings and first-run contain no
      Experimental app audio tracks checkbox, ids, copy, or event wiring.
- [ ] Add a repository contract that production capture code contains no process-
      loopback activation constants, render-session enumeration, or public process
      audio APIs while ordinary Windows process APIs used elsewhere remain allowed.
- [ ] Keep player tests proving historical `output` + `process_output` marker
      sidecars still render, toggle, preview, trim, share, and upload correctly.
- [ ] Run the focused tests and confirm they fail against the current feature.

### Task A2: Remove the app-facing feature

- [ ] Remove `split_output_by_process` from `AudioSettings`, `AudioOptions`,
      validation/conversion, diagnostics snapshots, defaults, and fixtures.
- [ ] Remove the Settings and first-run toggles plus all JavaScript serialization,
      rendering, disabled-state, recommendation, and event-listener code.
- [ ] Simplify `audio_sources_from_options` to attach one mixed endpoint source
      when output is enabled, followed by the existing microphone source.
- [ ] Delete process-candidate filtering and fallback warnings from the service.
- [ ] Preserve unknown legacy JSON fields on load by ignoring them, not by keeping
      a dormant setting or compatibility branch.

### Task A3: Delete native process-loopback capture

- [ ] Remove `AudioProcessInfo`, render-session enumeration/grouping, process
      identity snapshots, async process-loopback activation, availability/build
      gates used only by that path, and their tests.
- [ ] Remove `EndpointTarget::ProcessOutput`, process-specific start methods,
      constants/imports/exports, timeout handling, and process identity recovery.
- [ ] Retain endpoint loopback, microphone capture, PCM conversion, Opus encoding,
      device-loss silence/retry, and `windows_build_number` where support tooling
      still uses it.
- [ ] Delete the now-unused WASAPI process module instead of leaving scaffolding
      for a speculative future audio service.

### Task A4: Verify removal

- [ ] Run focused settings, service, WASAPI, player-core, and UI-contract tests.
- [ ] Run `cargo test --workspace` and warning-denied workspace Clippy.
- [ ] Launch Clipline and verify output + microphone recording and historical
      split-track clip playback still work.
- [ ] Update `ddoc.md` current audio architecture and append the removal checkpoint
      to `handoff.md`; do not rewrite historical handoff entries.
- [ ] Commit the removal as one conventional logical change.

## Milestone B: add supported playback-source lists

### Settings shape

Persist an ordered `playback_sources` list under `audio`. Each entry contains only:

- `device_id: Option<String>` — `None` is the single permitted Default output.
- `label: String` — the last known Windows friendly name for UI and clip metadata.
- `volume: f64` — the existing finite `0.0..=2.0` recording gain.

Keep `output_enabled` and every existing microphone field. The list order defines
the MP4 audio order before the microphone.

Migration rules:

- If `playback_sources` is absent, map legacy output device/volume into one source
  and preserve `output_enabled` and all microphone settings.
- If `playback_sources` is present, it is authoritative, including an empty list.
- Reject more than 16 entries, duplicate explicit endpoint ids, multiple Default
  entries, Default mixed with explicit sources, blank/oversized ids or labels,
  embedded NULs, and non-finite/out-of-range gain.
- Preserve endpoint ids byte-for-byte; never parse them or recover by friendly name.

### Task B1: Drive settings and service types with failing tests

- [ ] Add default, legacy migration, explicit-empty, round-trip, validation, and
      `to_service_options` tests for the list above.
- [ ] Cover one Default source, multiple explicit sources, duplicate endpoints,
      Default-plus-explicit rejection, 16 accepted/17 rejected, malformed JSON,
      unavailable saved labels, and output-disabled list preservation.
- [ ] Add service construction tests proving source order and metadata order are
      playback sources first, microphone last.
- [ ] Run the focused tests red before implementing the model.

### Task B2: Implement the source-list settings UI

- [ ] Replace the one output selector/range with an accessible list of source rows:
      device selector, volume, and Remove; add one `Add playback source` button.
- [ ] Reuse `list_audio_devices`, device-select filling, range styling, dirty-state
      indicators, settings drafts, and the existing save/restart transaction.
- [ ] Show a saved missing endpoint with its last known label and an unavailable
      state. Selecting a replacement updates that row; never match by name.
- [ ] Prevent adding a second row while the first follows Default; require choosing
      an explicit endpoint first. Prevent duplicate endpoint choices in the UI and
      repeat validation in Rust.
- [ ] Keep first-run minimal: one Default output source at 100%. Advanced source
      list editing remains in Settings after onboarding.
- [ ] Add UI-contract and pure-JavaScript tests for add/remove, list serialization,
      unavailable rows, duplicate prevention, output toggle state, keyboard focus,
      and first-run migration into the normal settings draft.

### Task B3: Make endpoint activation strict and startup-resilient

- [ ] Add failing Windows tests proving an invalid explicit endpoint never opens
      the default endpoint, while a Default source still uses `eConsole`.
- [ ] Separate explicit and Default activation policy; remove the existing selected-
      endpoint startup fallback.
- [ ] Extend the endpoint audio source with a dormant initial state: fixed Opus
      track config and timeline-aligned silence while activation fails, retrying
      the same target on the existing one-second cadence.
- [ ] On recovery, preserve the same assembler, Opus track, and metadata index;
      re-anchor on the first timestamped packet and keep sibling tracks/video alive.
- [ ] Keep invalid buffers, overflow, unsupported formats, and Opus failures fatal;
      only endpoint absence/invalidation becomes silence-and-retry.
- [ ] Give loss/retry/recovery diagnostics enough source identity to distinguish
      several playback endpoints without logging opaque device ids.
- [ ] Add tests for unavailable-at-start recovery, mid-session invalidation,
      changed endpoint format, retry cadence, strict target reuse, and shutdown of
      a dormant source.

### Task B4: Attach N endpoint tracks end to end

- [ ] Change `audio_sources_from_options` to construct one endpoint source for each
      validated row, with shared `RelativeClock`, row gain, fixed ordering, and
      `ClipAudioTrack { id: "playback:N", kind: "playback_endpoint" }` metadata.
- [ ] Keep microphone construction unchanged and append it after playback tracks.
- [ ] Ensure RAM replay, disk replay, full-session output, marker sidecars, trim,
      scan/inference, native mixing, sidecars, and upload accept 4+ playback tracks
      without introducing source-count special cases.
- [ ] Add distinguishable multi-source fixtures that validate content/order/timing,
      not merely MP4 track count, across replay and full-session paths.
- [ ] Measure replay retention with four endpoints plus microphone. Adjust byte
      budgeting only if the existing two-times video headroom fails the configured
      window; do not add a speculative bitrate model.

### Task B5: Persist and honor per-clip track inclusion

- [ ] Add backward-compatible `selected_audio_track_ids: Option<Vec<String>>` to
      `ClipMarkers`, with validation against `audio_tracks` and explicit empty mute.
- [ ] Add one Tauri command that atomically updates the marker sidecar under the
      existing clip-mutation lock; create a minimal sidecar when a legacy clip has
      inferred tracks but no marker file.
- [ ] Restore saved selection when opening a clip and persist checkbox changes
      independently of preview preparation success.
- [ ] Resolve effective selection consistently as explicit operation request, then
      saved clip choice, then existing legacy defaults.
- [ ] Preserve track metadata and saved selection through keyframe-aligned trims,
      including trims that exclude game-event markers.
- [ ] Make normal clipboard copy, individual upload, and group compilation honor
      the effective selection. Keep `Copy original` as the explicit all-recorded-
      tracks escape hatch; remove the implicit over-five-minute original bypass.
- [ ] Include effective member selections in compilation inputs/fingerprints so a
      changed choice cannot reuse an older mix. A muted member uses aligned silence.
- [ ] Add race/rollback tests for selection writes alongside marker enrichment,
      rename/delete, upload leases, and compilation publication.

### Task B6: Acceptance and handoff

- [ ] Run focused settings, service, WASAPI, pipeline, MP4, sidecar, player-core,
      trim/share/upload/group, and UI-contract tests.
- [ ] Run `cargo test --workspace`.
- [ ] Run fresh-cache warning-denied Clippy for changed crates, then
      `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] On Windows, record distinct signals through at least three playback endpoints
      plus microphone; verify labels, ordering, selection persistence, mute, trim,
      copy, upload payload, and group compilation.
- [ ] Start with one configured endpoint unavailable, confirm it never captures the
      default, then enable it and verify aligned recovery into the same track.
- [ ] Disable/re-enable one endpoint during recording and confirm video and sibling
      audio continue with silence only on the affected track.
- [ ] Run a 60-minute 4+mic game-load soak and compare early/late A/V and inter-track
      sync, CPU, memory, replay span, save latency, and stop latency.
- [ ] Open old process-track clips and confirm their legacy selection semantics still
      work even though no new process tracks can be recorded.
- [ ] Update `ddoc.md` and `handoff.md`, rebuild, launch Clipline, and give the user
      the focused manual checklist before committing the final logical changes.

