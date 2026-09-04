use std::path::Path;
use std::sync::MutexGuard;
use std::time::{Duration, Instant};

use tauri::{
    AppHandle, Emitter, Runtime,
};
use tauri_plugin_global_shortcut::Shortcut;


use crate::games::DetectedGame;
use crate::osu_enrichment::OsuTitleEvent;
use crate::service::{self, Cmd, Event};
use crate::settings::AppSettings;
use crate::util::unix_now_i64;
use super::*;

impl RuntimeState {
    pub(crate) fn request_save(&self) -> bool {
        const DOUBLE_TRIGGER_DEBOUNCE: Duration = Duration::from_millis(150);

        if let Ok(mut inner) = self.0.lock() {
            if inner.quota_blocked.is_some() {
                return false;
            }
            let Some(tx) = inner.tx.as_ref().cloned() else {
                return false;
            };
            let now = Instant::now();
            if inner
                .last_save_request
                .is_some_and(|last| now.duration_since(last) < DOUBLE_TRIGGER_DEBOUNCE)
            {
                return false;
            }
            if tx.send(Cmd::Save).is_ok() {
                inner.last_save_request = Some(now);
                return true;
            }
        }
        false
    }

    /// Drop a bookmark on the running recorder's timeline. Key repeat is already
    /// filtered by the hook, so rapid deliberate presses each place a marker.
    /// `pressed_at` comes from the hook rather than from here — see `HookTrigger`.
    pub(crate) fn request_bookmark<R: Runtime>(&self, app: &AppHandle<R>, pressed_at: Instant) -> bool {
        if self.send(Cmd::Bookmark { pressed_at }) {
            return true;
        }
        let _ = app.emit(
            "error",
            "nothing is recording, so there was no timeline to bookmark".to_string(),
        );
        false
    }

    pub(crate) fn request_save_or_show_quota<R: Runtime>(&self, app: &AppHandle<R>) -> bool {
        if self.request_save() {
            return true;
        }
        if let Some(event) = self.durable_quota_event_for_replay() {
            let _ = app.emit("storage-quota-full", event);
        }
        false
    }

    pub(crate) fn recheck_storage_quota<R: Runtime>(
        &self,
        app: AppHandle<R>,
        media_dir: &Path,
        quota_bytes: Option<u64>,
        auto_delete: bool,
        announce: bool,
    ) -> Result<bool, String> {
        let (required_bytes, still_stopping) = {
            let inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
            match inner.quota_blocked.as_ref() {
                Some(Event::StorageQuotaFull { required_bytes, .. }) => {
                    (*required_bytes, inner.tx.is_some())
                }
                _ => return Ok(true),
            }
        };
        if still_stopping {
            if announce {
                if let Some(event) = self.durable_quota_event_for_replay() {
                    let _ = app.emit("storage-quota-full", event);
                }
            }
            return Ok(false);
        }
        let mut status = clipline_storage::storage_status(media_dir, quota_bytes)
            .map_err(|error| format!("storage status for {media_dir:?}: {error}"))?;
        let over_quota = |status: &clipline_storage::StorageStatus| {
            quota_bytes.is_some_and(|quota| {
                status.total_bytes > quota
                    || required_bytes > quota.saturating_sub(status.total_bytes)
            })
        };
        if over_quota(&status) && auto_delete {
            if let Some(quota) = quota_bytes {
                let target = quota.saturating_sub(required_bytes);
                if let Err(error) =
                    crate::gc::enforce_quota_with_clip_policy(media_dir, Some(target), None)
                {
                    tracing::warn!(
                        event = "storage_quota_auto_delete_failed",
                        path = ?media_dir,
                        error = %error,
                    );
                }
                status = clipline_storage::storage_status(media_dir, quota_bytes)
                    .map_err(|error| format!("storage status for {media_dir:?}: {error}"))?;
            }
        }
        if over_quota(&status) {
            let event = Event::StorageQuotaFull {
                total_bytes: status.total_bytes,
                quota_bytes: quota_bytes.expect("quota checked above"),
                required_bytes,
            };
            {
                let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
                inner.quota_blocked = Some(event.clone());
            }
            if announce {
                let _ = app.emit("storage-quota-full", event);
            }
            return Ok(false);
        }

        let should_restart = {
            let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
            inner.quota_blocked = None;
            inner.recording_desired
        };
        let _ = app.emit("storage-quota-resolved", ());
        if should_restart {
            self.start_recording(app)?;
        }
        Ok(true)
    }

    /// True only when the recorder actually received `cmd`. A stopped recorder
    /// thread can outlive the `tx` that fed it, and a caller that reports
    /// success there promises an outcome — an event, a sound — that never comes.
    pub(crate) fn send(&self, cmd: Cmd) -> bool {
        if let Ok(inner) = self.0.lock() {
            if let Some(tx) = &inner.tx {
                return tx.send(cmd).is_ok();
            }
        }
        false
    }

    pub(crate) fn osu_title_events_for_window(
        &self,
        start: Option<i64>,
        end: Option<i64>,
    ) -> Vec<OsuTitleEvent> {
        let Some(start) = start else {
            return Vec::new();
        };
        let end = end.unwrap_or_else(unix_now_i64);
        self.0
            .lock()
            .map(|inner| filter_osu_title_events(&inner.osu_title_events, start, end))
            .unwrap_or_default()
    }

    pub(crate) fn settings(&self) -> AppSettings {
        self.0
            .lock()
            .map(|inner| inner.settings.clone())
            .unwrap_or_default()
    }

    pub(crate) fn update_cloud<F>(&self, update: F) -> Result<AppSettings, String>
    where
        F: FnOnce(&mut crate::settings::CloudSettings),
    {
        self.update_cloud_with(update, AppSettings::save)
    }

    pub(crate) fn update_cloud_with<F>(
        &self,
        update: F,
        save: impl FnOnce(&AppSettings) -> Result<(), String>,
    ) -> Result<AppSettings, String>
    where
        F: FnOnce(&mut crate::settings::CloudSettings),
    {
        // Serialize cloud settings saves so concurrent uploads preserve their
        // read-modify-write order without holding runtime state during disk I/O.
        let _save_guard = CLOUD_SETTINGS_SAVE_LOCK
            .lock()
            .map_err(|_| "cloud settings save lock poisoned")?;
        let mut next = self
            .0
            .lock()
            .map_err(|_| "runtime state lock poisoned")?
            .settings
            .clone();
        update(&mut next.cloud);
        next.cloud.normalize();
        save(&next)?;
        let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
        inner.settings.cloud = next.cloud;
        Ok(inner.settings.clone())
    }

    pub(crate) fn update_osu<F>(&self, update: F) -> Result<AppSettings, String>
    where
        F: FnOnce(&mut crate::settings::OsuApiSettings),
    {
        self.update_osu_with(update, AppSettings::save)
    }

    pub(crate) fn update_osu_with<F>(
        &self,
        update: F,
        save: impl FnOnce(&AppSettings) -> Result<(), String>,
    ) -> Result<AppSettings, String>
    where
        F: FnOnce(&mut crate::settings::OsuApiSettings),
    {
        let _save_guard = CLOUD_SETTINGS_SAVE_LOCK
            .lock()
            .map_err(|_| "settings save lock poisoned")?;
        let mut next = self
            .0
            .lock()
            .map_err(|_| "runtime state lock poisoned")?
            .settings
            .clone();
        update(&mut next.osu);
        next.osu.normalize();
        save(&next)?;
        let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
        inner.settings.osu = next.osu;
        Ok(inner.settings.clone())
    }

    pub(crate) fn lock_cloud_settings_save() -> Result<MutexGuard<'static, ()>, String> {
        CLOUD_SETTINGS_SAVE_LOCK
            .lock()
            .map_err(|_| "cloud settings save lock poisoned".to_string())
    }

    pub(crate) fn active_shortcut_matches(&self, shortcut: &Shortcut) -> bool {
        if crate::hotkeys::actions_paused() {
            return false;
        }
        let Ok(inner) = self.0.lock() else {
            return false;
        };
        inner
            .settings
            .hotkeys()
            .into_iter()
            .filter_map(|raw| parse_global_hotkey(raw).ok().flatten())
            .any(|active| &active == shortcut)
    }

    pub(crate) fn set_recording<R: Runtime>(
        &self,
        app: AppHandle<R>,
        recording: bool,
    ) -> Result<bool, String> {
        if recording {
            self.start_recording(app)
        } else {
            self.stop_recording()
        }
    }

    pub(crate) fn start_recording<R: Runtime>(&self, app: AppHandle<R>) -> Result<bool, String> {
        let (started, blocked) = {
            let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
            if inner.tx.is_some() {
                return Ok(true);
            }
            inner.recording_desired = true;
            inner.last_save_request = None;
            if let Some(event) = inner.quota_blocked.clone() {
                (None, Some(event))
            } else if inner.manual_full_session_desired
                || automatic_start_allowed(&inner, &inner.settings)
            {
                let (tx, rx) = service::spawn(Self::options(&inner)?);
                let generation = Self::install_recording_sender(&mut inner, tx);
                (Some((rx, generation)), None)
            } else {
                (None, None)
            }
        };
        if let Some(event) = blocked {
            let _ = app.emit("storage-quota-full", event);
            return Ok(false);
        }
        if let Some((rx, generation)) = started {
            pump_events(app, rx, generation);
        } else if let Some(status) = self.current_waiting_status() {
            let _ = app.emit("status", status);
        } else {
            // No spawn and no game wait: an active game is blocked by the
            // League gate. Publish the stopped state instead of leaving the
            // rail on a stale recording status.
            let _ = app.emit("status", stopped_status());
        }
        Ok(true)
    }

    pub(crate) fn stop_recording(&self) -> Result<bool, String> {
        let tx = {
            let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
            inner.recording_desired = false;
            inner.manual_full_session_desired = false;
            inner.recording_generation = inner.recording_generation.wrapping_add(1);
            let tx = inner.tx.take();
            inner.last_save_request = None;
            tx
        };
        if let Some(tx) = tx {
            let _ = tx.send(Cmd::Stop { announce: true });
        }
        Ok(false)
    }

    pub(crate) fn set_session_recording<R: Runtime>(
        &self,
        app: AppHandle<R>,
        recording: bool,
    ) -> Result<bool, String> {
        if !recording {
            let (tx, restart) = {
                let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
                Self::prepare_manual_session_stop(&mut inner)?
            };
            let session_stopped = tx
                .map(|tx| tx.send(Cmd::StopFullSession).is_ok())
                .unwrap_or(true);
            if let Some(prepared) = restart {
                self.finish_service_restart(app, prepared)?;
            }
            if !session_stopped {
                return Err("recorder stopped before the session could be finalized".into());
            }
            return Ok(false);
        }

        let (started, existing, blocked) = {
            let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
            if let Some(event) = Self::arm_manual_session_unless_blocked(&mut inner) {
                (None, None, Some(event))
            } else if let Some(tx) = inner.tx.clone() {
                (None, Some(tx), None)
            } else {
                let (tx, rx) = service::spawn(Self::options(&inner)?);
                let generation = Self::install_recording_sender(&mut inner, tx);
                (Some((rx, generation)), None, None)
            }
        };
        if let Some(event) = blocked {
            let _ = app.emit("storage-quota-full", event);
            return Ok(false);
        }
        if let Some(tx) = existing {
            tx.send(Cmd::StartFullSession)
                .map_err(|_| "recorder stopped before the session could start")?;
        }
        if let Some((rx, generation)) = started {
            pump_events(app, rx, generation);
        }
        Ok(true)
    }

    pub(crate) fn toggle_session_recording_from_hotkey<R: Runtime>(
        &self,
        app: AppHandle<R>,
    ) -> Result<bool, String> {
        let recording = {
            let inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
            inner.manual_full_session_desired
                || (inner.tx.is_some()
                    && inner
                        .last_recorder_status
                        .as_ref()
                        .is_some_and(|status| status.full_session))
        };
        self.set_session_recording(app, !recording)
    }

    pub(crate) fn set_detected_game<R: Runtime>(
        &self,
        app: AppHandle<R>,
        detected: Option<DetectedGame>,
        league_lookup: Option<LeagueGateLookup>,
    ) -> Result<(), String> {
        let (prepared_restart, emit_event, event) = {
            let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
            Self::plan_detection_transition(&mut inner, detected, league_lookup)?
        };
        if let Some(prepared) = prepared_restart {
            self.finish_service_restart(app.clone(), prepared)?;
        }
        if emit_event {
            let _ = app.emit("game-detection", event);
        }
        Ok(())
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{RuntimeInner, RuntimeState, active_game_still_configured, detected_game, record_osu_title_event, recorder_should_run};
    use crate::settings::{GameRecordingMode};
    use std::sync::mpsc;

    #[test]
    fn failed_cloud_settings_save_leaves_live_state_unchanged() {
        let state = RuntimeState::new(AppSettings::default(), None);

        let error = state
            .update_cloud_with(
                |cloud| cloud.host_url = "https://new.example".into(),
                |candidate| {
                    assert_eq!(candidate.cloud.host_url, "https://new.example");
                    Err("disk full".into())
                },
            )
            .unwrap_err();

        assert_eq!(error, "disk full");
        assert!(state.settings().cloud.host_url.is_empty());
    }

    #[test]
    fn failed_osu_settings_save_leaves_live_state_unchanged() {
        let state = RuntimeState::new(AppSettings::default(), None);

        let error = state
            .update_osu_with(
                |osu| osu.client_id = Some("1234".into()),
                |candidate| {
                    assert_eq!(candidate.osu.client_id.as_deref(), Some("1234"));
                    Err("settings denied".into())
                },
            )
            .unwrap_err();

        assert_eq!(error, "settings denied");
        assert!(state.settings().osu.client_id.is_none());
    }

    #[test]
    fn request_save_debounces_only_immediate_duplicate_triggers() {
        let (tx, rx) = mpsc::channel();
        let state = RuntimeState::with_sender(tx, AppSettings::default(), None);

        assert!(state.request_save());
        assert!(matches!(rx.try_recv(), Ok(Cmd::Save)));

        assert!(!state.request_save());
        assert!(rx.try_recv().is_err());

        {
            let mut inner = state.0.lock().unwrap();
            inner.last_save_request = Some(Instant::now() - Duration::from_millis(151));
        }

        assert!(state.request_save());
        assert!(matches!(rx.try_recv(), Ok(Cmd::Save)));
    }

    #[test]
    fn games_only_policy_requires_detection_and_an_active_game() {
        let mut settings = AppSettings::default();
        settings.games.pause_when_no_game = true;

        assert!(!recorder_should_run(&settings, None));
        assert!(recorder_should_run(
            &settings,
            Some(&detected_game("custom-game", "Game", 42))
        ));

        settings.games.auto_detect = false;
        assert!(recorder_should_run(&settings, None));

        settings.games.auto_detect = true;
        settings.games.pause_when_no_game = false;
        assert!(recorder_should_run(&settings, None));
    }

    #[test]
    fn manual_session_bypasses_games_only_waiting_with_full_session_mode() {
        let mut settings = AppSettings::default();
        settings.games.pause_when_no_game = true;
        let state = RuntimeState::new(settings, None);
        let mut inner = state.0.lock().unwrap();
        inner.recording_desired = true;
        inner.manual_full_session_desired = true;

        let prepared = RuntimeState::prepare_service_restart(&mut inner).unwrap();
        let (options, _) = prepared.replacement.expect("manual recording starts capture");

        assert_eq!(options.recording_mode, service::RecordingMode::FullSession);
        assert!(!prepared.waiting_for_game);
    }

    #[test]
    fn quota_blocked_manual_session_request_does_not_arm_a_future_recording() {
        let state = RuntimeState::new(AppSettings::default(), None);
        let mut inner = state.0.lock().unwrap();
        inner.quota_blocked = Some(Event::StorageQuotaFull {
            total_bytes: 10,
            quota_bytes: 10,
            required_bytes: 1,
        });

        let blocked = RuntimeState::arm_manual_session_unless_blocked(&mut inner);

        assert!(blocked.is_some());
        assert!(!inner.manual_full_session_desired);
        assert!(!inner.recording_desired);
    }

    #[test]
    fn stopping_manual_session_returns_games_only_capture_to_waiting() {
        let (tx, _rx) = mpsc::channel();
        let mut settings = AppSettings::default();
        settings.games.pause_when_no_game = true;
        let mut inner = RuntimeInner {
            tx: Some(tx),
            recording_generation: 1,
            recording_desired: true,
            manual_full_session_desired: true,
            settings,
            lol_url: None,
            active_game: None,
            osu_title_events: Vec::new(),
            last_save_request: None,
            decodable_codecs: vec![service::Codec::H264],
            last_recorder_status: None,
            last_storage_status: None,
            recent_recorder_error: false,
            quota_blocked: None,
            league_gate: None,
            league_gate_rx: None,
        };

        let (_, restart) = RuntimeState::prepare_manual_session_stop(&mut inner).unwrap();
        let restart = restart.expect("games-only capture must return to waiting");

        assert!(!inner.manual_full_session_desired);
        assert!(inner.recording_desired);
        assert!(restart.old_tx.is_some());
        assert!(restart.replacement.is_none());
        assert!(restart.waiting_for_game);
    }

    #[test]
    fn osu_title_events_record_only_changed_osu_titles() {
        let mut inner = RuntimeInner {
            tx: None,
            recording_generation: 0,
            recording_desired: false,
            manual_full_session_desired: false,
            settings: AppSettings::default(),
            lol_url: None,
            active_game: None,
            osu_title_events: Vec::new(),
            last_save_request: None,
            decodable_codecs: vec![service::Codec::H264],
            last_recorder_status: None,
            last_storage_status: None,
            recent_recorder_error: false,
            quota_blocked: None,
            league_gate: None,
            league_gate_rx: None,
        };
        let osu = DetectedGame {
            identity: crate::game_identity::GameIdentity::built_in_plugin(
                crate::game_plugins::OSU_ID,
            )
            .unwrap(),
            name: "osu!".into(),
            hwnd: 42,
            window_title: "osu! - xi - Blue Zenith [FOUR DIMENSIONS]".into(),
            process_id: 7,
            exe_name: "osu!.exe".into(),
            exe_path: None,
            recording_mode: GameRecordingMode::FullSession,
        };
        let league = DetectedGame {
            identity: crate::game_identity::GameIdentity::built_in_plugin(
                crate::game_plugins::LEAGUE_OF_LEGENDS_ID,
            )
            .unwrap(),
            name: "League of Legends".into(),
            window_title: "League".into(),
            exe_name: "League of Legends.exe".into(),
            ..osu.clone()
        };

        record_osu_title_event(&mut inner, Some(&osu), 100);
        record_osu_title_event(&mut inner, Some(&osu), 101);
        record_osu_title_event(&mut inner, Some(&league), 102);
        record_osu_title_event(
            &mut inner,
            Some(&DetectedGame {
                identity: crate::game_identity::GameIdentity::custom(crate::game_plugins::OSU_ID),
                name: "Custom impostor".into(),
                window_title: "must not be tracked".into(),
                exe_name: "impostor.exe".into(),
                ..osu.clone()
            }),
            102,
        );
        record_osu_title_event(
            &mut inner,
            Some(&DetectedGame {
                window_title: "osu!".into(),
                ..osu.clone()
            }),
            103,
        );

        assert_eq!(
            inner.osu_title_events,
            vec![
                OsuTitleEvent {
                    unix_s: 100,
                    title: "osu! - xi - Blue Zenith [FOUR DIMENSIONS]".into(),
                },
                OsuTitleEvent {
                    unix_s: 103,
                    title: "osu!".into(),
                }
            ]
        );
    }

    #[test]
    fn osu_title_events_for_window_filters_to_saved_recording_window() {
        let state = RuntimeState::new(AppSettings::default(), None);
        {
            let mut inner = state.0.lock().unwrap();
            inner.osu_title_events = vec![
                OsuTitleEvent {
                    unix_s: 90,
                    title: "too early".into(),
                },
                OsuTitleEvent {
                    unix_s: 96,
                    title: "start margin".into(),
                },
                OsuTitleEvent {
                    unix_s: 150,
                    title: "inside".into(),
                },
                OsuTitleEvent {
                    unix_s: 206,
                    title: "too late".into(),
                },
            ];
        }

        let titles: Vec<_> = state
            .osu_title_events_for_window(Some(100), Some(200))
            .into_iter()
            .map(|event| event.title)
            .collect();

        assert_eq!(titles, vec!["start margin", "inside"]);
    }

    #[test]
    fn built_in_league_profile_counts_as_active_game_configuration() {
        let active = DetectedGame {
            identity: crate::game_identity::GameIdentity::built_in_plugin(
                crate::game_plugins::LEAGUE_OF_LEGENDS_ID,
            )
            .unwrap(),
            name: "League of Legends".into(),
            hwnd: 42,
            window_title: "League of Legends (TM) Client".into(),
            process_id: 7,
            exe_name: "League of Legends.exe".into(),
            exe_path: None,
            recording_mode: GameRecordingMode::FullSession,
        };
        let mut settings = AppSettings::default();

        assert!(active_game_still_configured(&settings, Some(&active)));

        settings.games.plugins.insert(
            crate::game_plugins::LEAGUE_OF_LEGENDS_ID.into(),
            crate::settings::GamePluginSettings {
                enabled: false,
                recording_mode: GameRecordingMode::FullSession,
                review: Default::default(),
            },
        );
        assert!(!active_game_still_configured(&settings, Some(&active)));
    }
}
