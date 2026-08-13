# League Replay Gate, Library Stats Wrap, and LoL Type Search

**Goal:** Let users auto-record (or skip) League *client replays* as their own game type,
keep Library header stats readable when the window is narrow, and replace the League-only
Game type dropdown with Discord-style `LoL Type:` tokens in the search bar.

## Product decisions

- **Replay is a first-class League category**, not a queue ID. Detection is the game
  process command line containing a `.rofl` argument. That wins over LCU `gameData.queue.id`,
  which can still report the original match queue while a replay is playing.
- Settings get a **Record game types → Replay** toggle, default on, so upgrades keep
  record-all. The existing automatic-only gate applies; manual recording still bypasses.
- Sidecar tagging must not depend on Live Client Data. Replays often have no in-game API, so
  the poller emits `Queue(Replay)` as soon as the command line matches, before the live loop.
- Library header stats (`18 of 18` and `25.2 GB / 100 GB`) must wrap as **whole phrases**,
  stacked when they cannot sit side by side. Do not split inside a phrase. Drop the `·`
  prefix; spacing is CSS gap.
- **Game type is not a header dropdown.** Typing or choosing `LoL Type:` in Library search
  opens a Discord-style suggestion list (Ranked, Normal, Replay, …). The chosen value
  becomes a chip; remaining text is the free-text query. Same `galleryGameType` filter
  semantics as today.

## Minimal architecture

- Neutral `clipline-lol`: `LeagueQueueCategory::Replay`, `LeagueQueue::replay()`,
  `is_league_replay_command_line`.
- Windows `process_command_line(pid)` next to the existing process helpers. Gate lookup
  and the League poller both consult it first.
- DOM-free `gallery-search-core.js` owns prefix/value matching so Boa can test it.
  `library.js` / `main.js` own chips, the suggestion menu, and wiring.

## Plan-driven implementation

### Task 1: Replay category, detection, and gate

- [ ] Failing tests for `.rofl` command-line detection, `LeagueQueue::replay()`, settings
      `record_replay` default/load/allows/`has_gate`.
- [ ] Implement category, settings toggle, Windows command-line query, gate lookup override,
      and poller Queue(Replay) before Live Client.
- [ ] Settings UI + ui_contract coverage for the Replay checkbox.

### Task 2: Library header stats wrap

- [ ] Wrap `#gallery-count` and `#gallery-storage-used` in `.gallery-stats`.
- [ ] `white-space: nowrap` on each phrase; flex-wrap the group so they stack when tight.
- [ ] Stop prefixing storage with `·`.

### Task 3: LoL Type search tokens

- [ ] Failing Boa tests for prefix completion, value suggestions (including Replay), and
      chip/query split.
- [ ] Replace `#gallery-game-type` with search chips + suggestion menu.
- [ ] Keep `filterGalleryClips` category filtering; update ui_contract.

### Task 4: Verify and hand off

- [ ] `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Update `handoff.md` / `ddoc.md`, relaunch the app.
