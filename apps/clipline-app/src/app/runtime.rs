use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Mutex;
use std::time::Instant;

use tauri::{
    AppHandle, Emitter, Runtime,
};

use clipline_lol::LeagueQueue;

use crate::games::DetectedGame;
use crate::osu_enrichment::OsuTitleEvent;
use crate::service::{self, Cmd, Event};
use crate::settings::AppSettings;
use super::*;

pub(crate) struct RuntimeState(pub(crate) Mutex<RuntimeInner>);

pub(crate) static CLOUD_SETTINGS_SAVE_LOCK: Mutex<()> = Mutex::new(());

pub(crate) struct RuntimeInner {
    pub(crate) tx: Option<Sender<Cmd>>,
    pub(crate) recording_generation: u64,
    pub(crate) recording_desired: bool,
    pub(crate) manual_full_session_desired: bool,
    pub(crate) settings: AppSettings,
    pub(crate) lol_url: Option<String>,
    pub(crate) active_game: Option<DetectedGame>,
    pub(crate) osu_title_events: Vec<OsuTitleEvent>,
    pub(crate) last_save_request: Option<Instant>,
    /// Codecs WebView2 can decode, reported by the frontend. Drives the
    /// recorder's Automatic selection; H.264 is the always-safe default.
    pub(crate) decodable_codecs: Vec<service::Codec>,
    pub(crate) last_recorder_status: Option<RecorderDiagnosticStatus>,
    pub(crate) last_storage_status: Option<StorageDiagnosticStatus>,
    pub(crate) recent_recorder_error: bool,
    pub(crate) quota_blocked: Option<Event>,
    /// Gate verdict for the currently detected League game, if any.
    pub(crate) league_gate: Option<LeagueGateVerdict>,
    /// Pending queue lookup result; drained by `tick_league_gate`.
    pub(crate) league_gate_rx: Option<Receiver<Option<LeagueQueue>>>,
}

impl RuntimeState {
    pub(crate) fn new(settings: AppSettings, lol_url: Option<String>) -> Self {
        Self::from_parts(None, settings, lol_url)
    }

    #[cfg(test)]
    pub(crate) fn with_sender(tx: Sender<Cmd>, settings: AppSettings, lol_url: Option<String>) -> Self {
        Self::from_parts(Some(tx), settings, lol_url)
    }

    pub(crate) fn from_parts(tx: Option<Sender<Cmd>>, settings: AppSettings, lol_url: Option<String>) -> Self {
        let mut inner = RuntimeInner {
            tx: None,
            recording_generation: 0,
            recording_desired: false,
            manual_full_session_desired: false,
            settings,
            lol_url,
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
        if let Some(tx) = tx {
            Self::install_recording_sender(&mut inner, tx);
        }
        Self(Mutex::new(inner))
    }

    pub(crate) fn install_recording_sender(inner: &mut RuntimeInner, tx: Sender<Cmd>) -> u64 {
        inner.recording_generation = inner.recording_generation.wrapping_add(1);
        inner.recording_desired = true;
        inner.tx = Some(tx);
        inner.last_save_request = None;
        inner.recording_generation
    }

    pub(crate) fn accept_service_status(&self, generation: u64, recording: bool) -> bool {
        let Ok(mut inner) = self.0.lock() else {
            return false;
        };
        if inner.recording_generation != generation || inner.tx.is_none() {
            return false;
        }
        if !recording {
            inner.tx = None;
            if inner.quota_blocked.is_none() {
                inner.recording_desired = false;
                inner.manual_full_session_desired = false;
            }
            inner.recording_generation = inner.recording_generation.wrapping_add(1);
            inner.last_save_request = None;
        }
        true
    }

    pub(crate) fn accept_service_quota(&self, generation: u64, event: &Event) -> bool {
        let Event::StorageQuotaFull {
            total_bytes,
            quota_bytes,
            ..
        } = event
        else {
            return false;
        };
        let Ok(mut inner) = self.0.lock() else {
            return false;
        };
        if inner.recording_generation != generation || inner.tx.is_none() {
            return false;
        }
        inner.quota_blocked = Some(event.clone());
        inner.last_storage_status = Some(StorageDiagnosticStatus {
            total_bytes: *total_bytes,
            quota_bytes: Some(*quota_bytes),
            over_quota: true,
        });
        true
    }

    pub(crate) fn observe_runtime_event(&self, event: &Event) {
        let Ok(mut inner) = self.0.lock() else {
            return;
        };
        match event {
            Event::Status {
                recording,
                waiting_for_game,
                segments,
                buffered_s,
                buffered_mb,
                full_session,
                encoder,
                capture_backend,
            } => {
                inner.last_recorder_status = Some(RecorderDiagnosticStatus {
                    recording: *recording,
                    waiting_for_game: *waiting_for_game,
                    segments: *segments,
                    buffered_s: *buffered_s,
                    buffered_mb: *buffered_mb,
                    full_session: *full_session,
                    encoder: encoder.clone(),
                    capture_backend: capture_backend.clone(),
                });
                if *recording {
                    inner.recent_recorder_error = false;
                }
            }
            Event::Saved {
                storage_total_bytes,
                storage_quota_bytes,
                storage_over_quota,
                ..
            } => {
                inner.last_storage_status = Some(StorageDiagnosticStatus {
                    total_bytes: *storage_total_bytes,
                    quota_bytes: *storage_quota_bytes,
                    over_quota: *storage_over_quota,
                });
            }
            Event::StorageQuotaFull { .. } => {}
            Event::BookmarkAdded { .. } => {}
            Event::LibraryChanged => {}
            Event::Error { .. } => inner.recent_recorder_error = true,
            Event::MediaRootResolved { .. } => {}
        }
    }

    pub(crate) fn current_waiting_status(&self) -> Option<Event> {
        let inner = self.0.lock().ok()?;
        (inner.recording_desired
            && inner.tx.is_none()
            && inner.quota_blocked.is_none()
            && !inner.manual_full_session_desired
            && !recorder_should_run(&inner.settings, inner.active_game.as_ref()))
        .then(waiting_for_game_status)
    }

    /// Prefer the live waiting-for-game state; otherwise replay the last durable
    /// recorder status so a recreated UI can rehydrate without waiting for the
    /// next service tick.
    pub(crate) fn durable_recorder_status_for_replay(&self) -> Option<Event> {
        if let Some(waiting) = self.current_waiting_status() {
            return Some(waiting);
        }
        let inner = self.0.lock().ok()?;
        let status = inner.last_recorder_status.as_ref()?;
        Some(Event::Status {
            recording: status.recording,
            waiting_for_game: status.waiting_for_game,
            segments: status.segments,
            buffered_s: status.buffered_s,
            buffered_mb: status.buffered_mb,
            full_session: status.full_session,
            encoder: status.encoder.clone(),
            capture_backend: status.capture_backend.clone(),
        })
    }

    pub(crate) fn current_game_detection_for_replay(&self) -> Option<GameDetectionEvent> {
        let detected = self.0.lock().ok()?.active_game.clone();
        Some(GameDetectionEvent::from_detected(detected.as_ref()))
    }

    pub(crate) fn durable_quota_event_for_replay(&self) -> Option<Event> {
        self.0.lock().ok()?.quota_blocked.clone()
    }

    pub(crate) fn waiting_generation_is_current(&self, generation: u64) -> bool {
        self.0.lock().is_ok_and(|inner| {
            inner.recording_generation == generation
                && inner.recording_desired
                && inner.tx.is_none()
                && !inner.manual_full_session_desired
                && !recorder_should_run(&inner.settings, inner.active_game.as_ref())
        })
    }

    /// Replace the decodable-codec set from the frontend's canPlayType probe.
    /// Unknown keys are ignored; H.264 is always retained as the safe floor.
    pub(crate) fn set_decodable_codecs(&self, keys: &[String]) {
        let mut codecs = vec![service::Codec::H264];
        for key in keys {
            match key.as_str() {
                "hevc" if !codecs.contains(&service::Codec::Hevc) => {
                    codecs.push(service::Codec::Hevc)
                }
                "av1" if !codecs.contains(&service::Codec::Av1) => codecs.push(service::Codec::Av1),
                _ => {}
            }
        }
        match self.0.lock() {
            Ok(mut inner) => inner.decodable_codecs = codecs,
            Err(e) => tracing::error!(event = "decode_codec_state_lock_poisoned", error = %e),
        }
    }

    /// Build service options for the supplied settings and runtime context.
    pub(crate) fn options_for(
        settings: &AppSettings,
        lol_url: Option<String>,
        active_game: Option<&DetectedGame>,
        decodable_codecs: &[service::Codec],
    ) -> Result<service::ServiceOptions, String> {
        let mut opts = settings.to_service_options(lol_url)?;
        opts.decodable_codecs = decodable_codecs.to_vec();
        if let Some(game) = active_game {
            opts.capture_source = service::CaptureSource::WindowHandle {
                hwnd: game.hwnd,
                title: game.window_title.clone(),
            };
            opts.recording_mode = game.recording_mode.into();
            opts.active_game = Some(service::ActiveGame {
                identity: game.identity.clone(),
                name: game.name.clone(),
                exe_path: game.exe_path.as_deref().map(PathBuf::from),
                process_id: Some(game.process_id),
            });
        }
        Ok(opts)
    }

    pub(crate) fn options(inner: &RuntimeInner) -> Result<service::ServiceOptions, String> {
        let mut options = Self::options_for(
            &inner.settings,
            inner.lol_url.clone(),
            inner.active_game.as_ref(),
            &inner.decodable_codecs,
        )?;
        if inner.manual_full_session_desired {
            options.recording_mode = service::RecordingMode::FullSession;
        }
        Ok(options)
    }

    pub(crate) fn prepare_service_restart(inner: &mut RuntimeInner) -> Result<PreparedServiceRestart, String> {
        let should_run = inner.recording_desired
            && inner.quota_blocked.is_none()
            && (inner.manual_full_session_desired
                || automatic_start_allowed(inner, &inner.settings));
        let next_options = if should_run {
            let mut options = match Self::options(inner) {
                Ok(options) => options,
                Err(error) => {
                    // A sender means the current service is still authoritative,
                    // so preserve it on an option error. With no sender, a prior
                    // restart is already spawning; invalidate that stale plan.
                    if inner.tx.is_none() {
                        inner.recording_generation = inner.recording_generation.wrapping_add(1);
                    }
                    return Err(error);
                }
            };
            options.recover_abandoned_recordings = false;
            Some(options)
        } else {
            None
        };
        let old_tx = inner.tx.take();
        inner.recording_generation = inner.recording_generation.wrapping_add(1);
        let generation = inner.recording_generation;
        inner.last_save_request = None;
        Ok(PreparedServiceRestart {
            old_tx,
            replacement: next_options.map(|options| (options, generation)),
            waiting_for_game: inner.recording_desired
                && inner.quota_blocked.is_none()
                && !inner.manual_full_session_desired
                && !recorder_should_run(&inner.settings, inner.active_game.as_ref()),
            waiting_generation: (inner.recording_desired
                && inner.quota_blocked.is_none()
                && !inner.manual_full_session_desired
                && !recorder_should_run(&inner.settings, inner.active_game.as_ref()))
                .then_some(generation),
        })
    }

    pub(crate) fn arm_manual_session_unless_blocked(inner: &mut RuntimeInner) -> Option<Event> {
        if let Some(event) = inner.quota_blocked.clone() {
            return Some(event);
        }
        inner.manual_full_session_desired = true;
        inner.recording_desired = true;
        inner.last_save_request = None;
        None
    }

    pub(crate) fn prepare_manual_session_stop(
        inner: &mut RuntimeInner,
    ) -> Result<(Option<Sender<Cmd>>, Option<PreparedServiceRestart>), String> {
        inner.manual_full_session_desired = false;
        if inner.recording_desired
            && inner.quota_blocked.is_none()
            && !automatic_start_allowed(inner, &inner.settings)
        {
            let restart = Self::prepare_service_restart(inner)?;
            let session_tx = restart.old_tx.clone();
            Ok((session_tx, Some(restart)))
        } else {
            Ok((inner.tx.clone(), None))
        }
    }

    pub(crate) fn install_prepared_service_restart(
        inner: &mut RuntimeInner,
        generation: u64,
        tx: Sender<Cmd>,
    ) -> Result<u64, Sender<Cmd>> {
        if !inner.recording_desired
            || inner.quota_blocked.is_some()
            || inner.recording_generation != generation
            || inner.tx.is_some()
        {
            return Err(tx);
        }
        Ok(Self::install_recording_sender(inner, tx))
    }

    pub(crate) fn finish_service_restart<R: Runtime>(
        &self,
        app: AppHandle<R>,
        prepared: PreparedServiceRestart,
    ) -> Result<(), String> {
        let waiting_for_game = prepared.waiting_for_game;
        let waiting_generation = prepared.waiting_generation;
        let stopped_without_replacement =
            prepared.old_tx.is_some() && prepared.replacement.is_none() && !waiting_for_game;
        if let Some(tx) = prepared.old_tx {
            let _ = tx.send(Cmd::Stop { announce: false });
        }
        if let Some((options, restart_generation)) = prepared.replacement {
            let (tx, rx) = service::spawn(options);
            let installed = {
                let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
                Self::install_prepared_service_restart(&mut inner, restart_generation, tx)
            };
            match installed {
                Ok(generation) => pump_events(app.clone(), rx, generation),
                Err(tx) => {
                    let _ = tx.send(Cmd::Stop { announce: false });
                }
            }
        }
        if stopped_without_replacement {
            // The old service was stopped and nothing replaced it without a
            // game wait (e.g. the League gate blocked an active game). Its
            // final status is stale after the generation bump, so publish the
            // truth here instead of leaving the rail stuck on "recording".
            let _ = app.emit("status", stopped_status());
        }
        if waiting_for_game
            && waiting_generation
                .is_some_and(|generation| self.waiting_generation_is_current(generation))
        {
            emit_waiting_for_game(&app);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{RecorderDiagnosticStatus};
    use crate::settings::{GameRecordingMode};
    use std::path::Path;
    use std::sync::mpsc;

    #[test]
    fn durable_recorder_status_prefers_waiting_then_last_status() {
        let mut settings = AppSettings::default();
        settings.games.pause_when_no_game = true;
        let state = RuntimeState::new(settings, None);
        {
            let mut inner = state.0.lock().unwrap();
            inner.recording_desired = true;
            inner.last_recorder_status = Some(RecorderDiagnosticStatus {
                recording: true,
                waiting_for_game: false,
                segments: 3,
                buffered_s: 12.0,
                buffered_mb: 4.0,
                full_session: false,
                encoder: "mft-h264".into(),
                capture_backend: "wgc".into(),
            });
        }

        assert!(matches!(
            state.durable_recorder_status_for_replay(),
            Some(Event::Status {
                recording: false,
                waiting_for_game: true,
                ..
            })
        ));

        {
            let mut inner = state.0.lock().unwrap();
            inner.recording_desired = false;
        }

        assert!(matches!(
            state.durable_recorder_status_for_replay(),
            Some(Event::Status {
                recording: true,
                waiting_for_game: false,
                segments: 3,
                encoder,
                ..
            }) if encoder == "mft-h264"
        ));
    }

    #[test]
    fn a_command_is_only_sent_when_the_recorder_is_still_listening() {
        let (tx, rx) = mpsc::channel();
        let state = RuntimeState::with_sender(tx, AppSettings::default(), None);
        let pressed_at = Instant::now();

        assert!(state.send(Cmd::Bookmark { pressed_at }));
        assert!(matches!(rx.try_recv(), Ok(Cmd::Bookmark { .. })));

        // A recorder thread can exit before its sender is cleared; nothing
        // downstream of the bookmark can happen, so this must not claim success.
        drop(rx);
        assert!(!state.send(Cmd::Bookmark { pressed_at }));
    }

    #[test]
    fn quota_lock_blocks_save_commands_and_preserves_recording_intent_after_stop() {
        let (tx, rx) = mpsc::channel();
        let state = RuntimeState::with_sender(tx, AppSettings::default(), None);
        let generation = state.0.lock().unwrap().recording_generation;
        let event = Event::StorageQuotaFull {
            total_bytes: 100,
            quota_bytes: 100,
            required_bytes: 10,
        };
        assert!(state.accept_service_quota(generation, &event));

        assert!(!state.request_save());
        assert!(rx.try_recv().is_err());
        assert!(state.accept_service_status(generation, false));
        let inner = state.0.lock().unwrap();
        assert!(inner.tx.is_none());
        assert!(inner.recording_desired);
        assert!(inner.quota_blocked.is_some());
    }

    #[test]
    fn quota_lock_prevents_prepared_recorder_restarts() {
        let mut inner = RuntimeState::new(AppSettings::default(), None)
            .0
            .into_inner()
            .unwrap();
        inner.recording_desired = true;
        inner.quota_blocked = Some(Event::StorageQuotaFull {
            total_bytes: 100,
            quota_bytes: 100,
            required_bytes: 1,
        });

        let prepared = RuntimeState::prepare_service_restart(&mut inner).unwrap();

        assert!(prepared.replacement.is_none());
        assert!(!prepared.waiting_for_game);
    }

    #[test]
    fn stale_recorder_cannot_quota_lock_a_new_generation() {
        let (old_tx, _old_rx) = mpsc::channel();
        let state = RuntimeState::with_sender(old_tx, AppSettings::default(), None);
        let old_generation = state.0.lock().unwrap().recording_generation;
        let (new_tx, _new_rx) = mpsc::channel();
        {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::install_recording_sender(&mut inner, new_tx);
        }
        let event = Event::StorageQuotaFull {
            total_bytes: 100,
            quota_bytes: 100,
            required_bytes: 1,
        };

        assert!(!state.accept_service_quota(old_generation, &event));
        assert!(state.0.lock().unwrap().quota_blocked.is_none());
    }

    #[test]
    fn stopped_status_clears_matching_recording_sender() {
        let (tx, rx) = mpsc::channel();
        let state = RuntimeState::with_sender(tx, AppSettings::default(), None);
        let generation = {
            let mut inner = state.0.lock().unwrap();
            inner.last_save_request = Some(Instant::now());
            inner.recording_generation
        };

        assert!(state.accept_service_status(generation, false));

        let inner = state.0.lock().unwrap();
        assert!(inner.tx.is_none());
        assert!(inner.last_save_request.is_none());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn stale_stopped_status_does_not_clear_newer_recording_sender() {
        let (old_tx, _old_rx) = mpsc::channel();
        let state = RuntimeState::with_sender(old_tx, AppSettings::default(), None);
        let stale_generation = state.0.lock().unwrap().recording_generation;
        let (new_tx, new_rx) = mpsc::channel();
        {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::install_recording_sender(&mut inner, new_tx);
        }

        assert!(!state.accept_service_status(stale_generation, false));
        assert!(state.send(Cmd::Save));
        assert!(matches!(new_rx.try_recv(), Ok(Cmd::Save)));
    }

    #[test]
    fn stale_stopped_status_is_rejected_after_entering_waiting() {
        let (tx, _rx) = mpsc::channel();
        let mut settings = AppSettings::default();
        settings.games.pause_when_no_game = true;
        let state = RuntimeState::with_sender(tx, settings, None);
        let stale_generation = state.0.lock().unwrap().recording_generation;

        let waiting_generation = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::prepare_service_restart(&mut inner)
                .unwrap()
                .waiting_generation
                .unwrap()
        };

        assert!(!state.accept_service_status(stale_generation, false));
        assert!(state.waiting_generation_is_current(waiting_generation));
    }

    #[test]
    fn armed_waiting_status_is_available_for_frontend_replay() {
        let mut settings = AppSettings::default();
        settings.games.pause_when_no_game = true;
        let state = RuntimeState::new(settings, None);
        {
            let mut inner = state.0.lock().unwrap();
            inner.recording_desired = true;
        }

        assert!(matches!(
            state.current_waiting_status(),
            Some(Event::Status {
                recording: false,
                waiting_for_game: true,
                ..
            })
        ));
    }

    #[test]
    fn active_full_session_game_sets_service_recording_mode() {
        let inner = RuntimeInner {
            tx: None,
            recording_generation: 0,
            recording_desired: false,
            manual_full_session_desired: false,
            settings: AppSettings::default(),
            lol_url: None,
            active_game: Some(DetectedGame {
                identity: crate::game_identity::GameIdentity::custom(
                    crate::game_plugins::LEAGUE_OF_LEGENDS_ID,
                ),
                name: "Game".into(),
                hwnd: 42,
                window_title: "Game Window".into(),
                process_id: 7,
                exe_name: "game.exe".into(),
                exe_path: None,
                recording_mode: GameRecordingMode::FullSession,
            }),
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

        let opts = RuntimeState::options(&inner).unwrap();

        assert_eq!(
            opts.active_game
                .as_ref()
                .and_then(|game| game.identity.plugin_id()),
            None
        );
        assert_eq!(opts.recording_mode, service::RecordingMode::FullSession);
        assert_eq!(
            opts.capture_source,
            service::CaptureSource::WindowHandle {
                hwnd: 42,
                title: "Game Window".into(),
            }
        );
    }

    #[test]
    fn active_built_in_game_sets_service_plugin_id_for_event_sources() {
        let inner = RuntimeInner {
            tx: None,
            recording_generation: 0,
            recording_desired: false,
            manual_full_session_desired: false,
            settings: AppSettings::default(),
            lol_url: Some("http://mock".into()),
            active_game: Some(DetectedGame {
                identity: crate::game_identity::GameIdentity::built_in_plugin(
                    crate::game_plugins::LEAGUE_OF_LEGENDS_ID,
                )
                .unwrap(),
                name: "League of Legends".into(),
                hwnd: 42,
                window_title: "League".into(),
                process_id: 7,
                exe_name: "League of Legends.exe".into(),
                exe_path: Some(
                    r"C:\Riot Games\League of Legends\Game\League of Legends.exe".into(),
                ),
                recording_mode: GameRecordingMode::FullSession,
            }),
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

        let opts = RuntimeState::options(&inner).unwrap();

        assert_eq!(
            opts.active_game
                .as_ref()
                .and_then(|game| game.identity.plugin_id()),
            Some(crate::game_plugins::LEAGUE_OF_LEGENDS_ID)
        );
        assert_eq!(opts.lol_url.as_deref(), Some("http://mock"));
        assert_eq!(
            opts.active_game.as_ref().and_then(|game| game.exe_path.as_deref()),
            Some(Path::new(
                r"C:\Riot Games\League of Legends\Game\League of Legends.exe"
            ))
        );
    }
}
