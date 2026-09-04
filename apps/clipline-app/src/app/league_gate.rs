use std::path::Path;
use std::sync::mpsc::{self, Receiver, TryRecvError};

use tauri::{
    AppHandle, Emitter, Runtime,
};

use clipline_lol::LeagueQueue;

use crate::games::DetectedGame;
use crate::settings::AppSettings;
use crate::util::unix_now_i64;
use super::*;

/// Whether the League game-type gate blocks automatic recording right now.
pub(crate) fn league_gate_allows(inner: &RuntimeInner) -> bool {
    !matches!(
        inner.league_gate,
        Some(LeagueGateVerdict::Pending) | Some(LeagueGateVerdict::Denied)
    )
}

/// Whether the gate applies to this detected game: a League game with at
/// least one category switched off.
pub(crate) fn league_gate_applies(settings: &AppSettings, game: Option<&DetectedGame>) -> bool {
    settings.league.has_gate()
        && game.is_some_and(|game| {
            game.identity.id() == crate::game_identity::LEAGUE_OF_LEGENDS_ID
        })
}

/// Combined automatic-start predicate: game policy plus the gate. Manual
/// sessions bypass both.
pub(crate) fn automatic_start_allowed(inner: &RuntimeInner, settings: &AppSettings) -> bool {
    recorder_should_run(settings, inner.active_game.as_ref()) && league_gate_allows(inner)
}

/// Production gate lookup: one LCU request on a dedicated thread. Any failure
/// (missing lockfile, timeout, client error) resolves to `None` — the unknown
/// tag — so the gate can apply `record_unknown` instead of blocking forever.
pub(crate) fn spawn_gate_lookup(game: &DetectedGame) -> Receiver<Option<LeagueQueue>> {
    let (tx, rx) = mpsc::channel();
    let exe_path = game.exe_path.clone();
    let process_id = game.process_id;
    std::thread::Builder::new()
        .name("clipline-lol-gate".into())
        .spawn(move || {
            let queue = gate_queue_for_game(exe_path.as_deref(), process_id);
            let _ = tx.send(queue);
        })
        .expect("spawn league gate lookup thread");
    rx
}

pub(crate) fn gate_queue_for_game(exe_path: Option<&str>, process_id: u32) -> Option<LeagueQueue> {
    if let Some(command_line) = crate::windows::process_command_line(process_id) {
        if clipline_lol::is_league_replay_command_line(&command_line) {
            return Some(LeagueQueue::replay());
        }
    }
    gate_queue_for_exe(exe_path)
}

pub(crate) fn gate_queue_for_exe(exe_path: Option<&str>) -> Option<LeagueQueue> {
    let exe_path = exe_path?;
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => return None,
    };
    runtime.block_on(async move {
        let client = clipline_lol::LcuClient::from_game_executable(Path::new(exe_path)).ok()?;
        client.current_queue().await.ok()
    })
}

pub(crate) fn same_game_window(current: Option<&DetectedGame>, next: Option<&DetectedGame>) -> bool {    match (current, next) {
        (Some(current), Some(next)) => {
            current.identity == next.identity && current.hwnd == next.hwnd
        }
        (None, None) => true,
        _ => false,
    }
}

impl RuntimeState {
    pub(crate) fn plan_detection_transition(
        inner: &mut RuntimeInner,
        detected: Option<DetectedGame>,
        league_lookup: Option<LeagueGateLookup>,
    ) -> Result<(Option<PreparedServiceRestart>, bool, GameDetectionEvent), String> {
        let detected =
            detected.filter(|game| active_game_still_configured(&inner.settings, Some(game)));
        let event = GameDetectionEvent::from_detected(detected.as_ref());
        record_osu_title_event(inner, detected.as_ref(), unix_now_i64());
        if same_game_window(inner.active_game.as_ref(), detected.as_ref()) {
            if game_recording_mode_changed(inner.active_game.as_ref(), detected.as_ref()) {
                inner.active_game = detected;
                Ok((
                    Some(Self::prepare_service_restart(inner)?),
                    true,
                    event,
                ))
            } else if inner.active_game != detected {
                inner.active_game = detected;
                Ok((None, true, event))
            } else {
                Ok((None, false, event))
            }
        } else {
            inner.active_game = detected;
            if league_gate_applies(&inner.settings, inner.active_game.as_ref()) {
                // The factory comes from a settings snapshot taken before the
                // runtime lock; settings may have changed in between. Without a
                // factory, fall back to today's behavior instead of panicking
                // the only game-detector thread.
                if let Some(lookup) = league_lookup {
                    inner.league_gate = Some(LeagueGateVerdict::Pending);
                    inner.league_gate_rx = Some(lookup(
                        inner
                            .active_game
                            .as_ref()
                            .expect("gated detection always has a game"),
                    ));
                } else {
                    inner.league_gate = None;
                    inner.league_gate_rx = None;
                }
            } else {
                inner.league_gate = None;
                inner.league_gate_rx = None;
            }
            Ok((
                Some(Self::prepare_service_restart(inner)?),
                true,
                event,
            ))
        }
    }

    /// Drain a resolved gate lookup once and compute the verdict from the
    /// **current** settings. `Allowed` returns the restart that spawns the
    /// deferred recorder; `Denied` returns without one so the caller can
    /// notify. An unresolved lookup leaves the gate `Pending`.
    pub(crate) fn resolve_league_gate(
        inner: &mut RuntimeInner,
    ) -> Result<Option<LeagueGateResolution>, String> {
        let Some(rx) = inner.league_gate_rx.take() else {
            return Ok(None);
        };
        let queue = match rx.try_recv() {
            Ok(queue) => queue,
            Err(TryRecvError::Empty) => {
                inner.league_gate_rx = Some(rx);
                return Ok(None);
            }
            // Resolver vanished without a verdict: treat as unknown.
            Err(TryRecvError::Disconnected) => None,
        };
        if inner
            .settings
            .league
            .allows(queue.as_ref().map(|queue| &queue.category))
        {
            inner.league_gate = Some(LeagueGateVerdict::Allowed);
            // A manual session that started while the lookup was pending must
            // keep running: it bypasses the gate, so restarting it here would
            // split the recording.
            let restart = if inner.manual_full_session_desired {
                None
            } else {
                Some(Box::new(Self::prepare_service_restart(inner)?))
            };
            Ok(Some(LeagueGateResolution::Allowed(restart)))
        } else {
            inner.league_gate = Some(LeagueGateVerdict::Denied);
            Ok(Some(LeagueGateResolution::Denied))
        }
    }

    /// App-level gate tick: resolves a pending lookup and either starts the
    /// deferred recorder or notifies that the game type is skipped.
    pub(crate) fn tick_league_gate<R: Runtime>(&self, app: AppHandle<R>) -> Result<(), String> {
        let resolution = {
            let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
            Self::resolve_league_gate(&mut inner)?
        };
        match resolution {
            Some(LeagueGateResolution::Allowed(restart)) => {
                if let Some(prepared) = restart {
                    self.finish_service_restart(app, *prepared)?;
                }
                Ok(())
            }
            Some(LeagueGateResolution::Denied) => {
                let _ = app.emit("error", LEAGUE_GATE_SKIP_NOTICE.to_string());
                Ok(())
            }
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{CommittedRuntimeRestart, LeagueGateLookup, LeagueGateResolution, LeagueGateVerdict, RuntimeState, detected_built_in_game, detected_game};
    use crate::games::DetectedGame;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{Sender, TryRecvError};
    use std::sync::mpsc;
    use std::time::Duration;
    use crate::service::Cmd;

    fn league_game(hwnd: isize) -> DetectedGame {
        detected_built_in_game(
            crate::game_identity::LEAGUE_OF_LEGENDS_ID,
            "League",
            hwnd,
        )
    }

    fn gated_settings(record_normal: bool) -> AppSettings {
        let mut settings = AppSettings::default();
        settings.league.record_normal = record_normal;
        settings
    }

    /// A lookup factory the test controls: the returned sender resolves the
    /// verdict on demand, and the flag records whether the runtime kicked it.
    fn held_lookup(
    ) -> (
        Sender<Option<LeagueQueue>>,
        LeagueGateLookup,
        std::sync::Arc<AtomicBool>,
    ) {
        let (tx, rx) = mpsc::channel();
        let called = std::sync::Arc::new(AtomicBool::new(false));
        let flag = called.clone();
        let factory = Box::new(move |_: &DetectedGame| {
            flag.store(true, Ordering::SeqCst);
            rx
        }) as LeagueGateLookup;
        (tx, factory, called)
    }

    #[test]
    fn league_gate_pending_tears_down_old_sender_and_defers_spawn() {
        let (old_tx, old_rx) = mpsc::channel();
        let state = RuntimeState::with_sender(old_tx, gated_settings(false), None);
        {
            let mut inner = state.0.lock().unwrap();
            inner.active_game = Some(detected_game("other", "Game", 7));
        }

        let (gate_tx, lookup, called) = held_lookup();
        let (prepared, emit, _) = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::plan_detection_transition(
                &mut inner,
                Some(league_game(41)),
                Some(lookup),
            )
            .unwrap()
        };

        assert!(called.load(Ordering::SeqCst), "gate lookup must be kicked");
        assert!(emit);
        let prepared = prepared.expect("detection always prepares a restart");
        assert!(
            prepared.old_tx.is_some(),
            "old sender must be torn down immediately, not after the verdict"
        );
        assert!(
            prepared.replacement.is_none(),
            "replacement spawn must wait for the gate verdict"
        );
        {
            let inner = state.0.lock().unwrap();
            assert_eq!(inner.league_gate, Some(LeagueGateVerdict::Pending));
            assert_eq!(inner.active_game.as_ref().map(|game| game.hwnd), Some(41));
        }

        gate_tx.send(Some(LeagueQueue::from_id(420))).unwrap();
        let resolution = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::resolve_league_gate(&mut inner).unwrap()
        };
        match resolution.expect("resolved verdict") {
            LeagueGateResolution::Allowed(Some(prepared)) => {
                assert!(prepared.replacement.is_some());
                assert!(prepared.old_tx.is_none());
            }
            LeagueGateResolution::Allowed(None) => panic!("no manual session was active"),
            LeagueGateResolution::Denied => panic!("ranked solo/duo must be allowed"),
        }
        assert!(matches!(old_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn league_gate_denied_resolves_without_replacement() {
        let state = RuntimeState::new(gated_settings(false), None);
        let (gate_tx, lookup, _) = held_lookup();
        {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::plan_detection_transition(
                &mut inner,
                Some(league_game(41)),
                Some(lookup),
            )
            .unwrap();
        }

        gate_tx.send(Some(LeagueQueue::from_id(430))).unwrap();
        let resolution = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::resolve_league_gate(&mut inner).unwrap()
        };
        assert!(
            matches!(resolution, Some(LeagueGateResolution::Denied)),
            "normal games must be denied when record_normal is off"
        );
        {
            let inner = state.0.lock().unwrap();
            assert_eq!(inner.league_gate, Some(LeagueGateVerdict::Denied));
            assert!(inner.league_gate_rx.is_none());
        }
    }

    #[test]
    fn league_gate_lookup_failure_follows_unknown_policy() {
        for record_unknown in [false, true] {
            let mut settings = gated_settings(false);
            settings.league.record_unknown = record_unknown;
            let state = RuntimeState::new(settings, None);
            let (gate_tx, lookup, _) = held_lookup();
            {
                let mut inner = state.0.lock().unwrap();
                RuntimeState::plan_detection_transition(
                    &mut inner,
                    Some(league_game(41)),
                    Some(lookup),
                )
                .unwrap();
            }

            gate_tx.send(None).unwrap();
            let resolution = {
                let mut inner = state.0.lock().unwrap();
                RuntimeState::resolve_league_gate(&mut inner).unwrap()
            };
            if record_unknown {
                assert!(matches!(resolution, Some(LeagueGateResolution::Allowed(_))));
            } else {
                assert!(matches!(resolution, Some(LeagueGateResolution::Denied)));
            }
        }
    }

    #[test]
    fn settings_save_while_pending_or_denied_does_not_spawn_recorder() {
        for verdict in [
            LeagueGateVerdict::Pending,
            LeagueGateVerdict::Denied,
        ] {
            let state = RuntimeState::new(gated_settings(false), None);
            if verdict == LeagueGateVerdict::Pending {
                let (_, lookup, _) = held_lookup();
                let mut inner = state.0.lock().unwrap();
                RuntimeState::plan_detection_transition(
                    &mut inner,
                    Some(league_game(41)),
                    Some(lookup),
                )
                .unwrap();
            } else {
                let (gate_tx, lookup, _) = held_lookup();
                {
                    let mut inner = state.0.lock().unwrap();
                    RuntimeState::plan_detection_transition(
                        &mut inner,
                        Some(league_game(41)),
                        Some(lookup),
                    )
                    .unwrap();
                }
                gate_tx.send(Some(LeagueQueue::from_id(430))).unwrap();
                let mut inner = state.0.lock().unwrap();
                RuntimeState::resolve_league_gate(&mut inner).unwrap();
            }

            let changed = AppSettings {
                fps: 120,
                ..AppSettings::default()
            };
            let prepared = state.prepare_settings_restart(changed).unwrap();
            let committed: CommittedRuntimeRestart<()> = {
                let mut inner = state.0.lock().unwrap();
                RuntimeState::commit_prepared_restart_with(&mut inner, prepared, |_| {
                    panic!("a {verdict:?} gate must not spawn a recorder on settings save")
                })
                .unwrap()
            };
            assert!(committed.replacement.is_none());
            assert!(!committed.waiting_for_game);
            {
                let inner = state.0.lock().unwrap();
                assert_eq!(inner.league_gate, Some(verdict));
            }
        }
    }

    #[test]
    fn league_gate_toggle_mid_lookup_is_honored_at_resolution() {
        let state = RuntimeState::new(gated_settings(false), None);
        let (gate_tx, lookup, _) = held_lookup();
        {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::plan_detection_transition(
                &mut inner,
                Some(league_game(41)),
                Some(lookup),
            )
            .unwrap();
            inner.settings.league.record_normal = true;
        }

        gate_tx.send(Some(LeagueQueue::from_id(400))).unwrap();
        let resolution = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::resolve_league_gate(&mut inner).unwrap()
        };
        assert!(
            matches!(resolution, Some(LeagueGateResolution::Allowed(_))),
            "the verdict must use the settings current at resolution time"
        );
    }

    #[test]
    fn manual_session_start_bypasses_pending_gate() {
        let (tx, _rx) = mpsc::channel();
        let state = RuntimeState::with_sender(tx, gated_settings(false), None);
        let (_, lookup, _) = held_lookup();
        {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::plan_detection_transition(
                &mut inner,
                Some(league_game(41)),
                Some(lookup),
            )
            .unwrap();
            inner.manual_full_session_desired = true;
            let prepared = RuntimeState::prepare_service_restart(&mut inner).unwrap();
            assert!(
                prepared.replacement.is_some(),
                "manual recording must bypass the pending gate"
            );
        }
    }

    #[test]
    fn manual_session_stop_stays_stopped_with_denied_gate() {
        let (manual_tx, _manual_rx) = mpsc::channel();
        let state = RuntimeState::with_sender(manual_tx, gated_settings(false), None);
        {
            let mut inner = state.0.lock().unwrap();
            inner.active_game = Some(league_game(41));
            inner.league_gate = Some(LeagueGateVerdict::Denied);
            inner.manual_full_session_desired = true;
            let (tx, restart) = RuntimeState::prepare_manual_session_stop(&mut inner).unwrap();
            let tx = tx.expect("manual sender must be stopped");
            tx.send(Cmd::Stop { announce: false }).unwrap();
            let restart = restart.expect("gate-denied stop restarts into nothing");
            assert!(restart.replacement.is_none());
        }
    }

    #[test]
    fn non_league_game_never_consults_the_gate() {
        let (tx, _rx) = mpsc::channel();
        let state = RuntimeState::with_sender(tx, gated_settings(false), None);
        let (_, lookup, called) = held_lookup();
        let (prepared, _, _) = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::plan_detection_transition(
                &mut inner,
                Some(detected_built_in_game(crate::game_identity::OSU_ID, "osu!", 84)),
                Some(lookup),
            )
            .unwrap()
        };

        assert!(!called.load(Ordering::SeqCst));
        let prepared = prepared.expect("non-League detection restarts immediately");
        assert!(prepared.replacement.is_some());
        {
            let inner = state.0.lock().unwrap();
            assert_eq!(inner.league_gate, None);
        }
    }

    #[test]
    fn all_record_settings_start_immediately_without_lookup() {
        let (tx, _rx) = mpsc::channel();
        let state = RuntimeState::with_sender(tx, AppSettings::default(), None);
        let (_, lookup, called) = held_lookup();
        let (prepared, _, _) = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::plan_detection_transition(
                &mut inner,
                Some(league_game(41)),
                Some(lookup),
            )
            .unwrap()
        };

        assert!(!called.load(Ordering::SeqCst));
        assert!(prepared.unwrap().replacement.is_some());
        {
            let inner = state.0.lock().unwrap();
            assert_eq!(inner.league_gate, None);
        }
    }

    #[test]
    fn same_game_redetection_does_not_rekick_the_lookup() {
        let state = RuntimeState::new(gated_settings(false), None);
        let (gate_tx, lookup, called) = held_lookup();
        {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::plan_detection_transition(
                &mut inner,
                Some(league_game(41)),
                Some(lookup),
            )
            .unwrap();
        }
        assert!(called.load(Ordering::SeqCst));

        let (_, second_lookup, called_again) = held_lookup();
        let (prepared, emit, _) = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::plan_detection_transition(
                &mut inner,
                Some(league_game(41)),
                Some(second_lookup),
            )
            .unwrap()
        };
        assert!(!called_again.load(Ordering::SeqCst));
        assert!(prepared.is_none());
        assert!(!emit);
        {
            let inner = state.0.lock().unwrap();
            assert_eq!(inner.league_gate, Some(LeagueGateVerdict::Pending));
            assert!(
                inner.league_gate_rx.is_some(),
                "the pending verdict must stay resolvable"
            );
        }

        gate_tx.send(Some(LeagueQueue::from_id(430))).unwrap();
        let resolution = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::resolve_league_gate(&mut inner).unwrap()
        };
        assert!(matches!(resolution, Some(LeagueGateResolution::Denied)));
    }

    #[test]
    fn game_exit_clears_the_gate_verdict() {
        let mut settings = gated_settings(false);
        settings.games.pause_when_no_game = true;
        let state = RuntimeState::new(settings, None);
        let (gate_tx, lookup, _) = held_lookup();
        {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::plan_detection_transition(
                &mut inner,
                Some(league_game(41)),
                Some(lookup),
            )
            .unwrap();
        }
        gate_tx.send(Some(LeagueQueue::from_id(430))).unwrap();
        {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::resolve_league_gate(&mut inner).unwrap();
        }

        let (prepared, _, _) = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::plan_detection_transition(&mut inner, None, None).unwrap()
        };
        assert!(prepared.unwrap().replacement.is_none());
        {
            let inner = state.0.lock().unwrap();
            assert_eq!(inner.league_gate, None);
            assert!(inner.league_gate_rx.is_none());
            assert!(inner.active_game.is_none());
        }
    }

    #[test]
    fn allowed_resolution_preserves_an_active_manual_session() {
        let state = RuntimeState::new(gated_settings(false), None);
        let (gate_tx, lookup, _) = held_lookup();
        {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::plan_detection_transition(
                &mut inner,
                Some(league_game(41)),
                Some(lookup),
            )
            .unwrap();
            // Manual recording started while the lookup was pending, after
            // the detection teardown.
            let (manual_tx, _manual_rx) = mpsc::channel();
            RuntimeState::install_recording_sender(&mut inner, manual_tx);
            inner.manual_full_session_desired = true;
        }

        gate_tx.send(Some(LeagueQueue::from_id(420))).unwrap();
        let resolution = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::resolve_league_gate(&mut inner).unwrap()
        };
        assert!(
            matches!(resolution, Some(LeagueGateResolution::Allowed(None))),
            "an allowed verdict must not restart a running manual session"
        );
        {
            let inner = state.0.lock().unwrap();
            assert_eq!(inner.league_gate, Some(LeagueGateVerdict::Allowed));
            assert!(inner.tx.is_some(), "the manual sender must stay installed");
        }
    }

    #[test]
    fn detection_without_factory_falls_back_to_immediate_start() {
        // Simulates the settings snapshot race: the detector decided no gate
        // and passed no factory, but the live settings are gated by the time
        // the runtime checks. Must not panic the detector thread.
        let (tx, _rx) = mpsc::channel();
        let state = RuntimeState::with_sender(tx, gated_settings(false), None);
        let (prepared, _, _) = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::plan_detection_transition(&mut inner, Some(league_game(41)), None)
                .unwrap()
        };
        let prepared = prepared.expect("fallback must start immediately");
        assert!(prepared.replacement.is_some());
        {
            let inner = state.0.lock().unwrap();
            assert_eq!(inner.league_gate, None);
            assert!(inner.league_gate_rx.is_none());
        }
    }

    #[test]
    fn settings_restart_clearing_the_active_game_clears_the_gate() {
        let (tx, _rx) = mpsc::channel();
        let state = RuntimeState::with_sender(tx, gated_settings(false), None);
        let (gate_tx, lookup, _) = held_lookup();
        {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::plan_detection_transition(
                &mut inner,
                Some(league_game(41)),
                Some(lookup),
            )
            .unwrap();
        }
        gate_tx.send(Some(LeagueQueue::from_id(430))).unwrap();
        {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::resolve_league_gate(&mut inner).unwrap();
            assert_eq!(inner.league_gate, Some(LeagueGateVerdict::Denied));
        }

        // Disabling auto-detect removes the active game on the next save.
        let changed = AppSettings {
            games: crate::settings::GameSettings {
                auto_detect: false,
                ..AppSettings::default().games
            },
            ..AppSettings::default()
        };
        let prepared = state.prepare_settings_restart(changed).unwrap();
        let committed: CommittedRuntimeRestart<()> = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::commit_prepared_restart_with(&mut inner, prepared, |_| {
                (mpsc::channel().0, ())
            })
            .unwrap()
        };
        assert!(committed.cleared_active_game);
        assert!(
            committed.replacement.is_some(),
            "a stale gate must not keep the recorder stopped after the game is cleared"
        );
        {
            let inner = state.0.lock().unwrap();
            assert_eq!(inner.league_gate, None);
            assert!(inner.league_gate_rx.is_none());
            assert!(inner.active_game.is_none());
        }
    }

    #[test]
    fn gate_lookup_missing_lockfile_resolves_unknown() {
        let missing = std::env::temp_dir()
            .join(format!("clipline-gate-missing-{}", std::process::id()))
            .join("Game")
            .join("League of Legends.exe");
        let game = DetectedGame {
            exe_path: Some(missing.to_string_lossy().into_owned()),
            ..league_game(1)
        };

        let rx = spawn_gate_lookup(&game);
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(None) => {}
            other => panic!("missing lockfile must resolve to unknown: {other:?}"),
        }
    }
}
