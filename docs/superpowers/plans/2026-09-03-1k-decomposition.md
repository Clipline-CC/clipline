# 1k Decomposition Program — master plan

Full thermos-baselined refactor: decompose every production file over 1000 lines
(thermos quality-rubric 1k rule) plus mechanical cleanup. Workers run on
`opencodex/opencode-zen/muse-spark-1.3-contributor-free` (inherited session model —
free, 1M context). Thermos double-review gates every batch.

## Non-goals (explicit)

- `third-party/shiguredo_opus` — vendored, excluded.
- Test files (`ui_contract.rs`, `player_core.rs`, `settings/tests.rs`,
  `repository_security.rs`) — low split value, excluded.
- `ponytail:` process-wide locks (`gc.rs:13`, `storage/lib.rs:55`,
  `library/groups.rs:508`) — deliberate ceilings, keep.
- Legacy-compat shims (`storage/empty_sessions.rs:272`, `storage/lib.rs:579`,
  `cloud.rs:770,781`) — need migration/retention decision, not blind delete.
- `normalize_*` family (~13 fns) and `save_window` parity layers — per-domain
  keep unless a worker proves a safe consolidation.
- `main.js` (exactly 1000) — stays unless it grows.

## Batches

- [ ] Phase 0 — mechanical: quota pair merge (`settings/persistence.rs:536,549`),
  facade prune-or-justify (`settings/mod.rs:34,46`), `main.rs:31` mod gate check.
- [ ] Group A — app shell: `app.rs` (7389) + `app/` dir. Window-lifecycle dead
  scaffolding (`app.rs:72,445,451,480,527`) delete or `cfg(test)`.
- [ ] Group B — library domain: `library.rs` (5668) + `library/groups.rs` (1071).
- [ ] Group C — recorder service: `service.rs` (4586).
- [ ] Group D — cloud sync: `cloud.rs` (3709) + `cloud_upload.rs` (2134).
- [ ] Group E — mp4: `trim.rs` (3633) + `writer.rs` (1627). Invariants: 4-byte
  length-prefixed NALs, B-frames disabled (no ctts), version-1 duration boxes.
- [ ] Group F — capture pipeline: `pipeline.rs` (3158) + `ffmpeg_encoder.rs` (1019).
  Invariants: one shared D3D device + one RelativeClock; `unsafe` stays in
  `windows/` behind safe wrappers.
- [ ] Group G — windows media: `wasapi.rs` (2224) + `mft.rs` (1452). Same
  invariants as F; 48 kHz float mix format.
- [ ] Group H — storage: `storage/lib.rs` (1725). Process-wide
  SESSION_MUTATION_LOCK stays.
- [ ] Group I — game integrations: `osu_enrichment.rs`, `osu_api.rs`,
  `game_discovery.rs`, `game_plugins.rs`.
- [ ] Group J — UI JS: `settings.js`, `library.js`, `review-player.js`,
  `player-core.js`. Player math stays in `player-core.js` (DOM-free, boa-tested);
  `[hidden]` needs explicit `display:none` per stacked view.
- [ ] Group K — `ffmpeg_install.rs` (1075). `clear_cancel` dead path: exercise or
  remove. SVT-AV1 takes no `-maxrate`/`-bufsize`; AMF rejects tiny resolutions.
- [ ] Integration pass — apply cross-group caller updates workers report.
- [ ] Gates — `cargo test --workspace`, `cargo clippy --workspace --all-targets
  -- -D warnings` (with `cargo clean -p` on changed crates first).
- [ ] Thermos gate — both review subagents on the batch diff, fix findings.
- [ ] Land — one conventional commit per group, push, CI green both OSes,
  update `handoff.md`.

## Worker contract

One owned file set per worker, no outside edits (report needed external caller
updates as return value). Proposal to `local://cleanup/<group>.md` first, then
implement. No commits, no test/clippy runs (parent runs gates once). Keep
neutral logic neutral and testable both OSes; Windows-only behind
`#[cfg(windows)]`. No GPL code (FFmpeg stays a spawned process).
