//! Ask before recording unlisted Steam games. The detector reports such a
//! launch as `GameIdentity::DiscoveredSteam`; it never becomes the active
//! game. It becomes the pending prompt instead, and the Clipline window
//! comes forward so the user can Add it (a custom game that records from
//! the next detector tick) or Ignore it (for this launch, or for good).

use tauri::{AppHandle, Emitter, Runtime};

use crate::game_identity::GameIdentity;
use crate::games::DetectedGame;
use crate::settings::{AppSettings, CustomGameSettings, GameSettings, IgnoredSteamGame};

use super::*;

pub(crate) const STEAM_GAME_PROMPT_EVENT: &str = "steam-game-prompt";

/// Launches ignored "for this launch" are remembered per process; the cap
/// only bounds a session that ignores many launches.
const MAX_SKIPPED_STEAM_LAUNCHES: usize = 64;

#[derive(serde::Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SteamGamePrompt {
    pub(crate) app_id: u32,
    pub(crate) process_id: u32,
    pub(crate) name: String,
}

impl SteamGamePrompt {
    fn from_game(game: &DetectedGame) -> Option<Self> {
        let GameIdentity::DiscoveredSteam { app_id, .. } = game.identity else {
            return None;
        };
        Some(Self {
            app_id,
            process_id: game.process_id,
            name: game.name.clone(),
        })
    }
}

/// The prompt a decision was made for. The frontend never sends a path; the
/// rule is rebuilt from the pending `DetectedGame`, and the app and process
/// ids keep a stale dialog from acting on a different launch.
#[derive(serde::Deserialize, Clone, Copy, Debug)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SteamPromptTarget {
    pub(crate) app_id: u32,
    pub(crate) process_id: u32,
}

impl SteamPromptTarget {
    fn matches(self, game: &DetectedGame) -> bool {
        SteamGamePrompt::from_game(game)
            .is_some_and(|prompt| prompt.app_id == self.app_id && prompt.process_id == self.process_id)
    }
}

/// The pending prompt as the frontend sees it. `revision` rises on every
/// change, so a late event or a boot-time query answered before a newer
/// change can never close or replace the newer dialog.
#[derive(serde::Serialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct SteamPromptSnapshot {
    pub(crate) revision: u64,
    pub(crate) prompt: Option<SteamGamePrompt>,
}

/// Runtime-owned prompt state; lives on `RuntimeInner::steam_prompt`.
#[derive(Default)]
pub(crate) struct SteamPromptState {
    pending: Option<DetectedGame>,
    revision: u64,
    /// `(app_id, process_id)` launches ignored for this launch only. The
    /// detector drops them from Steam candidate selection.
    skipped_launches: Vec<(u32, u32)>,
}

impl SteamPromptState {
    /// Replace the pending game. Title or window changes of the same launch
    /// keep the revision; a different launch (or none) bumps it.
    fn set_pending(&mut self, next: Option<DetectedGame>) -> bool {
        let changed = prompt_key(self.pending.as_ref()) != prompt_key(next.as_ref());
        self.pending = next;
        if changed {
            self.revision += 1;
        }
        changed
    }

    /// Clear the prompt only if it is still `target`'s; a newer launch that
    /// opened meanwhile stays pending.
    fn resolve(&mut self, target: SteamPromptTarget) {
        if self.pending.as_ref().is_some_and(|game| target.matches(game)) {
            self.set_pending(None);
        }
    }

    fn snapshot(&self) -> SteamPromptSnapshot {
        SteamPromptSnapshot {
            revision: self.revision,
            prompt: self.pending.as_ref().and_then(SteamGamePrompt::from_game),
        }
    }
}

/// A custom rule already targets this executable (its exact path, or a
/// path-less rule's exe name). Shared by the prompt filter and Add, so an
/// added game never prompts again and is never added twice.
pub(crate) fn custom_rule_covers(games: &GameSettings, exe_path: Option<&str>, exe_name: &str) -> bool {
    let path_key = exe_path
        .filter(|path| !path.trim().is_empty())
        .map(crate::games::path_key);
    games.custom_games.iter().any(|existing| match existing.process_path.as_deref() {
        Some(path) => path_key.as_deref() == Some(crate::games::path_key(path).as_str()),
        None => {
            let exe = existing.exe_name.trim();
            !exe.is_empty() && exe.eq_ignore_ascii_case(exe_name.trim())
        }
    })
}

/// Checked against the live settings, not the detector's snapshot: a tick
/// that started before Add or Ignore landed must not reopen the prompt.
fn prompt_allowed(inner: &RuntimeInner, game: &DetectedGame) -> bool {
    let Some(prompt) = SteamGamePrompt::from_game(game) else {
        return false;
    };
    let games = &inner.settings.games;
    games.auto_detect
        && games.auto_detect_steam_launches
        && !games.steam_app_ignored(prompt.app_id)
        && !inner
            .steam_prompt
            .skipped_launches
            .contains(&(prompt.app_id, prompt.process_id))
        && !custom_rule_covers(games, game.exe_path.as_deref(), &game.exe_name)
}

fn prompt_key(game: Option<&DetectedGame>) -> Option<(u32, u32)> {
    game.and_then(SteamGamePrompt::from_game)
        .map(|prompt| (prompt.app_id, prompt.process_id))
}

pub(crate) fn custom_game_from_prompt(game: &DetectedGame) -> Result<CustomGameSettings, String> {
    let GameIdentity::DiscoveredSteam { app_id, .. } = &game.identity else {
        return Err("the detected game is not a discovered Steam launch".into());
    };
    let exe_path = game
        .exe_path
        .clone()
        .filter(|path| !path.trim().is_empty())
        .ok_or("the detected Steam window has no executable path")?;
    Ok(CustomGameSettings {
        id: format!("custom-steam-{app_id}"),
        // Sessions nightly 1.0.7 recorded before a manual add carry this id.
        legacy_ids: vec![format!("steam-{app_id}")],
        name: game.name.clone(),
        enabled: true,
        exe_name: game.exe_name.clone(),
        process_path: Some(exe_path),
        window_title: game.window_title.clone(),
        recording_mode: game.recording_mode,
        icon: None,
    })
}

/// Append `game` unless a rule already covers it, then run the normal
/// custom-game normalization so ids and dedupe match manual adds. Steam ids
/// (`custom-steam-<u32>`, plus a short suffix) stay far below the custom id
/// length limit.
pub(crate) fn insert_prompted_custom_game(
    games: &mut GameSettings,
    mut game: CustomGameSettings,
) -> Option<CustomGameSettings> {
    if custom_rule_covers(games, game.process_path.as_deref(), &game.exe_name) {
        return None;
    }
    let base = game.id.clone();
    let mut suffix = 2;
    while games.custom_games.iter().any(|existing| existing.id == game.id) {
        game.id = format!("{base}-{suffix}");
        suffix += 1;
    }
    let id = game.id.clone();
    games.custom_games.push(game);
    games.normalize();
    games.custom_games.iter().find(|existing| existing.id == id).cloned()
}

impl RuntimeState {
    /// Pull an unlisted Steam launch out of `detected` so it never records,
    /// and track it as the pending prompt. Returns the snapshot to announce
    /// when the prompt changed.
    pub(crate) fn take_steam_prompt(
        inner: &mut RuntimeInner,
        detected: &mut Option<DetectedGame>,
    ) -> Option<SteamPromptSnapshot> {
        let candidate = if detected
            .as_ref()
            .is_some_and(|game| matches!(game.identity, GameIdentity::DiscoveredSteam { .. }))
        {
            detected.take().filter(|game| prompt_allowed(inner, game))
        } else {
            None
        };
        inner
            .steam_prompt
            .set_pending(candidate)
            .then(|| inner.steam_prompt.snapshot())
    }

    pub(crate) fn steam_prompt_snapshot(&self) -> SteamPromptSnapshot {
        match self.0.lock() {
            Ok(inner) => inner.steam_prompt.snapshot(),
            Err(_) => SteamPromptSnapshot { revision: 0, prompt: None },
        }
    }

    pub(crate) fn skipped_steam_launches(&self) -> Vec<(u32, u32)> {
        self.0
            .lock()
            .map(|inner| inner.steam_prompt.skipped_launches.clone())
            .unwrap_or_default()
    }

    fn pending_prompt_game(
        inner: &RuntimeInner,
        target: SteamPromptTarget,
    ) -> Result<DetectedGame, String> {
        inner
            .steam_prompt
            .pending
            .as_ref()
            .filter(|game| target.matches(game))
            .cloned()
            .ok_or_else(|| "that Steam game is no longer waiting for a decision".into())
    }

    /// Add: save the pending launch as an enabled custom game. The next
    /// detector tick matches it through the new rule and starts capture.
    /// `None` means an existing rule already covered it.
    pub(crate) fn add_prompted_steam_game_with(
        &self,
        target: SteamPromptTarget,
        icon_for_path: impl FnOnce(&str) -> Option<String>,
        save: impl FnOnce(&AppSettings) -> Result<(), String>,
    ) -> Result<Option<CustomGameSettings>, String> {
        let pending = {
            let inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
            Self::pending_prompt_game(&inner, target)?
        };
        let mut game = custom_game_from_prompt(&pending)?;
        // Icon extraction reads the executable; keep it outside the lock.
        game.icon = game.process_path.as_deref().and_then(icon_for_path);
        let _save_guard = CLOUD_SETTINGS_SAVE_LOCK
            .lock()
            .map_err(|_| "settings save lock poisoned")?;
        let mut next = self.settings();
        let added = insert_prompted_custom_game(&mut next.games, game);
        if added.is_some() {
            save(&next)?;
        }
        let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
        inner.settings.games.custom_games = next.games.custom_games;
        inner.steam_prompt.resolve(target);
        Ok(added)
    }

    /// Ignore: skip this launch, or with `never_ask_again` persist the app so
    /// detection drops it for good. Returns the saved ignore list.
    pub(crate) fn ignore_prompted_steam_game_with(
        &self,
        target: SteamPromptTarget,
        never_ask_again: bool,
        save: impl FnOnce(&AppSettings) -> Result<(), String>,
    ) -> Result<Vec<IgnoredSteamGame>, String> {
        let _save_guard = CLOUD_SETTINGS_SAVE_LOCK
            .lock()
            .map_err(|_| "settings save lock poisoned")?;
        let (pending, mut next) = {
            let inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
            (Self::pending_prompt_game(&inner, target)?, inner.settings.clone())
        };
        if never_ask_again {
            next.games.ignored_steam_games.push(IgnoredSteamGame {
                app_id: target.app_id,
                name: pending.name,
            });
            next.games.normalize();
            save(&next)?;
        }
        let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
        if never_ask_again {
            inner.settings.games.ignored_steam_games = next.games.ignored_steam_games;
        } else {
            let skipped = &mut inner.steam_prompt.skipped_launches;
            let launch = (target.app_id, target.process_id);
            if !skipped.contains(&launch) {
                skipped.push(launch);
            }
            let overflow = skipped.len().saturating_sub(MAX_SKIPPED_STEAM_LAUNCHES);
            skipped.drain(..overflow);
        }
        inner.steam_prompt.resolve(target);
        Ok(inner.settings.games.ignored_steam_games.clone())
    }

    /// Settings → Games: let a "never ask again" game prompt again.
    pub(crate) fn unignore_steam_game_with(
        &self,
        app_id: u32,
        save: impl FnOnce(&AppSettings) -> Result<(), String>,
    ) -> Result<Vec<IgnoredSteamGame>, String> {
        let _save_guard = CLOUD_SETTINGS_SAVE_LOCK
            .lock()
            .map_err(|_| "settings save lock poisoned")?;
        let mut next = self.settings();
        next.games.ignored_steam_games.retain(|game| game.app_id != app_id);
        save(&next)?;
        let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
        inner.settings.games.ignored_steam_games = next.games.ignored_steam_games;
        Ok(inner.settings.games.ignored_steam_games.clone())
    }
}

/// Announce a prompt change. A new prompt also brings Clipline forward;
/// the frontend re-reads `steam_game_prompt` on boot, so a window rebuilt
/// from the tray still shows it.
pub(crate) fn announce_steam_prompt<R: Runtime>(app: &AppHandle<R>, snapshot: SteamPromptSnapshot) {
    if let Some(prompt) = &snapshot.prompt {
        log_diagnostic(format!(
            "steam game prompt: app_id={} name={:?}",
            prompt.app_id, prompt.name
        ));
        let handle = app.clone();
        let _ = app.run_on_main_thread(move || {
            if let Err(error) = open_main_window(&handle) {
                log_diagnostic(format!("steam game prompt open failed: {error}"));
            }
        });
    }
    let _ = app.emit(STEAM_GAME_PROMPT_EVENT, snapshot);
}

#[tauri::command]
pub(crate) fn steam_game_prompt(state: tauri::State<RuntimeState>) -> SteamPromptSnapshot {
    state.steam_prompt_snapshot()
}

#[tauri::command(async)]
pub(crate) fn add_prompted_steam_game<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<RuntimeState>,
    target: SteamPromptTarget,
) -> Result<Option<CustomGameSettings>, String> {
    let added = state.add_prompted_steam_game_with(
        target,
        crate::game_icon::extract_exe_icon_data_url,
        AppSettings::save,
    )?;
    let _ = app.emit(STEAM_GAME_PROMPT_EVENT, state.steam_prompt_snapshot());
    Ok(added)
}

#[tauri::command(async)]
pub(crate) fn ignore_prompted_steam_game<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<RuntimeState>,
    target: SteamPromptTarget,
    never_ask_again: bool,
) -> Result<Vec<IgnoredSteamGame>, String> {
    let ignored = state.ignore_prompted_steam_game_with(target, never_ask_again, AppSettings::save)?;
    let _ = app.emit(STEAM_GAME_PROMPT_EVENT, state.steam_prompt_snapshot());
    Ok(ignored)
}

#[tauri::command(async)]
pub(crate) fn unignore_steam_game(
    state: tauri::State<RuntimeState>,
    app_id: u32,
) -> Result<Vec<IgnoredSteamGame>, String> {
    state.unignore_steam_game_with(app_id, AppSettings::save)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::discovered_steam_game;

    fn steam_settings() -> AppSettings {
        let mut settings = AppSettings::default();
        settings.games.pause_when_no_game = true;
        settings.games.auto_detect_steam_launches = true;
        settings
    }

    fn target(process_id: u32) -> SteamPromptTarget {
        SteamPromptTarget { app_id: 427520, process_id }
    }

    fn prompt(process_id: u32) -> SteamGamePrompt {
        SteamGamePrompt {
            app_id: 427520,
            process_id,
            name: "Friendslop".into(),
        }
    }

    /// One detector tick: the announced prompt (outer `None` = no change)
    /// plus what reached detection.
    fn tick(
        state: &RuntimeState,
        game: Option<DetectedGame>,
    ) -> (Option<Option<SteamGamePrompt>>, Option<DetectedGame>) {
        let mut detected = game;
        let mut inner = state.0.lock().unwrap();
        let change = RuntimeState::take_steam_prompt(&mut inner, &mut detected);
        (change.map(|snapshot| snapshot.prompt), detected)
    }

    #[test]
    fn unlisted_steam_launch_prompts_once_and_never_reaches_capture() {
        let state = RuntimeState::new(steam_settings(), None);

        let (change, detected) = tick(&state, Some(discovered_steam_game(42)));
        assert_eq!(change, Some(Some(prompt(42))));
        assert!(detected.is_none(), "a pending Steam game must not record");
        assert_eq!(state.steam_prompt_snapshot().prompt, Some(prompt(42)));

        let mut retitled = discovered_steam_game(42);
        retitled.window_title = "Friendslop - Level 2".into();
        assert_eq!(tick(&state, Some(retitled)).0, None);

        assert_eq!(tick(&state, None).0, Some(None));
        assert_eq!(state.steam_prompt_snapshot().prompt, None);
    }

    #[test]
    fn configured_games_pass_through_and_close_the_prompt() {
        let state = RuntimeState::new(steam_settings(), None);
        tick(&state, Some(discovered_steam_game(42)));

        let custom = crate::app::detected_game("custom-other", "Other", 7);
        let (change, detected) = tick(&state, Some(custom.clone()));
        assert_eq!(change, Some(None));
        assert_eq!(detected, Some(custom));
    }

    #[test]
    fn discovered_steam_game_never_counts_as_configured() {
        let game = discovered_steam_game(42);
        assert!(!active_game_still_configured(&steam_settings(), Some(&game)));
    }

    #[test]
    fn add_saves_an_enabled_rule_and_clears_the_prompt() {
        let state = RuntimeState::new(steam_settings(), None);
        tick(&state, Some(discovered_steam_game(42)));
        let mut saved = None;

        let added = state
            .add_prompted_steam_game_with(target(42), |_| None, |next| {
                saved = Some(next.games.custom_games.clone());
                Ok(())
            })
            .unwrap()
            .expect("new rule");

        assert_eq!(added.id, "custom-steam-427520");
        assert_eq!(added.legacy_ids, ["steam-427520"]);
        assert!(added.enabled);
        assert_eq!(
            added.process_path.as_deref(),
            Some(r"C:\Steam\steamapps\common\Friendslop\Friendslop.exe")
        );
        assert_eq!(saved.unwrap(), vec![added.clone()]);
        assert_eq!(state.settings().games.custom_games, vec![added]);
        assert_eq!(state.steam_prompt_snapshot().prompt, None);

        // A detector tick that began before the add must not reopen it.
        let (change, detected) = tick(&state, Some(discovered_steam_game(42)));
        assert_eq!(change, None);
        assert!(detected.is_none());
    }

    #[test]
    fn add_rejects_a_stale_target() {
        let state = RuntimeState::new(steam_settings(), None);
        tick(&state, Some(discovered_steam_game(42)));

        let error = state
            .add_prompted_steam_game_with(target(43), |_| None, |_| panic!("must not save"))
            .unwrap_err();
        assert!(error.contains("no longer waiting"));
        assert_eq!(state.steam_prompt_snapshot().prompt, Some(prompt(42)));
    }

    #[test]
    fn failed_add_save_leaves_settings_and_prompt_unchanged() {
        let state = RuntimeState::new(steam_settings(), None);
        tick(&state, Some(discovered_steam_game(42)));

        let error = state
            .add_prompted_steam_game_with(target(42), |_| None, |_| Err("disk full".into()))
            .unwrap_err();
        assert_eq!(error, "disk full");
        assert!(state.settings().games.custom_games.is_empty());
        assert_eq!(state.steam_prompt_snapshot().prompt, Some(prompt(42)));
    }

    #[test]
    fn inserting_a_covered_game_is_a_no_op_and_ids_stay_unique() {
        let rule = custom_game_from_prompt(&discovered_steam_game(42)).unwrap();
        let mut games = GameSettings::default();
        assert_eq!(insert_prompted_custom_game(&mut games, rule.clone()), Some(rule.clone()));
        assert_eq!(insert_prompted_custom_game(&mut games, rule.clone()), None);

        let mut games = GameSettings::default();
        games.custom_games.push(CustomGameSettings {
            process_path: Some(r"C:\Other\Other.exe".into()),
            exe_name: "Other.exe".into(),
            ..rule.clone()
        });
        let added = insert_prompted_custom_game(&mut games, rule).unwrap();
        assert_eq!(added.id, "custom-steam-427520-2");
    }

    #[test]
    fn ignore_skips_only_this_launch() {
        let state = RuntimeState::new(steam_settings(), None);
        tick(&state, Some(discovered_steam_game(42)));

        let ignored = state
            .ignore_prompted_steam_game_with(target(42), false, |_| panic!("nothing persists"))
            .unwrap();
        assert!(ignored.is_empty());
        assert_eq!(state.steam_prompt_snapshot().prompt, None);

        let (change, detected) = tick(&state, Some(discovered_steam_game(42)));
        assert_eq!(change, None, "the same launch stays ignored");
        assert!(detected.is_none());

        let (change, _) = tick(&state, Some(discovered_steam_game(43)));
        assert_eq!(change, Some(Some(prompt(43))), "a relaunch asks again");
    }

    #[test]
    fn never_ask_again_persists_the_app_until_removed() {
        let state = RuntimeState::new(steam_settings(), None);
        tick(&state, Some(discovered_steam_game(42)));

        let ignored = state
            .ignore_prompted_steam_game_with(target(42), true, |next| {
                assert_eq!(next.games.ignored_steam_games.len(), 1);
                Ok(())
            })
            .unwrap();
        assert_eq!(
            ignored,
            vec![IgnoredSteamGame { app_id: 427520, name: "Friendslop".into() }]
        );
        assert_eq!(tick(&state, Some(discovered_steam_game(43))).0, None);

        let ignored = state.unignore_steam_game_with(427520, |_| Ok(())).unwrap();
        assert!(ignored.is_empty());
        assert_eq!(
            tick(&state, Some(discovered_steam_game(43))).0,
            Some(Some(prompt(43)))
        );
    }

    #[test]
    fn revision_rises_on_every_change_so_stale_snapshots_lose() {
        let state = RuntimeState::new(steam_settings(), None);
        let start = state.steam_prompt_snapshot().revision;
        tick(&state, Some(discovered_steam_game(42)));
        let opened = state.steam_prompt_snapshot().revision;
        assert!(opened > start);

        let mut retitled = discovered_steam_game(42);
        retitled.window_title = "Friendslop - Level 2".into();
        tick(&state, Some(retitled));
        assert_eq!(state.steam_prompt_snapshot().revision, opened, "same launch, same revision");

        state
            .ignore_prompted_steam_game_with(target(42), false, |_| Ok(()))
            .unwrap();
        assert!(state.steam_prompt_snapshot().revision > opened);
    }

    #[test]
    fn a_decision_for_an_old_launch_never_clears_a_newer_prompt() {
        let state = RuntimeState::new(steam_settings(), None);
        tick(&state, Some(discovered_steam_game(42)));
        let old = target(42);
        // The detector moves on to a relaunch before the old answer lands.
        tick(&state, Some(discovered_steam_game(43)));

        assert!(state.ignore_prompted_steam_game_with(old, false, |_| Ok(())).is_err());
        assert_eq!(state.steam_prompt_snapshot().prompt, Some(prompt(43)));
    }

    #[test]
    fn launches_ignored_once_reach_the_detector() {
        let state = RuntimeState::new(steam_settings(), None);
        tick(&state, Some(discovered_steam_game(42)));
        state
            .ignore_prompted_steam_game_with(target(42), false, |_| Ok(()))
            .unwrap();
        assert_eq!(state.skipped_steam_launches(), vec![(427520, 42)]);
    }

    #[test]
    fn turning_steam_detection_off_closes_a_stale_prompt() {
        let state = RuntimeState::new(steam_settings(), None);
        tick(&state, Some(discovered_steam_game(42)));
        state.0.lock().unwrap().settings.games.auto_detect_steam_launches = false;

        assert_eq!(
            tick(&state, Some(discovered_steam_game(42))).0,
            Some(None)
        );
    }

    #[test]
    fn saving_settings_keeps_the_backend_ignore_list() {
        let mut backend = steam_settings();
        backend.games.ignored_steam_games =
            vec![IgnoredSteamGame { app_id: 427520, name: "Friendslop".into() }];
        let mut frontend = steam_settings();
        preserve_backend_owned_settings_fields(&mut frontend, &backend);
        assert_eq!(frontend.games.ignored_steam_games, backend.games.ignored_steam_games);
    }
}
