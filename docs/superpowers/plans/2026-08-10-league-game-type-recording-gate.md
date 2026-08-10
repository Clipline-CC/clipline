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
  sessions already bypass the games-only wait. Manual stop likewise stays stopped until the next
  detection.
- Per-category flags default to record-all so upgrades preserve current behavior. Categories are
  the existing stable set: Ranked Solo/Duo, Ranked Flex, Normal, ARAM, Arena, Custom, Other.
  Lookup failure gets its own `record unknown` policy, defaulting to **record** (queue enrichment
  must never interrupt recording).
- The gate decision is read once per detected game. Changing the toggles mid-match only affects
  the next detected game; a toggle changed while a lookup is pending is honored when the verdict
  is computed (verdict evaluates **current** settings at resolution time, not at kick time).
- On a new League detection the previous recorder is torn down **immediately** — the pending
  window never captures the new game. Only the replacement spawn is deferred until the gate
  verdict. A denied game: no recorder, one notice explaining why. Non-League games and the
  existing pause/games-only semantics are untouched.
- The verdict is cached per detection as `Pending` / `Allowed` / `Denied` and **every automatic
  start path** (game detection, settings-save restart, autostart resume) consults it, so a save
  while pending/denied cannot sneak a recorder up. `Unknown` folds into Allowed/Denied per
  `record_unknown` at verdict time.
- One lookup attempt with the explicit bound LcuClient already has (connect 1 s, read 2 s, total
  2 s — `lcu.rs`); a failure means Unknown and the policy decides. The poller's existing
  per-match enrichment keeps running for allowed games and still tags Unknown when its own lookup
  fails.
- Do not add a database, per-match UI overrides, a configurable queue catalog, or changes to the
  shipped library Game type filter.

## Minimal architecture

New `settings/league.rs` module (mirroring `settings/osu.rs` precedent): `LeagueModeSettings` with
one record flag per category plus `record_unknown`, held as a `#[serde(default)]` field on
`AppSettings`. Backward-compatible load: missing field means record-all. Defaults and
save/load round-trip are the persistence contract (boolean-only settings need no extra
validation).

Runtime gate: a per-detection state machine on the runtime, `league_gate: Option<LeagueGate>`
with `{ game identity, verdict: Pending | Allowed | Denied }`.

- Detection of a League game with a configured gate: clear any prior state, tear down the old
  recorder sender immediately, record `Pending`, and kick a one-shot lookup thread (same pattern
  as `markers::spawn` — current-thread tokio runtime, `LcuClient::from_game_executable` +
  `current_queue`).
- The lookup is injected through a spawn seam so tests control resolution timing: the runtime
  calls `spawn_gate_lookup(exe_path, result_tx)`, and tests supply a resolver they hold open for
  a deliberately pending case. `lol_url` is **not** reused — it overrides only the
  unauthenticated Live Client endpoint (`ServiceOptions.lol_url`), while LCU reads credentials
  from the lockfile (`LcuClient::from_game_executable`).
- Verdict on resolution: `league_gate_allows(category, &settings.league)` computed from **current**
  settings; `Pending` → `Allowed` starts the deferred replacement, `Denied` skips it and emits the
  notice. Game exit or a different game detection clears the state.
- All automatic start paths (`recorder_should_run` callers: detection event, settings-save
  restart via `prepare_settings_restart`, autostart resume) treat `Pending` and `Denied` as
  "do not start"; `Allowed` as today.

## Plan-driven implementation

### Task 1: Lock gate contracts with failing tests

- [ ] Settings tests: default record-all, backward-compatible load of older settings files,
      per-category flag and `record_unknown` round-trip through save/load persistence.
- [ ] Table tests for `league_gate_allows`: each category allowed/denied, Unknown honored under
      both `record_unknown` policies.
- [ ] Lookup wrapper test: **bounded failure maps to Unknown** (gate consumes `None` correctly).
      Skip the queue-420 httpmock case — it already exists in `clipline-lol/tests/client_http.rs`.
- [ ] Runtime tests with a held-open resolver:
      - detection tears down the old sender immediately while the lookup is pending (no capture
        of the new game), then resolves Allowed → replacement starts, Denied → no replacement and
        a notice;
      - settings save while Pending and while Denied does not start a recorder;
      - toggle change mid-lookup is honored at resolution (verdict uses current settings);
      - manual session start bypasses Pending/Denied; manual stop stays stopped until the next
        detection;
      - non-League games never consult the gate; game exit clears the cached verdict.

### Task 2: Settings and UI

- [ ] Add `settings/league.rs` with `LeagueModeSettings`, wire it into `AppSettings` with
      `#[serde(default)]`, and cover defaults and persistence round-trip.
- [ ] Render the League section in Settings: one toggle per category plus Unknown, with
      explanatory text; hidden or disabled when the League plugin is disabled.
- [ ] Add `tests/ui_contract.rs` / settings UI coverage: toggles render, persist, and round-trip
      through the settings save path.

### Task 3: Gate at detection

- [ ] Runtime: per-detection `league_gate` state consulted by every automatic start path;
      immediate sender teardown on League detection, deferred replacement spawn gated on the
      verdict.
- [ ] `spawn_gate_lookup` seam: production wraps `LcuClient::from_game_executable` +
      `current_queue` in a current-thread tokio runtime; tests inject a controllable resolver.
      Lookup failures are warning-only logs and resolve to Unknown.
- [ ] Denied path: no recorder, stop nothing further (already torn down), emit the skip notice.
- [ ] Manual session start bypasses the gate; game exit before the lookup resolves cancels
      cleanly and clears the state.

### Task 4: Verify and hand off

- [ ] Run focused crate and UI tests while iterating.
- [ ] Run `cargo test --workspace`.
- [ ] Run `cargo clippy --workspace --all-targets -- -D warnings` with a clean cache for changed
      crates.
- [ ] Update `handoff.md` and `ddoc.md`, commit the implementation, and relaunch Clipline for
      manual testing (solo/duo records, Normal skipped, Unknown policy honored).
