use std::sync::mpsc::{Receiver, Sender};
#[cfg(test)]
use crate::settings::GameRecordingMode;

use tauri::{
    AppHandle, Emitter, Runtime,
};

use clipline_lol::LeagueQueue;

use crate::games::DetectedGame;
use crate::osu_enrichment::OsuTitleEvent;
use crate::service::{Cmd, Event, ServiceOptions};
use crate::settings::AppSettings;
use super::*;

/// Per-detection League game-type gate verdict. `None` on the runtime means
/// no gate applies (not a League game, or every category is set to record).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LeagueGateVerdict {
    /// Queue lookup in flight; automatic recording must not start yet.
    Pending,
    Allowed,
    Denied,
}

/// Outcome of a resolved gate lookup. `Allowed` carries the restart that
/// spawns the deferred recorder — `None` when a manual session already runs
/// (manual bypasses the gate, so nothing should be restarted). `Denied` skips
/// the recorder and should notify the user.
pub(crate) enum LeagueGateResolution {
    Allowed(Option<Box<PreparedServiceRestart>>),
    Denied,
}

pub(crate) type LeagueGateLookup =
    Box<dyn FnOnce(&DetectedGame) -> Receiver<Option<LeagueQueue>> + Send>;

pub(crate) const LEAGUE_GATE_SKIP_NOTICE: &str =
    "League of Legends: this game type is set to not record; skipped.";

#[derive(Clone)]
pub(crate) struct RecorderDiagnosticStatus {
    pub(crate) recording: bool,
    pub(crate) waiting_for_game: bool,
    pub(crate) segments: usize,
    pub(crate) buffered_s: f64,
    pub(crate) buffered_mb: f64,
    pub(crate) full_session: bool,
    pub(crate) encoder: String,
    pub(crate) capture_backend: String,
}

#[derive(Clone)]
pub(crate) struct StorageDiagnosticStatus {
    pub(crate) total_bytes: u64,
    pub(crate) quota_bytes: Option<u64>,
    pub(crate) over_quota: bool,
}

pub(crate) struct PreparedRuntimeRestart {
    pub(crate) settings: AppSettings,
}

pub(crate) struct PreparedServiceRestart {
    pub(crate) old_tx: Option<Sender<Cmd>>,
    pub(crate) replacement: Option<(ServiceOptions, u64)>,
    pub(crate) waiting_for_game: bool,
    pub(crate) waiting_generation: Option<u64>,
}

#[derive(Debug)]
pub(crate) struct CommittedRuntimeRestart<T> {
    pub(crate) old_tx: Option<Sender<Cmd>>,
    pub(crate) replacement: Option<(T, u64)>,
    pub(crate) cleared_active_game: bool,
    pub(crate) waiting_for_game: bool,
    pub(crate) waiting_generation: Option<u64>,
}

pub(crate) fn recorder_should_run(settings: &AppSettings, active_game: Option<&DetectedGame>) -> bool {
    !settings.games.auto_detect || !settings.games.pause_when_no_game || active_game.is_some()
}

pub(crate) fn waiting_for_game_status() -> Event {
    Event::Status {
        recording: false,
        waiting_for_game: true,
        segments: 0,
        buffered_s: 0.0,
        buffered_mb: 0.0,
        full_session: false,
        encoder: String::new(),
        capture_backend: String::new(),
    }
}

/// Truthful idle status for a stopped recorder with no game wait (e.g. the
/// League gate blocking an active game). Keeps the rail from showing a stale
/// "recording" state after the gate tears a service down.
pub(crate) fn stopped_status() -> Event {
    Event::Status {
        recording: false,
        waiting_for_game: false,
        segments: 0,
        buffered_s: 0.0,
        buffered_mb: 0.0,
        full_session: false,
        encoder: String::new(),
        capture_backend: String::new(),
    }
}

pub(crate) fn emit_waiting_for_game<R: Runtime>(app: &AppHandle<R>) {
    let _ = app.emit("status", waiting_for_game_status());
}

pub(crate) fn record_osu_title_event(inner: &mut RuntimeInner, detected: Option<&DetectedGame>, unix_s: i64) {
    const MAX_OSU_TITLE_EVENTS: usize = 512;
    let Some(game) = detected else {
        return;
    };
    if !game
        .identity
        .is_built_in_plugin(crate::game_plugins::OSU_ID)
    {
        return;
    }
    let title = game.window_title.trim();
    if title.is_empty() {
        return;
    }
    if inner
        .osu_title_events
        .last()
        .is_some_and(|event| event.title == title)
    {
        return;
    }
    inner.osu_title_events.push(OsuTitleEvent {
        unix_s,
        title: title.to_string(),
    });
    if inner.osu_title_events.len() > MAX_OSU_TITLE_EVENTS {
        let overflow = inner.osu_title_events.len() - MAX_OSU_TITLE_EVENTS;
        inner.osu_title_events.drain(0..overflow);
    }
}

pub(crate) fn filter_osu_title_events(events: &[OsuTitleEvent], start: i64, end: i64) -> Vec<OsuTitleEvent> {
    let start = start - 5;
    let end = end.max(start) + 5;
    events
        .iter()
        .filter(|event| event.unix_s >= start && event.unix_s <= end)
        .cloned()
        .collect()
}

pub(crate) fn preserve_backend_owned_settings_fields(settings: &mut AppSettings, backend: &AppSettings) {
    settings.cloud.host_url = backend.cloud.host_url.clone();
    settings.cloud.public_url = backend.cloud.public_url.clone();
    settings.cloud.connected_user_id = backend.cloud.connected_user_id.clone();
    settings.cloud.connected_username = backend.cloud.connected_username.clone();
    settings.cloud.connected_display_name = backend.cloud.connected_display_name.clone();
    settings.cloud.credential_target = backend.cloud.credential_target.clone();
    settings.cloud.credential_cleanup_targets = backend.cloud.credential_cleanup_targets.clone();
    settings.cloud.uploads = backend.cloud.uploads.clone();
    settings.osu = backend.osu.clone();
}

pub(crate) fn game_recording_mode_changed(
    current: Option<&DetectedGame>,
    next: Option<&DetectedGame>,
) -> bool {
    match (current, next) {
        (Some(current), Some(next)) => current.recording_mode != next.recording_mode,
        _ => false,
    }
}

pub(crate) fn active_game_still_configured(settings: &AppSettings, active: Option<&DetectedGame>) -> bool {
    let Some(active) = active else { return true };
    if !settings.games.auto_detect {
        return false;
    }
    match &active.identity {
        crate::game_identity::GameIdentity::BuiltInPlugin(_) => {
            crate::games::built_in_game_still_configured(&settings.games, &active.identity)
        }
        crate::game_identity::GameIdentity::Custom(id) => settings
            .games
            .custom_games
            .iter()
            .any(|game| game.enabled && game.id == *id),
    }
}

#[cfg(test)]
pub(crate) fn detected_game(id: &str, name: &str, hwnd: isize) -> DetectedGame {
    DetectedGame {
        identity: crate::game_identity::GameIdentity::custom(id),
        name: name.into(),
        hwnd,
        window_title: format!("{name} Window"),
        process_id: hwnd as u32,
        exe_name: format!("{name}.exe"),
        exe_path: None,
        recording_mode: GameRecordingMode::FullSession,
    }
}

#[cfg(test)]
pub(crate) fn detected_built_in_game(id: &str, name: &str, hwnd: isize) -> DetectedGame {
    DetectedGame {
        identity: crate::game_identity::GameIdentity::built_in_plugin(id)
            .expect("test built-in id"),
        name: name.into(),
        hwnd,
        window_title: format!("{name} Window"),
        process_id: hwnd as u32,
        exe_name: format!("{name}.exe"),
        exe_path: None,
        recording_mode: GameRecordingMode::FullSession,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{RuntimeState, same_game_window};
    use crate::settings::{CloudUploadRecord, GameRecordingMode};

    #[test]
    fn current_game_detection_is_available_for_frontend_replay() {
        let state = RuntimeState::new(AppSettings::default(), None);
        state.0.lock().unwrap().active_game = Some(detected_game("game", "Game", 42));

        let event = state.current_game_detection_for_replay().unwrap();

        assert!(event.active);
        assert_eq!(event.name.as_deref(), Some("Game"));
        assert_eq!(event.window_title.as_deref(), Some("Game Window"));
    }

    #[test]
    fn preserve_backend_owned_settings_fields_keeps_upload_state_but_allows_preferences() {
        let mut frontend = AppSettings::default();
        frontend.cloud.host_url = "https://stale.example.com".into();
        frontend.cloud.public_url = Some("https://stale-public.example.com".into());
        frontend.cloud.connected_user_id = Some("stale-user".into());
        frontend.cloud.connected_username = Some("stale-name".into());
        frontend.cloud.connected_display_name = Some("Stale".into());
        frontend.cloud.credential_target = Some("stale-target".into());
        frontend.cloud.default_visibility = "public".into();
        frontend.cloud.delete_local_after_upload = true;
        frontend.cloud.auto_upload_rules = true;

        let mut backend = AppSettings::default();
        backend.cloud.host_url = "https://cloud.example.com".into();
        backend.cloud.public_url = Some("https://public.example.com".into());
        backend.cloud.connected_user_id = Some("user-1".into());
        backend.cloud.connected_username = Some("dain".into());
        backend.cloud.connected_display_name = Some("Dain".into());
        backend.cloud.credential_target = Some("clipline:user-1".into());
        backend.cloud.credential_cleanup_targets = vec!["clipline:old-user".into()];
        backend.cloud.uploads.insert(
            "local-1".into(),
            CloudUploadRecord {
                local_clip_id: "local-1".into(),
                path: "D:\\Videos\\Clipline\\clip.mp4".into(),
                remote_clip_id: Some("remote-1".into()),
                remote_url: Some("https://public.example.com/remote-1".into()),
                visibility: "private".into(),
                upload_status: "uploaded_private".into(),
                error: None,
                updated_at_unix: 42,
            },
        );

        preserve_backend_owned_settings_fields(&mut frontend, &backend);

        assert_eq!(frontend.cloud.host_url, backend.cloud.host_url);
        assert_eq!(frontend.cloud.public_url, backend.cloud.public_url);
        assert_eq!(
            frontend.cloud.connected_user_id,
            backend.cloud.connected_user_id
        );
        assert_eq!(
            frontend.cloud.connected_username,
            backend.cloud.connected_username
        );
        assert_eq!(
            frontend.cloud.connected_display_name,
            backend.cloud.connected_display_name
        );
        assert_eq!(
            frontend.cloud.credential_target,
            backend.cloud.credential_target
        );
        assert_eq!(
            frontend.cloud.credential_cleanup_targets,
            backend.cloud.credential_cleanup_targets
        );
        assert_eq!(frontend.cloud.uploads, backend.cloud.uploads);
        assert_eq!(frontend.cloud.default_visibility, "public");
        assert!(frontend.cloud.delete_local_after_upload);
        assert!(frontend.cloud.auto_upload_rules);
    }

    #[test]
    fn preserve_backend_owned_settings_fields_keeps_osu_credentials_from_backend() {
        let mut frontend = AppSettings::default();
        frontend.osu.client_id = None;
        frontend.osu.user = None;
        frontend.osu.credential_target = None;
        frontend.osu.last_connected_username = None;

        let mut backend = AppSettings::default();
        backend.osu.client_id = Some("61835".into());
        backend.osu.user = Some("3426414".into());
        backend.osu.credential_target = Some("Clipline osu!:61835:3426414".into());
        backend.osu.credential_cleanup_targets = vec!["Clipline osu!:old".into()];
        backend.osu.last_connected_username = Some("Dain".into());

        preserve_backend_owned_settings_fields(&mut frontend, &backend);

        assert_eq!(frontend.osu, backend.osu);
    }

    #[test]
    fn detected_game_identity_ignores_volatile_window_title() {
        let current = DetectedGame {
            identity: crate::game_identity::GameIdentity::custom("custom-game"),
            name: "Game".into(),
            hwnd: 42,
            window_title: "Loading".into(),
            process_id: 7,
            exe_name: "game.exe".into(),
            exe_path: None,
            recording_mode: GameRecordingMode::ReplaysOnly,
        };
        let updated_title = DetectedGame {
            window_title: "Paused".into(),
            ..current.clone()
        };
        let different_window = DetectedGame {
            hwnd: 43,
            ..current.clone()
        };

        assert!(same_game_window(Some(&current), Some(&updated_title)));
        assert!(!same_game_window(Some(&current), Some(&different_window)));
    }

    #[test]
    fn detected_game_recording_mode_change_requires_service_restart() {
        let current = DetectedGame {
            identity: crate::game_identity::GameIdentity::custom("custom-game"),
            name: "Game".into(),
            hwnd: 42,
            window_title: "Game".into(),
            process_id: 7,
            exe_name: "game.exe".into(),
            exe_path: None,
            recording_mode: GameRecordingMode::ReplaysOnly,
        };
        let updated_mode = DetectedGame {
            recording_mode: GameRecordingMode::FullSession,
            ..current.clone()
        };
        let updated_title = DetectedGame {
            window_title: "Game - Loading".into(),
            ..current.clone()
        };

        assert!(same_game_window(Some(&current), Some(&updated_mode)));
        assert!(game_recording_mode_changed(
            Some(&current),
            Some(&updated_mode)
        ));
        assert!(!game_recording_mode_changed(
            Some(&current),
            Some(&updated_title)
        ));
    }
}
