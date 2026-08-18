# Auto-detect newly launched Steam games

**Goal:** When a just-installed Steam game launches, Clipline should start the replay buffer on that window without the user adding a custom game first.

**User story:** Sluggo installs a friendslop, launches it, and Save Replay does nothing because Games-only is waiting for an allowlisted game. Detection already runs. The game is just not on the list.

## Product decisions

- Reuse the existing 500 ms detector in `spawn_game_detector`. Do not add WMI, ETW, or a second poller.
- After enabled built-in profiles and enabled custom games miss, match a visible titled window whose `exe_path` sits under a cached Steam install dir (`steamapps\\common\\...`) and is not a launcher/helper.
- Opt-in setting `games.auto_detect_steam_launches`, default **on**. Existing installs get it through serde default. Games-only users are the ones who cannot clip today. Desktop-buffer users still benefit (window capture instead of the monitor fallback) and can turn it off.
- Default-on is an intentional product decision: a fresh Games-only install with empty lists now enumerates windows every 500 ms. That is the same cost class as a configured install; it is also how the friendslop story works.
- Session-only identity. Do **not** write Custom games. Recording mode is `replays_only`. Toast: `Recording {name}` with an **Always add** action that appends a normal custom game (exe path + name + icon) through the existing save path.
- A newly detected Steam window starts the same capture restart already used for custom games. Same HWND stays captured; leaving the window returns to Waiting or the fallback target as today.
- Built-in profiles still win, including disabled ones. The live detector plugins today are League and osu (neither is a Steam install). Still skip any Steam app whose path/exe would match a registered plugin window, enabled or not, so a later Steam plugin (CS2) cannot be revived by this matcher. Enabled custom games still win over Steam-path.
- Ignore Steam client, web helper, crash handlers, launchers, browsers, and other existing discovery noise. Prefer the longest non-helper title when several windows share one Steam exe.
- Steam catalog is cached. Refresh at detector start, then at most every 30 s while a window''s `exe_path` sits under a Steam root but misses the catalog. A browser/Explorer window is not a refresh trigger. Never scan `libraryfolders.vdf` / appmanifests on the 500 ms tick.
- Exclusive fullscreen remains out of scope (WGC cannot capture it without injection). Epic/GOG/itch/standalone and "any fullscreen app" are later, not this plan.

## Why not fake a custom id

`active_game_still_configured` drops `GameIdentity::Custom` unless that id is in `settings.games.custom_games`. A synthetic `custom-...` identity would match once, then get cleared on the next detector tick. Add `GameIdentity::DiscoveredSteam { app_id: u32, id: String }` and keep that variant configured while `auto_detect` and `auto_detect_steam_launches` are on.

`id()` today returns `&str`, so store the formatted id on the variant (`steam-{app_id}`). Never a reserved built-in id, never a `custom-` id. Clip sidecars already store `game.identity.id()` as a plain string.

Every catalog entry comes from an appmanifest, so `app_id` always exists. Do not add a path-keyed identity fork.

## Event contract (required for the notice)

`GameDetectionEvent` currently has name / window_title / process_id / exe_name / recording_mode — no identity, no source, no `exe_path`. The UI cannot tell a discovered Steam game from a custom game.

Add `discovered_steam: bool` (default false). Same-window re-detections already set `emit_event=false`, so the frontend can treat the first `discovered_steam` event for an id as the toast trigger. Do not add a second Rust event.

Do **not** put `exe_path` on the event. Always-add reconstructs the custom game on the backend from the live `DetectedGame`.

## Minimal architecture

1. **Settings:** `GameSettings.auto_detect_steam_launches: bool` default true. Must land on **all four**: `GameSettings`, `GameSettingsWire` with `#[serde(default = "default_enabled")]`, `Default`, and the deserialize mapping. Missing the wire type compiles and silently defaults — the load test must deserialize JSON that omits the field, not only round-trip `Default`. JS: `defaultGameSettings()`, the settings payload collector, the checkbox, and the `main.js` change-listener id list (`set-games-auto-detect` / `set-games-pause-when-empty`).
2. **Identity:** `GameIdentity::DiscoveredSteam { app_id, id }` with `id()` = stored `steam-{app_id}`. Update `active_game_still_configured` and any identity match. Session meta is already a string id.
3. **Steam cache:** extract a **manifest-only** listing from `game_discovery.rs` (`app_id`, `name`, `install_dir`). Reuse `steam_install_roots`, `steam_libraries_from_root`, `steam_app_from_manifest`, `is_path_within`, `is_helper_exe_name`. Do **not** call `infer_executable_path` / `steam_apps_from_roots` on this path — that scoring walk is Detect Games UI work and is dead weight here because lookup is install-dir prefix, not inferred exe name. Detector holds `SteamLaunchCatalog { apps, loaded_at }`. Lookup: first app whose `install_dir` contains `exe_path`.
4. **Matcher:** keep `detect_active_game_from_windows(settings, windows)` as the public test signature (~25 existing call sites). Add a catalog-injecting helper for new Steam-path tests only. Do not thread catalog through the old tests. After built-in + custom, if the new setting is on, pick the best non-noise Steam window. Skip plugin-mapped windows even when the plugin is disabled. Name from the Steam app, else exe stem. `recording_mode: ReplaysOnly`.
5. **Notice + Always add:** first `game-detection` with `discovered_steam: true` shows `Recording {name}` plus an Always add button. `#notice` / `setNotice` is text-only; `#deck-status` already has `setDeckStatusAction`. Put Always add on the deck-status action (or a one-off sibling button next to `#notice`) — do not invent a second toast system. Button click invokes a new backend command that builds `CustomGameSettings` from the live `DetectedGame` (`exe_path` required, plus name + icon) and saves through the existing settings path (dedupe already exists). If the game is already a custom game, no-op. Frontend never rebuilds the rule from `exe_name`.
6. **UI:** Settings > Games checkbox under Game detection. Rail already shows `Active: {name}`. No new rail control.

## Plan-driven implementation

### Task 1: Identity and settings contracts

- [ ] Failing `GameIdentity` tests: discovered Steam id is stable (`steam-427520`), not a built-in, not a valid custom id, and does not collide with `custom-`. `id()` returns the stored string.
- [ ] Failing settings tests: default on; JSON **without** the field deserializes as on (this is the `GameSettingsWire` trap); explicit false round-trips through save/load.
- [ ] Failing unknown-id consumer test: session meta / `ClipGame` round-trips `steam-427520` + display name as opaque strings (renders the name, does not error).
- [ ] Implement the enum variant and `auto_detect_steam_launches` on `GameSettings`, `GameSettingsWire`, `Default`, and the wire mapping.

### Task 2: Steam-path matcher

- [ ] Failing `games.rs` tests, injecting a fake catalog (no live Steam scan). Existing `detect_active_game_from_windows(settings, windows)` call sites stay as they are. New Steam-path tests call a catalog-injecting helper:
      - unmatched Steam `exe_path` under `steamapps\\common\\Friendslop` becomes active with `replays_only` and identity `steam-{app_id}`;
      - built-in still wins;
      - a window that a registered plugin would match is not revived when that plugin is disabled (use a synthetic plugin-shaped window / CS2-shaped Steam app 730 if adding a fixture; League/osu paths are not Steam);
      - enabled custom still wins;
      - helper/launcher/Steam client ignored;
      - setting off → no match;
      - `auto_detect` off → no match.
- [ ] Failing catalog test: path under install dir matches even when the running exe is not the inferred folder-name exe (Unity/Unreal). Catalog entries have no inferred exe field.
- [ ] Implement matcher + manifest-only catalog lookup. Keep filesystem work off the 500 ms path except the bounded refresh.
- [ ] Catalog refresh: Steam-rooted miss only, 30 s bound. Not "any unmatched window".

### Task 3: Runtime keep-alive and capture start

- [ ] Failing `active_game_still_configured` test: a discovered Steam game stays configured while both auto-detect flags are on, and drops when `auto_detect_steam_launches` is turned off (same pattern as disabling a custom game).
- [ ] Failing runtime test: games-only waiting → Steam window detected → recorder starts on that HWND; window gone → waiting again.
- [ ] Wire `has_enabled_games` so Steam auto-detect counts as an enabled source (otherwise the detector skips enumeration when plugins and custom games are empty).
- [ ] `GameDetectionEvent` includes `discovered_steam: bool`. Same-window re-detect does not re-emit.

### Task 4: Always-add notice and Settings UI

- [ ] Failing UI contract: checkbox `set-games-auto-detect-steam`; payload includes `auto_detect_steam_launches`; `main.js` listens on that checkbox id with the other game-detection toggles.
- [ ] Failing contract: Always add is shown only when `discovered_steam` is true; click calls the backend add command (not a client-side `customGames.push` from `exe_name`); command uses live `DetectedGame.exe_path` and existing save/dedupe; already-added is a no-op; toast is not re-shown for the same id in one UI session.
- [ ] Implement the checkbox, notice+action, and add command. No auto-persist.

### Task 5: Docs, gates, handoff

- [ ] Note in `ddoc.md` / `handoff.md`: opt-in Steam-path fallback, session-only unless Always add, exclusive fullscreen still unsupported.
- [ ] `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Manual: Games-only on, launch an unlisted Steam game, confirm Buffer leaves Waiting, Save Replay works, Always add persists it, turning the setting off restores Waiting.

## Out of scope

Epic/GOG/Xbox catalogs, fullscreen-any-app, exclusive fullscreen, background library scanning, auto-writing Custom games, Detect Games running-window restore, a second toast framework, exe-inference on the detector catalog (nice later, not required for this story).

