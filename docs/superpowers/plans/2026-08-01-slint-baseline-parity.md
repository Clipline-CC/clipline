# Slint Milestone 1: Matched Baseline and Parity Ledger

> **For agentic workers:** Execute each task in order. Write the failing contract test before the
> implementation it protects. Checkboxes remain unticked by repository convention.

**Goal:** Make the Tauri-to-Slint decision measurable and auditable before adding Slint. This
milestone delivers a frontend-neutral Windows process-tree measurement harness, reproducible
playback fixtures, and a checked parity ledger derived from the current `develop` frontend.

**Scope:** Tooling, fixtures, documentation, and migration-contract tests only. Do not add Slint,
change shipping runtime behavior, or collect publishable performance numbers on an unmatched
developer setup.

**Baseline:** `origin/develop` at `5eea6c3` (Clipline 0.1.43). The implementation branch is
`agent/slint-frontend-replacement-plan`.

## Task 1: Freeze the milestone contract

**Files:**

- Create: `apps/clipline-app/tests/slint_migration_contract.rs`
- Create: `docs/slint/parity-ledger.md`
- Create: `docs/slint/baseline-protocol.md`

- [ ] Add a failing Rust integration test that extracts every entry from the production
      `tauri::generate_handler!` list and requires a corresponding command token in the parity
      ledger.
- [ ] Add required migration-surface assertions for emitted/listened events, pages, dialogs,
      shortcuts, tray actions, updater actions, and review gestures. Keep the authoritative token
      list readable in the test so additions fail with a useful missing-token message.
- [ ] Run `cargo test -p clipline-app --test slint_migration_contract` and confirm the missing
      documents/tokens fail for the intended reason.
- [ ] Build the ledger from the shipping Rust/HTML/JS/Tauri configuration. Each row must contain a
      stable ID, current source, behavior, owner milestone, acceptance method, and status.
- [ ] Document how to add a new current-frontend surface without silently bypassing the ledger.
- [ ] Re-run the contract test and confirm it passes.

## Task 2: Extract shared process-tree measurement primitives

**Files:**

- Create: `scripts/lib/Clipline.ProcessMetrics.psm1`
- Create: `scripts/test-frontend-baseline-tools.ps1`

- [ ] Add failing PowerShell self-tests for descendant traversal with PID reuse protection,
      medians/percentiles, CPU deltas, empty samples, and stable CSV column order. Tests use
      synthetic process rows and must not launch Clipline.
- [ ] Move the reusable Windows memory reader and process-tree logic into a module without changing
      the existing two measurement scripts yet. Native APIs are loaded lazily so the pure helpers
      remain testable and syntax-checkable off Windows.
- [ ] Collect root and descendant private working set, private commit, ordinary working set,
      cumulative CPU time, handle count, and process count. Keep GPU local/non-local allocation as
      optional counters because driver support varies.
- [ ] Run the self-test script under PowerShell and syntax-parse all changed `.ps1`/`.psm1` files.

## Task 3: Add the matched frontend baseline harness

**Files:**

- Create: `scripts/measure-frontend-baseline.ps1`
- Modify: `docs/slint/baseline-protocol.md`

- [ ] Add the scenario matrix: cold autostart tray, Local Library with 50/500/2,000 entries,
      Settings, H.264 Review idle, H.264 Review playing, scrub storm, close-to-tray, and 100
      reveal/close cycles.
- [ ] Separate sampling from frontend driving. The initial Tauri driver may use CDP, but the output
      schema and sampler cannot encode WebView-only assumptions; a later Slint driver must emit the
      same phase/ready markers.
- [ ] Emit raw long-form samples plus a metadata JSON sidecar recording executable SHA-256, git
      commit, frontend/renderer, scenario, fixture hashes, OS/build, CPU, GPU/driver, display scale,
      run timing, and harness version.
- [ ] Summarize five-minute steady windows with median/p95 values while retaining raw samples.
      First-usable latency is measured from process creation to a driver readiness marker.
- [ ] Fail closed when the executable, fixture, process identity, requested scenario, or required
      readiness marker is missing. Never rewrite the user's settings or media directory.
- [ ] Document exact matched-run discipline and the distinction between process-tree private
      working set, private commit, ordinary working set, and Task Manager's grouped headline.

## Task 4: Create reproducible playback fixtures

**Files:**

- Create: `scripts/generate-playback-fixtures.ps1`
- Create: `fixtures/playback/README.md`
- Create: `fixtures/playback/manifest.json`
- Modify: `.gitignore`

- [ ] Add failing generator self-validation for required streams, dimensions, duration, GOP
      pattern, audio-track count, and manifest hash coverage.
- [ ] Generate media from first-party procedural video/audio only, through the existing separately
      spawned FFmpeg boundary. Do not link FFmpeg or add GPL dependencies.
- [ ] Cover H.264 High + one Opus track, H.264 High + output/microphone Opus tracks and Clipline
      marker sidecar, long GOP, and variable-frame-content. Keep HEVC/AV1 optional capability
      fixtures clearly non-gating.
- [ ] Pin every encoder/mux input affecting reproducibility, record the FFmpeg build/version and
      exact command template, and write SHA-256 hashes. Generated media stays ignored; the manifest,
      marker sidecars, provenance, and generator are committed.
- [ ] Re-run generation and validation twice; require identical hashes with the same FFmpeg binary.

## Task 5: Verify and hand off

**Files:**

- Modify: `handoff.md`

- [ ] Run the migration contract test, PowerShell self-tests, and PowerShell parser checks.
- [ ] Run `cargo test --workspace`.
- [ ] Run `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Have an independent reviewer check the harness for metric mismatch, PID reuse, profile
      mutation, and false readiness; check the ledger for material omissions.
- [ ] Update `handoff.md` with commands, produced artifacts, known manual steps, and the next gate.
- [ ] Commit the implementation in logical conventional commits and push the branch. Do not report
      a Slint memory estimate as measured until matched raw runs exist.
