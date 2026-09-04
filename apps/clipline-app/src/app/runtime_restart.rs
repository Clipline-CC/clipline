use std::sync::mpsc::Sender;

use tauri::{
    AppHandle, Emitter, Runtime,
};


use crate::service::{self, Cmd, ServiceOptions};
use crate::settings::AppSettings;
use super::*;

impl RuntimeState {
    pub(crate) fn prepare_settings_restart(
        &self,
        settings: AppSettings,
    ) -> Result<PreparedRuntimeRestart, String> {
        let inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
        let cleared_active_game = inner.active_game.is_some()
            && !active_game_still_configured(&settings, inner.active_game.as_ref());
        let active_game = if cleared_active_game {
            None
        } else {
            inner.active_game.as_ref()
        };
        if inner.recording_desired
            && inner.quota_blocked.is_none()
            && (inner.manual_full_session_desired || recorder_should_run(&settings, active_game))
        {
            Self::options_for(
                &settings,
                inner.lol_url.clone(),
                active_game,
                &inner.decodable_codecs,
            )?;
        }
        Ok(PreparedRuntimeRestart { settings })
    }

    pub(crate) fn commit_prepared_restart_with<T, F>(
        inner: &mut RuntimeInner,
        prepared: PreparedRuntimeRestart,
        spawn: F,
    ) -> Result<CommittedRuntimeRestart<T>, String>
    where
        F: FnOnce(ServiceOptions) -> (Sender<Cmd>, T),
    {
        let PreparedRuntimeRestart { settings } = prepared;
        let cleared_active_game = inner.active_game.is_some()
            && !active_game_still_configured(&settings, inner.active_game.as_ref());
        let active_game = if cleared_active_game {
            None
        } else {
            inner.active_game.as_ref()
        };
        let base_should_run = recorder_should_run(&settings, active_game);
        // A game dropped by the settings change takes its gate verdict with
        // it: the stale Denied/Pending must not keep blocking (it is also
        // cleared below, but the spawn decision happens first).
        let gate_allows = cleared_active_game || league_gate_allows(inner);
        let should_run = inner.recording_desired
            && inner.quota_blocked.is_none()
            && (inner.manual_full_session_desired
                || (base_should_run && gate_allows));
        let waiting_for_game = inner.recording_desired
            && inner.quota_blocked.is_none()
            && !inner.manual_full_session_desired
            && !base_should_run;
        let next_options = if should_run {
            let mut options = Self::options_for(
                &settings,
                inner.lol_url.clone(),
                active_game,
                &inner.decodable_codecs,
            )?;
            if inner.manual_full_session_desired {
                options.recording_mode = service::RecordingMode::FullSession;
            }
            options.recover_abandoned_recordings = false;
            Some(options)
        } else {
            None
        };

        inner.settings = settings;
        if cleared_active_game {
            inner.active_game = None;
            inner.league_gate = None;
            inner.league_gate_rx = None;
        }
        let old_tx = inner.tx.take();
        let replacement = if let Some(options) = next_options {
            let (tx, spawned) = spawn(options);
            let generation = Self::install_recording_sender(inner, tx);
            Some((spawned, generation))
        } else {
            None
        };
        if waiting_for_game {
            inner.recording_generation = inner.recording_generation.wrapping_add(1);
            inner.last_save_request = None;
        }
        let waiting_generation = waiting_for_game.then_some(inner.recording_generation);

        Ok(CommittedRuntimeRestart {
            old_tx,
            replacement,
            cleared_active_game,
            waiting_for_game,
            waiting_generation,
        })
    }

    pub(crate) fn finish_prepared_restart<R: Runtime>(
        &self,
        app: AppHandle<R>,
        prepared: PreparedRuntimeRestart,
    ) -> Result<(), String> {
        let CommittedRuntimeRestart {
            old_tx,
            replacement,
            cleared_active_game,
            waiting_for_game,
            waiting_generation,
        } = {
            let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
            Self::commit_prepared_restart_with(&mut inner, prepared, service::spawn)?
        };
        if let Some(tx) = old_tx {
            let _ = tx.send(Cmd::Stop { announce: false });
        }
        if let Some((rx, generation)) = replacement {
            pump_events(app.clone(), rx, generation);
        }
        if waiting_for_game
            && waiting_generation
                .is_some_and(|generation| self.waiting_generation_is_current(generation))
        {
            emit_waiting_for_game(&app);
        }
        if cleared_active_game {
            let _ = app.emit("game-detection", GameDetectionEvent::from_detected(None));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{CommittedRuntimeRestart, PreparedRuntimeRestart, RuntimeInner, RuntimeState, detected_built_in_game, detected_game, run_before_releasing_settings_save_lock};
    use crate::settings::{GameRecordingMode, ReplayStorageMode, ReplayStorageSettings};
    use crate::games::DetectedGame;
    use std::sync::mpsc;
    use std::sync::Mutex;
    use std::time::Instant;

    #[test]
    fn prepared_settings_restart_is_non_mutating_until_commit() {
        let (tx, rx) = mpsc::channel();
        let original = AppSettings::default();
        let state = RuntimeState::with_sender(tx, original.clone(), None);
        let mut changed = original.clone();
        changed.fps = 120;

        let prepared = state.prepare_settings_restart(changed).unwrap();

        assert_eq!(state.settings().fps, original.fps);
        assert!(state.send(Cmd::Save), "active sender must remain installed");
        assert!(matches!(rx.try_recv(), Ok(Cmd::Save)));
        assert_eq!(prepared.settings.fps, 120);

        drop(prepared); // Simulates a later tray-label or hook-registration failure.
        assert!(
            state.send(Cmd::Save),
            "dropping a plan must not stop recording"
        );
        assert!(matches!(rx.try_recv(), Ok(Cmd::Save)));
    }

    #[test]
    fn settings_save_lock_remains_held_through_runtime_commit() {
        let save_lock = Mutex::new(());
        let save_guard = save_lock.lock().unwrap();
        let original = AppSettings::default();
        let state = RuntimeState::new(original.clone(), None);
        let changed = AppSettings {
            fps: 120,
            ..original
        };
        let prepared = state.prepare_settings_restart(changed).unwrap();

        run_before_releasing_settings_save_lock(save_guard, || {
            let committed: CommittedRuntimeRestart<()> = {
                let mut inner = state.0.lock().unwrap();
                RuntimeState::commit_prepared_restart_with(&mut inner, prepared, |_| {
                    unreachable!("inactive runtime must not spawn a replacement")
                })
                .unwrap()
            };

            assert!(committed.old_tx.is_none());
            assert_eq!(state.settings().fps, 120);
            assert!(
                matches!(
                    save_lock.try_lock(),
                    Err(std::sync::TryLockError::WouldBlock)
                ),
                "settings save lock was released before the runtime commit completed"
            );
            Ok(())
        })
        .unwrap();

        assert!(save_lock.try_lock().is_ok());
    }

    #[test]
    fn game_restart_pauses_service_but_keeps_recorder_armed() {
        let (tx, _rx) = mpsc::channel();
        let mut settings = AppSettings::default();
        settings.games.pause_when_no_game = true;
        let mut inner = RuntimeInner {
            tx: Some(tx),
            recording_generation: 1,
            recording_desired: true,
            manual_full_session_desired: false,
            settings,
            lol_url: None,
            active_game: None,
            osu_title_events: Vec::new(),
            last_save_request: Some(Instant::now()),
            decodable_codecs: vec![service::Codec::H264],
            last_recorder_status: None,
            last_storage_status: None,
            recent_recorder_error: false,
            quota_blocked: None,
            league_gate: None,
            league_gate_rx: None,
        };

        let prepared = RuntimeState::prepare_service_restart(&mut inner).unwrap();

        assert!(prepared.old_tx.is_some());
        assert!(prepared.replacement.is_none());
        assert!(prepared.waiting_for_game);
        assert!(inner.recording_desired);
        assert!(inner.tx.is_none());
    }

    #[test]
    fn game_restart_resumes_an_armed_policy_pause() {
        let mut settings = AppSettings::default();
        settings.games.pause_when_no_game = true;
        let mut inner = RuntimeInner {
            tx: None,
            recording_generation: 4,
            recording_desired: true,
            manual_full_session_desired: false,
            settings,
            lol_url: None,
            active_game: Some(detected_game("custom-game", "Game", 42)),
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

        let prepared = RuntimeState::prepare_service_restart(&mut inner).unwrap();

        assert!(prepared.old_tx.is_none());
        assert!(prepared.replacement.is_some());
        assert!(!prepared.waiting_for_game);
        assert!(inner.recording_desired);
    }

    #[test]
    fn manual_stop_invalidates_a_pending_waiting_status() {
        let mut settings = AppSettings::default();
        settings.games.pause_when_no_game = true;
        let state = RuntimeState::new(settings, None);
        let waiting_generation = {
            let mut inner = state.0.lock().unwrap();
            inner.recording_desired = true;
            RuntimeState::prepare_service_restart(&mut inner)
                .unwrap()
                .waiting_generation
                .expect("policy pause must carry its generation")
        };

        state.stop_recording().unwrap();

        assert!(!state.waiting_generation_is_current(waiting_generation));
    }

    #[test]
    fn enabling_games_only_policy_stops_fallback_capture_at_commit() {
        let (tx, _rx) = mpsc::channel();
        let state = RuntimeState::with_sender(tx, AppSettings::default(), None);
        let mut changed = AppSettings::default();
        changed.games.pause_when_no_game = true;
        let prepared = state.prepare_settings_restart(changed).unwrap();

        let mut spawned = false;
        let committed = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::commit_prepared_restart_with(&mut inner, prepared, |_| {
                spawned = true;
                let (replacement_tx, _replacement_rx) = mpsc::channel();
                (replacement_tx, ())
            })
            .unwrap()
        };

        assert!(!spawned);
        assert!(committed.old_tx.is_some());
        assert!(committed.replacement.is_none());
        assert!(committed.waiting_for_game);
        assert!(state.0.lock().unwrap().recording_desired);
    }

    #[test]
    fn committed_waiting_invalidates_detector_restart_already_spawning() {
        let (initial_tx, _initial_rx) = mpsc::channel();
        let state = RuntimeState::with_sender(initial_tx, AppSettings::default(), None);
        let detector_restart = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::prepare_service_restart(&mut inner).unwrap()
        };
        let (_options, detector_generation) = detector_restart.replacement.unwrap();

        let mut waiting_settings = AppSettings::default();
        waiting_settings.games.pause_when_no_game = true;
        let prepared = state.prepare_settings_restart(waiting_settings).unwrap();
        let committed: CommittedRuntimeRestart<()> = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::commit_prepared_restart_with(&mut inner, prepared, |_| {
                unreachable!("waiting must not spawn a recorder")
            })
            .unwrap()
        };
        assert!(committed.waiting_for_game);
        assert!(committed.old_tx.is_none());

        let (stale_tx, stale_rx) = mpsc::channel();
        let rejected = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::install_prepared_service_restart(
                &mut inner,
                detector_generation,
                stale_tx,
            )
            .unwrap_err()
        };
        rejected.send(Cmd::Stop { announce: false }).unwrap();

        assert!(matches!(
            stale_rx.try_recv(),
            Ok(Cmd::Stop { announce: false })
        ));
        assert!(!state.send(Cmd::Save));
    }

    #[test]
    fn prepared_settings_restart_uses_current_game_and_sender_at_commit() {
        let (initial_tx, _initial_rx) = mpsc::channel();
        let state = RuntimeState::with_sender(initial_tx, AppSettings::default(), None);
        {
            state.0.lock().unwrap().active_game = Some(detected_built_in_game(
                crate::game_plugins::LEAGUE_OF_LEGENDS_ID,
                "League",
                41,
            ));
        }
        let changed = AppSettings {
            fps: 120,
            ..AppSettings::default()
        };
        let prepared = state.prepare_settings_restart(changed).unwrap();

        let (newer_tx, newer_rx) = mpsc::channel();
        let (replacement_tx, replacement_rx) = mpsc::channel();
        let mut committed_options = None;
        let committed = {
            let mut inner = state.0.lock().unwrap();
            inner.active_game = Some(detected_built_in_game(
                crate::game_plugins::OSU_ID,
                "osu!",
                84,
            ));
            RuntimeState::install_recording_sender(&mut inner, newer_tx);
            RuntimeState::commit_prepared_restart_with(&mut inner, prepared, |options| {
                committed_options = Some(options);
                (replacement_tx, ())
            })
            .unwrap()
        };

        let options = committed_options.unwrap();
        assert_eq!(options.fps, 120);
        assert_eq!(
            options.capture_source,
            service::CaptureSource::WindowHandle {
                hwnd: 84,
                title: "osu! Window".into(),
            }
        );
        assert_eq!(
            options.active_game.as_ref().map(|game| game.identity.id()),
            Some(crate::game_plugins::OSU_ID)
        );
        committed.old_tx.unwrap().send(Cmd::Save).unwrap();
        assert!(matches!(newer_rx.try_recv(), Ok(Cmd::Save)));
        assert!(committed.replacement.is_some());
        assert!(state.send(Cmd::Save));
        assert!(matches!(replacement_rx.try_recv(), Ok(Cmd::Save)));
    }

    #[test]
    fn prepared_settings_restart_restarts_sender_that_started_before_commit() {
        let state = RuntimeState::new(AppSettings::default(), None);
        let changed = AppSettings {
            fps: 120,
            ..AppSettings::default()
        };
        let prepared = state.prepare_settings_restart(changed).unwrap();

        let (started_tx, started_rx) = mpsc::channel();
        let (replacement_tx, replacement_rx) = mpsc::channel();
        let mut committed_options = None;
        let committed = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::install_recording_sender(&mut inner, started_tx);
            RuntimeState::commit_prepared_restart_with(&mut inner, prepared, |options| {
                committed_options = Some(options);
                (replacement_tx, ())
            })
            .unwrap()
        };

        assert_eq!(committed_options.unwrap().fps, 120);
        committed.old_tx.unwrap().send(Cmd::Save).unwrap();
        assert!(matches!(started_rx.try_recv(), Ok(Cmd::Save)));
        assert!(committed.replacement.is_some());
        assert!(state.send(Cmd::Save));
        assert!(matches!(replacement_rx.try_recv(), Ok(Cmd::Save)));
    }

    #[test]
    fn prepared_settings_restart_does_not_resurrect_sender_stopped_before_commit() {
        let (tx, _rx) = mpsc::channel();
        let state = RuntimeState::with_sender(tx, AppSettings::default(), None);
        let changed = AppSettings {
            fps: 120,
            ..AppSettings::default()
        };
        let prepared = state.prepare_settings_restart(changed).unwrap();

        state.stop_recording().unwrap();

        let mut spawned = false;
        let committed = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::commit_prepared_restart_with(&mut inner, prepared, |_| {
                spawned = true;
                let (replacement_tx, _replacement_rx) = mpsc::channel();
                (replacement_tx, ())
            })
            .unwrap()
        };

        assert!(!spawned);
        assert!(committed.old_tx.is_none());
        assert!(committed.replacement.is_none());
        assert!(!state.send(Cmd::Save));
        assert_eq!(state.settings().fps, 120);
    }

    #[test]
    fn commit_time_restart_option_error_keeps_current_sender_and_settings() {
        let (tx, rx) = mpsc::channel();
        let original = AppSettings::default();
        let state = RuntimeState::with_sender(tx, original.clone(), None);
        let prepared = PreparedRuntimeRestart {
            settings: invalid_disk_replay_settings(),
        };

        let mut spawned = false;
        let error = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::commit_prepared_restart_with(&mut inner, prepared, |_| {
                spawned = true;
                let (replacement_tx, _replacement_rx) = mpsc::channel();
                (replacement_tx, ())
            })
            .unwrap_err()
        };

        assert!(error.contains("replay cache folder"), "{error}");
        assert!(!spawned);
        assert_eq!(state.settings().replay_storage, original.replay_storage);
        assert!(state.send(Cmd::Save));
        assert!(matches!(rx.try_recv(), Ok(Cmd::Save)));
    }

    #[test]
    fn recording_sender_survives_restart_option_error() {
        let (tx, _rx) = mpsc::channel();
        let mut inner = RuntimeInner {
            tx: Some(tx),
            recording_generation: 1,
            recording_desired: true,
            manual_full_session_desired: false,
            settings: invalid_disk_replay_settings(),
            lol_url: None,
            active_game: None,
            osu_title_events: Vec::new(),
            last_save_request: Some(Instant::now()),
            decodable_codecs: vec![service::Codec::H264],
            last_recorder_status: None,
            last_storage_status: None,
            recent_recorder_error: false,
            quota_blocked: None,
            league_gate: None,
            league_gate_rx: None,
        };

        let err = match RuntimeState::prepare_service_restart(&mut inner) {
            Ok(_) => panic!("restart options should fail"),
            Err(err) => err,
        };

        assert!(err.contains("replay cache folder"), "{err}");
        assert!(inner.tx.is_some(), "failed options must not drop sender");
        assert!(inner.recording_desired);
        assert_eq!(inner.recording_generation, 1);
        assert!(
            inner.last_save_request.is_some(),
            "failed options must not clear debounce state"
        );
    }

    #[test]
    fn prepared_restart_skips_abandoned_recording_recovery() {
        let (tx, _rx) = mpsc::channel();
        let mut inner = RuntimeInner {
            tx: Some(tx),
            recording_generation: 1,
            recording_desired: true,
            manual_full_session_desired: false,
            settings: AppSettings::default(),
            lol_url: None,
            active_game: None,
            osu_title_events: Vec::new(),
            last_save_request: Some(Instant::now()),
            decodable_codecs: vec![service::Codec::H264],
            last_recorder_status: None,
            last_storage_status: None,
            recent_recorder_error: false,
            quota_blocked: None,
            league_gate: None,
            league_gate_rx: None,
        };

        let prepared = RuntimeState::prepare_service_restart(&mut inner).unwrap();
        let (next_options, _generation) = prepared.replacement.unwrap();

        assert!(
            !next_options.recover_abandoned_recordings,
            "internal recorder restarts must not recover another active recorder's temp file"
        );
    }

    #[test]
    fn game_restart_gap_does_not_resurrect_after_user_stop() {
        let (initial_tx, _initial_rx) = mpsc::channel();
        let state = RuntimeState::with_sender(initial_tx, AppSettings::default(), None);
        let prepared = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::prepare_service_restart(&mut inner).unwrap()
        };
        let (_options, restart_generation) = prepared.replacement.unwrap();

        state.stop_recording().unwrap();
        let (replacement_tx, replacement_rx) = mpsc::channel();
        let rejected = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::install_prepared_service_restart(
                &mut inner,
                restart_generation,
                replacement_tx,
            )
            .unwrap_err()
        };
        rejected.send(Cmd::Stop { announce: false }).unwrap();

        assert!(
            !state.send(Cmd::Save),
            "a replacement spawned before Stop must not resurrect recording"
        );
        assert!(matches!(
            replacement_rx.try_recv(),
            Ok(Cmd::Stop { announce: false })
        ));
    }

    #[test]
    fn game_restart_gap_does_not_overwrite_a_newer_manual_start() {
        let (initial_tx, _initial_rx) = mpsc::channel();
        let state = RuntimeState::with_sender(initial_tx, AppSettings::default(), None);
        let prepared = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::prepare_service_restart(&mut inner).unwrap()
        };
        let (_options, restart_generation) = prepared.replacement.unwrap();

        let (newer_tx, newer_rx) = mpsc::channel();
        let (stale_tx, stale_rx) = mpsc::channel();
        let rejected = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::install_recording_sender(&mut inner, newer_tx);
            RuntimeState::install_prepared_service_restart(&mut inner, restart_generation, stale_tx)
                .unwrap_err()
        };
        rejected.send(Cmd::Stop { announce: false }).unwrap();

        assert!(state.send(Cmd::Save));
        assert!(
            matches!(newer_rx.try_recv(), Ok(Cmd::Save)),
            "the manual start must remain the active sender"
        );
        assert!(matches!(
            stale_rx.try_recv(),
            Ok(Cmd::Stop { announce: false })
        ));
    }

    #[test]
    fn newer_game_restart_supersedes_a_restart_already_spawning() {
        let (initial_tx, _initial_rx) = mpsc::channel();
        let state = RuntimeState::with_sender(initial_tx, AppSettings::default(), None);
        let first = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::prepare_service_restart(&mut inner).unwrap()
        };
        let (_first_options, first_generation) = first.replacement.unwrap();

        let second = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::prepare_service_restart(&mut inner).unwrap()
        };
        let (_second_options, second_generation) = second.replacement.unwrap();

        let (first_tx, first_rx) = mpsc::channel();
        let (second_tx, second_rx) = mpsc::channel();
        let first_rejected = {
            let mut inner = state.0.lock().unwrap();
            let rejected = RuntimeState::install_prepared_service_restart(
                &mut inner,
                first_generation,
                first_tx,
            )
            .unwrap_err();
            RuntimeState::install_prepared_service_restart(
                &mut inner,
                second_generation,
                second_tx,
            )
            .unwrap();
            rejected
        };
        first_rejected.send(Cmd::Stop { announce: false }).unwrap();
        assert!(matches!(
            first_rx.try_recv(),
            Ok(Cmd::Stop { announce: false })
        ));
        assert!(state.send(Cmd::Save));
        assert!(matches!(second_rx.try_recv(), Ok(Cmd::Save)));
    }

    #[test]
    fn recording_sender_survives_game_restart_option_error() {
        let (tx, _rx) = mpsc::channel();
        let mut inner = RuntimeInner {
            tx: Some(tx),
            recording_generation: 1,
            recording_desired: true,
            manual_full_session_desired: false,
            settings: invalid_disk_replay_settings(),
            lol_url: None,
            active_game: Some(DetectedGame {
                identity: crate::game_identity::GameIdentity::custom("custom-game"),
                name: "Game".into(),
                hwnd: 42,
                window_title: "Game".into(),
                process_id: 7,
                exe_name: "game.exe".into(),
                exe_path: None,
                recording_mode: GameRecordingMode::FullSession,
            }),
            osu_title_events: Vec::new(),
            last_save_request: Some(Instant::now()),
            decodable_codecs: vec![service::Codec::H264],
            last_recorder_status: None,
            last_storage_status: None,
            recent_recorder_error: false,
            quota_blocked: None,
            league_gate: None,
            league_gate_rx: None,
        };

        let err = match RuntimeState::prepare_service_restart(&mut inner) {
            Ok(_) => panic!("restart options should fail"),
            Err(err) => err,
        };

        assert!(err.contains("replay cache folder"), "{err}");
        assert!(inner.tx.is_some(), "failed options must not drop sender");
        assert!(
            inner.last_save_request.is_some(),
            "failed options must not clear debounce state"
        );
    }

    #[test]
    fn failed_newer_game_restart_invalidates_a_plan_already_spawning() {
        let mut inner = RuntimeInner {
            tx: None,
            recording_generation: 7,
            recording_desired: true,
            manual_full_session_desired: false,
            settings: invalid_disk_replay_settings(),
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

        assert!(RuntimeState::prepare_service_restart(&mut inner).is_err());
        assert_eq!(inner.recording_generation, 8);
        assert!(inner.recording_desired);
        assert!(inner.tx.is_none());
    }

    fn invalid_disk_replay_settings() -> AppSettings {
        AppSettings {
            replay_storage: ReplayStorageSettings {
                mode: ReplayStorageMode::Disk,
                disk_dir: String::new(),
                disk_quota_gb: 2.0,
                disk_acknowledged: true,
            },
            ..AppSettings::default()
        }
    }
}
