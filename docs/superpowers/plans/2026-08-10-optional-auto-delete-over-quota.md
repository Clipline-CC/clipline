# Optional auto-delete when over quota

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans or
> superpowers:subagent-driven-development to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** Keep Clipline's default saved-media quota lock non-destructive, but let users opt into
oldest-first auto-delete from Settings when they want the classic recorder behavior.

**Exit criterion:** Settings → Storage exposes `auto_delete_when_over_quota` (off by default).
When enabled, quota checks free oldest managed clips before locking recording; active recordings
and in-progress uploads stay protected; the Library refreshes after background cleanup; emptied
session folders including `clipline-session.json` are removed; concurrent rename/delete cannot
lose sidecars if the MP4 disappears mid-collection.

**Architecture:** Restore `clipline-storage::enforce_quota(_with_protection)` as an explicit
collector used only when the setting is on. The recorder service passes the flag into startup,
replay-save, full-session reserve, and once-per-second full-session checks. Full-session checks
update the cached `saved_media_baseline_bytes` after cleanup and emit `LibraryChanged` so the
UI refreshes without waiting for a save.

**Tech Stack:** existing Rust workspace + vanilla Settings UI.

---

### Task 1: restore protected oldest-first GC

**Files:** `crates/clipline-storage/src/lib.rs`

- [ ] Add failing tests for oldest-first deletion, upload/recording protection, session-folder
  cleanup, session-metadata removal, and skipping sidecar deletes when the MP4 is already gone.
- [ ] Reintroduce `GcReport` / `enforce_quota_with_protection`.
- [ ] Before removing an emptied session directory, delete `clipline-session.json` when no
  managed clips remain.
- [ ] If `remove_file` on the inventoried MP4 returns `NotFound`, skip sidecar cleanup.

### Task 2: settings + service wiring

**Files:** `apps/clipline-app/src/settings/*`, `apps/clipline-app/src/service.rs`,
`apps/clipline-app/src/app.rs`, `apps/clipline-app/src/app/support.rs`

- [ ] Persist `auto_delete_when_over_quota: bool` (default false).
- [ ] Thread it through `ServiceOptions` and every quota preflight.
- [ ] On full-session auto-delete success, refresh `saved_media_baseline_bytes`.
- [ ] Emit `Event::LibraryChanged` whenever auto-delete removes clips.

### Task 3: UI + contracts

**Files:** `apps/clipline-app/ui/index.html`, `apps/clipline-app/ui/settings.js`,
`apps/clipline-app/ui/main.js`, `apps/clipline-app/tests/ui_contract.rs`

- [ ] Add the Storage toggle and update quota copy / first-run text.
- [ ] Listen for `library-changed` and refresh the Library.
- [ ] Guard the DOM/JS contract.

### Task 4: docs

**Files:** `ddoc.md`, `handoff.md`

- [ ] Describe the opt-in collector instead of claiming clips are never removed automatically.

### Task 5: verify

- [ ] Focused storage/service/settings/UI tests.
- [ ] Launch the local debug build and confirm the Settings toggle.
