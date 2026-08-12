# User Bookmark Hotkey Plan

**Goal:** A hotkey that drops a bookmark on the recording timeline. It renders as one more timeline
marker in the review player, but it is user-placed rather than game-derived — the thing users asked
for while reviewing long full-session recordings.

## Product decisions

- **Default binding is `F7`** (next to the `F6` save-replay default) so the feature is discoverable
  out of the box. Upgrades must never break an existing keybind: when `bookmark_hotkey` is *absent*
  from a settings file the `F7` default applies, and it is **dropped** if it collides with any
  binding the user already has. A key that is *present but null/blank* stays unbound — clearing the
  field in Settings is honored and never resurrected by the default.
- **Distinct short sound on press.** Bookmarks land mid-game with the app invisible, so silent
  confirmation is useless, and reusing `soundeffect.ogg` would make "bookmark dropped" and "replay
  saved" indistinguishable. A new, shorter, quieter `bookmark.ogg` ships alongside it. rodio is
  built `default-features = false, features = ["vorbis"]`, so the asset must be Ogg Vorbis — no
  dependency change.
- **Bookmarks work in both recording modes.** They are motivated by full-session recordings, but
  the marker log is shared with the replay ring, so a bookmark dropped before a Save Replay rides
  into that clip for free. In replays-only mode the existing ring pruning discards bookmarks that
  scroll out of the window, exactly as it does for game markers.
- **Bookmarks are not `GameEvent`s.** `GameEvent.game_id` is a four-game enum, meaningless for a
  bookmark dropped while recording a custom game or no game at all. They get their own sidecar
  array, so no synthetic game identity and no disturbance to plugin marker presentation.
- **Bookmarks bypass the game-review filters.** `reviewTimelineMarkers` keys on game-marker
  categories and is gated on per-game review settings; a user-placed bookmark must show on a clip
  with no detected game and must not vanish because someone turned off objective markers.
- **No label/rename in v1.** The request was a timeline marker, not annotation. `ClipBookmark` gets
  a struct (not a bare float) so a `label` field can be added later without a sidecar migration.
- Not in scope: placing or deleting bookmarks from the review timeline UI, bookmarks in the
  game-event rail, and exporting bookmarks as chapter markers.

## Minimal architecture

The review timeline is already generic: pins, overview ticks, marker count, prev/next-marker
navigation, drag snapping and the library "marked" filter all key on `{t_s, kind}` and resolve
`kind` → category → glyph/color. So bookmarks are mapped into marker shape in exactly **one** place
(`clipMarkers()` in `app-core.js`) and every surface picks them up.

**Sidecar** (`crates/clipline-events`): `ClipMarkers` gains
`#[serde(default, skip_serializing_if = "Vec::is_empty")] bookmarks: Vec<ClipBookmark>` where
`ClipBookmark { t_s: f64 }`. Old sidecars deserialize unchanged; bookmark-free clips serialize
unchanged.

**Marker log:** `MarkerLog` owns bookmark offsets next to its events so bookmarks inherit the same
timeline semantics for free — `retain_from_recording_offset` prunes them, `clip_markers` filters to
the window and re-bases to clip time. New `push_bookmark(offset_s)`, ignoring non-finite values and
respecting the retained-media front like `push` does.

**Recorder:** `Cmd::Bookmark { pressed_at: Instant }` carries the keypress instant so command-queue
latency cannot skew placement; the service converts it against `recording_t0`, the same origin every
other marker offset is anchored to. A new `Event::BookmarkAdded { t_s }` drives the sound and a UI
toast. `write_marker_sidecar` must stop dropping bookmarks (it filters `markers` by
`is_review_event`) and must count them in its content guard, or a bookmark-only session writes no
sidecar at all.

**Hotkey:** a third `HookAction::Bookmark` on the existing low-level hook. The hook's two hotkey
slices become a `HookHotkeys { save, recording, bookmark }` struct rather than a third positional
slice. Registration is hook-only, matching the recording toggle — the Tauri global-shortcut handler
assumes save-replay and stays untouched.

**Library:** `crop_markers` must crop and re-base bookmarks (otherwise trim-export drops or
misplaces them) and `has_marker_sidecar_content` must count them.

## Plan-driven implementation

### Task 1: Sidecar and marker log

- [ ] Failing tests in `clipline-events`: `push_bookmark` anchors on the recording timeline;
      `clip_markers` filters to `[start, end)` and re-bases bookmarks to clip time;
      `retain_from_recording_offset` prunes bookmarks and refuses late pushes behind the media
      front; sidecar round-trips with and without bookmarks; a bookmark-free clip serializes no
      `bookmarks` key.
- [ ] Add `ClipBookmark`, the `ClipMarkers.bookmarks` field, and `MarkerLog::push_bookmark`.

### Task 2: Recorder wiring

- [ ] Failing tests: `write_marker_sidecar` keeps bookmarks (they are not review events) and writes
      a sidecar for a bookmark-only clip; the saved-clip marker count includes bookmarks.
- [ ] Add `Cmd::Bookmark { pressed_at }` and `Event::BookmarkAdded { t_s }`; convert against
      `recording_t0` and push into the marker log.
- [ ] Fix the `write_marker_sidecar` retain and content guard.

### Task 3: Hotkey and settings

- [ ] Failing tests: bookmark bindings dispatch `HookAction::Bookmark` and collide-check against
      save and recording bindings; the absent-key default is `F7`; an absent default that collides
      with an existing binding is dropped; an explicitly null/blank field stays unbound;
      save/load round-trip; `validate()` rejects a bookmark hotkey that duplicates another action.
- [ ] `HookHotkeys` struct, `HookAction::Bookmark`, and the third binding set in `hotkeys.rs`.
- [ ] `bookmark_hotkey` / `bookmark_hotkey_secondary` on `AppSettings` with the load repair,
      `bookmark_hotkeys()`, validation, and `save_to` normalization.
- [ ] Wire the hook callback to a `RuntimeState` bookmark request that sends `Cmd::Bookmark` and
      reports "not recording" when no recorder is running.

### Task 4: Sound

- [ ] Generate `bookmark.ogg` (short two-tone blip, quieter than the save sound) with the bundled
      ffmpeg, and record the exact command in `handoff.md` so the asset is reproducible and
      swappable.
- [ ] Generalize `sound::play_once` over the asset bytes, add `play_bookmark_added()`, and cover
      that the new asset decodes (mirroring the existing sound test).

### Task 5: Review UI

- [ ] Failing `tests/player_core.rs` (boa) cases: bookmarks merge into the timeline marker list
      sorted by time, survive game-review filters being off, and are deduplicated/finite-checked.
- [ ] `player-core.js`: `Bookmark` kind → new `bookmark` category with its own glyph, plus the pure
      merge helper. `app-core.js`: read `clip.markers.bookmarks` and merge past the review filter.
- [ ] `review-player.js` bookmark tooltip (no `undefined` actor) and a `.marker-bookmark` /
      `.ov-marker.marker-bookmark` color in `styles.css`.
- [ ] Settings UI: the two keybind fields in `index.html` + `settings.js` (field ids, status
      element, payload keys), with `tests/ui_contract.rs` coverage.

### Task 6: Library

- [ ] Failing tests: `crop_markers` crops and re-bases bookmarks for trim-export;
      `has_marker_sidecar_content` counts a bookmark-only sidecar.
- [ ] Implement both.

### Task 7: Verify and hand off

- [ ] `cargo test --workspace` green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean, with a clean cache for changed
      crates.
- [ ] Update `handoff.md` and `ddoc.md`, then relaunch Clipline for manual testing: F7 during a
      full session plays the new sound and the bookmark appears on the timeline after the session
      ends; F7 before a Save Replay rides into the clip; prev/next-marker navigation and trim-export
      keep bookmarks; an upgraded settings file that already uses F7 does not lose its binding.
