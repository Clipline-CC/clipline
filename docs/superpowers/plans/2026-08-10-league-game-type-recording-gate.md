# League Game-Type Recording Gate Plan

**Goal:** Let users choose which League of Legends game types are recorded automatically. For
example, record every Ranked Solo/Duo match while skipping Normal games entirely — no capture, no
session file, no match markers.

## Product decisions

- Reuse the shipped queue enrichment: the game type is known from the local League client
  (`LcuClient` lockfile lookup + `LeagueQueue::from_id`) by the time the in-game process launches,
  so the decision is made at game detection, before any footage exists. No Riot account, remote
  API, or cloud lookup.
- The gate applies to **automatic** recording of a detected League game. Manual session start
  (button or hotkey) always bypasses the gate — explicit user intent, mirroring how manual
  sessions already bypass the games-only wait.
- Per-category flags default to record-all so upgrades preserve current behavior. Categories are
  the existing stable set: Ranked Solo/Duo, Ranked Flex, Normal, ARAM, Arena, Custom, Other.
  Lookup failure gets its own `record unknown` policy, defaulting to **record** (queue enrichment
  must never interrupt recording).
- The gate decision is read once per detected game. Changing the toggles mid-match only affects
  the next detected game.
- A disallowed game: the recorder never starts (replay buffer included), any recorder still
  running for a previous game is stopped, and one notice explains why. Non-League games and the
  existing pause/games-only semantics are untouched.
- One bounded lookup attempt at detection (~1 s); a failure means Unknown and the policy decides.
  The poller's existing per-match enrichment keeps running for allowed games and still tags
  Unknown when its own lookup fails.
- Do not add a database, per-match UI overrides, a configurable queue catalog, or changes to the
  shipped library Game type filter.

## Minimal architecture

New `settings/league.rs` module (mirroring `settings/osu.rs` precedent): `LeagueModeSettings` with
one record flag per category plus `record_unknown`, held as a `#[serde(default)]` field on
`AppSettings`. Backward-compatible load: missing field means record-all.

Gate flow in the runtime: when detection reports a League game whose `exe_path` is known, spawn a
one-shot lookup thread (same pattern as `markers::spawn` — current-thread tokio runtime,
`LcuClient::from_game_executable` + `current_queue`, honoring the existing `lol_url` test
override). Defer the recorder start for that game until the result arrives — the game is still on
the load screen, so nothing meaningful is missed. Allowed or Unknown-record: the existing start
path. Denied: skip start, stop a running recorder via the existing restart/stop machinery, emit a
notice.

The pure decision lives in `settings/league.rs`:
`league_gate_allows(category: Option<&LeagueQueueCategory>, settings: &LeagueModeSettings) -> bool`.
UI renders the toggles in the League section of Settings, next to the existing
`Show League match details` row.

## Plan-driven implementation

### Task 1: Lock gate contracts with failing tests

- [ ] Settings tests: default record-all, backward-compatible load of older settings files,
      per-category flag round-trip through persistence.
- [ ] Table tests for `league_gate_allows`: each category allowed/denied, Unknown honored under
      both `record_unknown` policies.
- [ ] One-shot lookup wrapper test with `httpmock` (existing `LcuClient` test pattern): queue 420
      resolves to Ranked Solo/Duo; failure resolves to `None` within the attempt bound.
- [ ] App runtime tests: League detected with Normal disabled and Solo/Duo enabled — recorder
      starts for Solo/Duo, skipped for Normal with a notice; a recorder already running for a
      previous game is stopped on a denied detection; manual session start bypasses the gate;
      non-League games never consult the gate.

### Task 2: Settings and UI

- [ ] Add `settings/league.rs` with `LeagueModeSettings`, wire it into `AppSettings` with
      `#[serde(default)]`, and cover validation and persistence.
- [ ] Render the League section in Settings: one toggle per category plus Unknown, with
      explanatory text; hidden or disabled when the League plugin is disabled.
- [ ] Add `tests/ui_contract.rs` / settings UI coverage: toggles render, persist, and round-trip
      through the settings save path.

### Task 3: Gate at detection

- [ ] Runtime: on League detection, kick the one-shot lookup and defer recorder start until the
      decision; cache the decision per detected game so re-detection polls do not re-lookup.
- [ ] Denied path: do not start the recorder, stop a running one, and emit the skip notice.
- [ ] Unknown path: apply `record_unknown`; lookup failures are warning-only logs.
- [ ] Manual session start bypasses the gate; game exit before the lookup resolves cancels cleanly.

### Task 4: Verify and hand off

- [ ] Run focused crate and UI tests while iterating.
- [ ] Run `cargo test --workspace`.
- [ ] Run `cargo clippy --workspace --all-targets -- -D warnings` with a clean cache for changed
      crates.
- [ ] Update `handoff.md` and `ddoc.md`, commit the implementation, and relaunch Clipline for
      manual testing (solo/duo records, Normal skipped, Unknown policy honored).
