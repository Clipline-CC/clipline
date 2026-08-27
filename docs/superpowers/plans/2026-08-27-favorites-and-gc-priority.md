# Favorites + kind-ordered auto-delete

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans or
> superpowers:subagent-driven-development to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** Two storage/library features:

1. **Favorites.** A clip a user wants to keep can be marked as a favorite from Review and from
   the Library card context menu. A **Favorites** chip in the Library filter row isolates them.
   Favorited clips are never auto-deleted by quota GC.
2. **Auto-delete priority.** When Settings → Storage → Auto-delete oldest clips is on, quota GC
   deletes oldest-first *within* kind, but kinds are drained in order: **Sessions → Replay →
   Trim** (sessions first, trims last).

**Exit criterion:** A replay can be favorited (heart button in Review + context-menu entry on
cards + heart badge on favorited cards), the Favorites chip filters the local Library to
favorited clips, favorites survive quota auto-delete, and an over-quota library frees room by
deleting all candidate sessions before replays and replays before trims, oldest within each
kind.

**Architecture:**

- The per-clip metadata sidecar (`<clip>.clipline.json`) already owns `title` and `kind`;
  it gains `favorite: bool` (`#[serde(default, skip_serializing_if = "is_false")]`, so
  non-favorite sidecars stay byte-identical). `ClipInfo` (the Library listing payload) gains
  `favorite` so cards, Review, and the filter chip see it without extra round trips.
- New Tauri command `set_clip_favorite(path, favorite)` read-modifies-writes that sidecar and
  returns `{ path, favorite }`; the UI patches the cache in place (same pattern as rename).
- Favorites are enforced at GC time by the app's protection closure (reading the sidecar), not
  by storage: `clipline-storage::enforce_quota_with_policy` gains a caller-supplied
  `priority: impl Fn(&Path) -> u8` sort key; `enforce_quota_with_protection` delegates with a
  constant priority so existing callers are unchanged. Storage stays neutral — kind meaning
  (`session` 0, `replay` 1, `trim` 2) and favorite meaning live in the app
  (`library.rs::clip_gc_priority`, `is_favorite_clip`, shared `enforce_quota_with_clip_policy`).
- All three quota-GC call sites (`service.rs::make_room_for_quota`, the full-session check, and
  `app.rs::recheck_storage_quota`) switch to the shared app policy, so recorder GC, replay-save
  GC, and manual recheck GC all honor favorites + kind priority.

**Tech Stack:** existing Rust workspace + vanilla Settings/Library UI.

---

### Task 1: storage gains a priority-ordered collector

**Files:** `crates/clipline-storage/src/lib.rs`

- [ ] Add failing test: with mixed kinds and a quota that frees only part of the library, clips
  sort by `(priority, modified, file name)` — a newer session deletes before an older replay,
  a newer replay before an older trim.
- [ ] Add `enforce_quota_with_policy(dir, quota_bytes, protect, additionally_protected,
  priority)`; keep `enforce_quota_with_protection` delegating with `|_| 0`.
- [ ] Sort `clips` by `priority(path).cmp()` then the existing `(modified, file_name)` tiebreak.

### Task 2: app clip policy + favorite command

**Files:** `apps/clipline-app/src/library.rs`, `apps/clipline-app/src/app.rs`

- [ ] Failing tests: metadata round-trip preserves `favorite`; non-favorite metadata serializes
  byte-identically to today; `set_clip_favorite_impl` sets and clears the flag on an owned clip
  and validates the path like `delete_clip`.
- [ ] `ClipMetadata.favorite` with `skip_serializing_if = "is_false"`; `ClipInfo.favorite`
  populated in `push_clips_from`.
- [ ] `pub(crate) fn clip_gc_priority(path) -> u8` (session 0 / replay 1 / trim 2, via
  `clip_kind_for_path`) and `pub(crate) fn is_favorite_clip(path) -> bool`.
- [ ] `pub(crate) fn enforce_quota_with_clip_policy(dir, quota_bytes, protect)` wrapping
  storage `enforce_quota_with_policy` with uploads + favorites protected and `clip_gc_priority`.
- [ ] `#[tauri::command] set_clip_favorite` + `SetClipFavoriteInfo`; register in `app.rs`
  `invoke_handler`.

### Task 3: wire the service and recheck to the app policy

**Files:** `apps/clipline-app/src/service.rs`, `apps/clipline-app/src/app.rs`

- [ ] Failing tests: `storage_quota_full_event` with auto-delete on keeps a favorited clip and
  deletes an older non-favorite; a session deletes before a newer replay and a replay before a
  newer trim.
- [ ] `make_room_for_quota` and `recheck_storage_quota` call
  `library::enforce_quota_with_clip_policy` (favorites + active uploads protected).

### Task 4: UI — heart button, card badge, Favorites chip, context menu

**Files:** `apps/clipline-app/ui/index.html`, `apps/clipline-app/ui/main.js`,
`apps/clipline-app/ui/library.js`, `apps/clipline-app/ui/review-player.js`,
`apps/clipline-app/ui/styles.css`

- [ ] Review header gains `#favorite-clip` heart icon-button; `syncReviewLocalActions` hides it
  for cloud-only clips and syncs `on`/`aria-pressed`/title from `currentClip.favorite`.
- [ ] Card builder adds a heart badge when `c.favorite`; local-card context menu gains
  `#clip-menu-favorite` (label follows state) wired to the same toggle.
- [ ] Filter row gains a Favorites chip (`data-filter="favorite"`); `filterGalleryClips` keeps
  only `c.favorite`; the chip participates in the existing `on`-class sync.
- [ ] Toggle handler invokes `set_clip_favorite`, patches `clipsCache` + `currentClip`, and
  re-renders; failure surfaces in the shared error line.

### Task 5: UI contract + docs

**Files:** `apps/clipline-app/tests/ui_contract.rs`, `ddoc.md`, `handoff.md`

- [ ] Contract tests guard: `#favorite-clip` in the review header and the cloud-only hide list;
  the Favorites chip + `filterGalleryClips` favorite branch; the `set_clip_favorite` invoke;
  the context-menu entry; the card badge.
- [ ] `ddoc.md` Storage section: favorites are never auto-deleted; auto-delete drains kinds in
  Sessions → Replay → Trim order. `handoff.md` checkpoint entry.

### Task 6: verify

- [ ] `cargo test --workspace` green; `cargo clippy --workspace --all-targets -- -D warnings`
  clean (fresh cache for changed crates).
- [ ] Launch the local debug build: favorite a replay from Review and from a card's context
  menu, confirm the heart badge and Favorites chip isolate it, and confirm auto-delete skips it
  and prefers sessions over replays over trims.
