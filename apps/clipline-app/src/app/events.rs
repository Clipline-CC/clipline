use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use tauri::{
    AppHandle, Emitter, Manager, Runtime, WindowEvent,
};


use crate::games::DetectedGame;
use crate::service::Event;
use crate::settings::GameRecordingMode;
use super::*;

#[derive(serde::Serialize, Clone)]
pub(crate) struct GameDetectionEvent {
    pub(crate) active: bool,
    pub(crate) name: Option<String>,
    pub(crate) window_title: Option<String>,
    pub(crate) process_id: Option<u32>,
    pub(crate) process_instance_id: Option<String>,
    pub(crate) exe_name: Option<String>,
    pub(crate) recording_mode: Option<GameRecordingMode>,
    pub(crate) elevated_hotkeys_blocked: bool,
}

impl GameDetectionEvent {
    pub(crate) fn from_detected(detected: Option<&DetectedGame>) -> Self {
        Self::from_detected_with_process_queries(
            detected,
            crate::windows::current_process_is_elevated(),
            crate::windows::process_is_elevated,
            crate::windows::process_instance_id,
        )
    }

    #[cfg(test)]
    pub(crate) fn from_detected_with_elevation(
        detected: Option<&DetectedGame>,
        clipline_elevated: Result<bool, String>,
        game_is_elevated: impl FnOnce(u32) -> Result<bool, String>,
    ) -> Self {
        Self::from_detected_with_process_queries(
            detected,
            clipline_elevated,
            game_is_elevated,
            |process_id| Ok(format!("{process_id}:test")),
        )
    }

    pub(crate) fn from_detected_with_process_queries(
        detected: Option<&DetectedGame>,
        clipline_elevated: Result<bool, String>,
        game_is_elevated: impl FnOnce(u32) -> Result<bool, String>,
        process_instance_id: impl FnOnce(u32) -> Result<String, String>,
    ) -> Self {
        match detected {
            Some(game) => {
                let elevated_hotkeys_blocked = matches!(clipline_elevated, Ok(false))
                    && game_is_elevated(game.process_id).unwrap_or(true);
                let process_instance_id = elevated_hotkeys_blocked.then(|| {
                    process_instance_id(game.process_id)
                        .unwrap_or_else(|_| format!("{}:window:{}", game.process_id, game.hwnd))
                });
                Self {
                    active: true,
                    name: Some(game.name.clone()),
                    window_title: Some(game.window_title.clone()),
                    process_id: Some(game.process_id),
                    process_instance_id,
                    exe_name: Some(game.exe_name.clone()),
                    recording_mode: Some(game.recording_mode),
                    elevated_hotkeys_blocked,
                }
            }
            None => Self {
                active: false,
                name: None,
                window_title: None,
                process_id: None,
                process_instance_id: None,
                exe_name: None,
                recording_mode: None,
                elevated_hotkeys_blocked: false,
            },
        }
    }
}

pub(crate) fn should_log_window_event(event: &WindowEvent) -> bool {
    !matches!(event, WindowEvent::Moved(_) | WindowEvent::Resized(_))
}

pub(crate) fn should_reconcile_native_window_event(event: &WindowEvent) -> bool {
    matches!(event, WindowEvent::Focused(_) | WindowEvent::Resized(_))
}

impl RuntimeState {
    pub(crate) fn accept_service_media_root(
        &self,
        generation: u64,
        event: &Event,
        publish: impl FnOnce(PathBuf),
    ) -> bool {
        let Event::MediaRootResolved { path, .. } = event else {
            return false;
        };
        let Ok(inner) = self.0.lock() else {
            return false;
        };
        if inner.recording_generation != generation || inner.tx.is_none() {
            return false;
        }
        // Keep runtime ownership through publication: a replacement must not
        // install its sender between this check and the library root update.
        publish(PathBuf::from(path));
        true
    }
}

pub(crate) fn pump_events<R: Runtime>(handle: AppHandle<R>, event_rx: Receiver<Event>, generation: u64) {
    std::thread::spawn(move || {
        for event in event_rx {
            if matches!(&event, Event::StorageQuotaFull { .. }) {
                if !handle
                    .state::<RuntimeState>()
                    .accept_service_quota(generation, &event)
                {
                    continue;
                }
            } else {
                handle.state::<RuntimeState>().observe_runtime_event(&event);
            }
            if matches!(&event, Event::MediaRootResolved { .. }) {
                if !handle.state::<RuntimeState>().accept_service_media_root(
                    generation,
                    &event,
                    |root| {
                        handle
                            .state::<crate::library::StorageSettings>()
                            .set_media_dir(root)
                    },
                ) {
                    continue;
                }
            }
            if let Event::Status { recording, .. } = &event {
                let accepted = handle
                    .state::<RuntimeState>()
                    .accept_service_status(generation, *recording);
                if !accepted {
                    continue;
                }
            }
            let _ = match &event {
                Event::MediaRootResolved { .. } => Ok(()),
                Event::Status { .. } => handle.emit("status", &event),
                Event::Saved { .. } => handle.emit("saved", &event),
                Event::StorageQuotaFull { .. } => handle.emit("storage-quota-full", &event),
                Event::BookmarkAdded { .. } => handle.emit("bookmark-added", &event),
                Event::LibraryChanged => handle.emit("library-changed", ()),
                Event::Error { message } => handle.emit("error", message.clone()),
            };
            if let Event::BookmarkAdded { .. } = &event {
                crate::sound::play_bookmark_added();
            }
            if let Event::Saved {
                full_session: false,
                ..
            } = &event
            {
                crate::sound::play_replay_saved();
            }
            if let Event::Saved {
                path,
                seconds,
                recording_start_unix,
                recording_end_unix,
                full_session: true,
                ..
            } = &event
            {
                let title_events = handle
                    .state::<RuntimeState>()
                    .osu_title_events_for_window(*recording_start_unix, *recording_end_unix);
                let saved = crate::osu_enrichment::OsuSavedClip {
                    path: std::path::PathBuf::from(path),
                    seconds: *seconds,
                    full_session: true,
                    recording_start_unix: *recording_start_unix,
                    recording_end_unix: *recording_end_unix,
                    title_events,
                };
                match crate::osu_enrichment::write_pending_for_saved_clip(&saved) {
                    Ok(Some(_)) => {
                        let app = handle.clone();
                        let media_root = handle
                            .state::<crate::library::StorageSettings>()
                            .media_dir();
                        tauri::async_runtime::spawn(async move {
                            if let Err(e) =
                                crate::osu_api::retry_pending_enrichment(&app, media_root).await
                            {
                                tracing::warn!(event = "save_osu_enrichment_retry_failed", error = %e);
                            }
                        });
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(event = "osu_enrichment_queue_failed", error = %e);
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{detected_game};

    #[test]
    fn media_root_old_service_cannot_replace_committed_replacement_root() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let state = RuntimeState::with_sender(tx, crate::settings::AppSettings::default(), None);
        let storage = crate::library::StorageSettings::new(None, PathBuf::from("initial"));
        let old_generation = state.0.lock().unwrap().recording_generation;
        let prepared = state.prepare_settings_restart(state.settings()).unwrap();
        let committed = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::commit_prepared_restart_with(&mut inner, prepared, |_| {
                (std::sync::mpsc::channel().0, ())
            })
            .unwrap()
        };
        let (_, generation) = committed.replacement.unwrap();
        let root_b = Event::MediaRootResolved {
            path: "replacement-B".into(),
            fell_back: false,
        };
        assert!(state
            .accept_service_media_root(generation, &root_b, |root| storage.set_media_dir(root)));
        let root_a = Event::MediaRootResolved {
            path: "old-A".into(),
            fell_back: true,
        };
        state
            .accept_service_media_root(old_generation, &root_a, |root| storage.set_media_dir(root));
        assert_eq!(storage.media_dir(), PathBuf::from("replacement-B"));
    }

    #[test]
    fn media_root_publication_serializes_with_runtime_replacement() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let state = RuntimeState::with_sender(tx, crate::settings::AppSettings::default(), None);
        let storage = crate::library::StorageSettings::new(None, PathBuf::from("initial"));
        let generation = state.0.lock().unwrap().recording_generation;
        let prepared = state.prepare_settings_restart(state.settings()).unwrap();
        let (publishing_tx, publishing_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let state = &state;
            let storage = &storage;
            let replacement = scope.spawn(move || {
                publishing_rx.recv().unwrap();
                let committed = {
                    let mut inner = state.0.lock().unwrap();
                    RuntimeState::commit_prepared_restart_with(&mut inner, prepared, |_| {
                        (std::sync::mpsc::channel().0, ())
                    })
                    .unwrap()
                };
                let (_, generation) = committed.replacement.unwrap();
                state.accept_service_media_root(
                    generation,
                    &Event::MediaRootResolved {
                        path: "replacement-B".into(),
                        fell_back: false,
                    },
                    |root| storage.set_media_dir(root),
                );
            });
            state.accept_service_media_root(
                generation,
                &Event::MediaRootResolved {
                    path: "old-A".into(),
                    fell_back: false,
                },
                |root| {
                    let owns_runtime =
                        matches!(state.0.try_lock(), Err(std::sync::TryLockError::WouldBlock));
                    publishing_tx.send(()).unwrap();
                    storage.set_media_dir(root);
                    assert!(
                        owns_runtime,
                        "generation validation and root mutation must share the runtime lock"
                    );
                },
            );
            replacement.join().unwrap();
        });
        assert_eq!(storage.media_dir(), PathBuf::from("replacement-B"));
    }

    #[test]
    fn media_root_retired_service_cannot_publish_during_restart_gap() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let state = RuntimeState::with_sender(tx, crate::settings::AppSettings::default(), None);
        let storage = crate::library::StorageSettings::new(None, PathBuf::from("initial"));
        let generation = state.0.lock().unwrap().recording_generation;
        let _restart = RuntimeState::prepare_service_restart(&mut state.0.lock().unwrap()).unwrap();
        state.accept_service_media_root(
            generation,
            &Event::MediaRootResolved {
                path: "old-A".into(),
                fell_back: false,
            },
            |root| storage.set_media_dir(root),
        );
        assert_eq!(storage.media_dir(), PathBuf::from("initial"));
    }

    #[test]
    fn elevated_game_warning_requires_lower_privilege_clipline() {
        let game = detected_game("endfield", "Arknights: Endfield", 42);

        let blocked = GameDetectionEvent::from_detected_with_elevation(
            Some(&game),
            Ok(false),
            |process_id| Ok(process_id == 42),
        );
        assert!(blocked.elevated_hotkeys_blocked);

        let already_elevated =
            GameDetectionEvent::from_detected_with_elevation(Some(&game), Ok(true), |_| Ok(true));
        assert!(!already_elevated.elevated_hotkeys_blocked);

        let ordinary_game =
            GameDetectionEvent::from_detected_with_elevation(Some(&game), Ok(false), |_| Ok(false));
        assert!(!ordinary_game.elevated_hotkeys_blocked);

        let inactive =
            GameDetectionEvent::from_detected_with_elevation(None, Ok(false), |_| Ok(true));
        assert!(!inactive.elevated_hotkeys_blocked);
    }

    #[test]
    fn elevated_game_warning_carries_process_instance_identity() {
        let game = detected_game("endfield", "Arknights: Endfield", 42);

        let event = GameDetectionEvent::from_detected_with_process_queries(
            Some(&game),
            Ok(false),
            |_| Ok(true),
            |process_id| Ok(format!("{process_id}:987654321")),
        );

        assert_eq!(event.process_instance_id.as_deref(), Some("42:987654321"));
    }

    #[test]
    fn elevated_game_warning_is_conservative_when_elevation_cannot_be_queried() {
        let game = detected_game("endfield", "Arknights: Endfield", 42);

        let blocked =
            GameDetectionEvent::from_detected_with_elevation(Some(&game), Ok(false), |_| {
                Err("protected process".to_string())
            });
        assert!(blocked.elevated_hotkeys_blocked);

        let unknown_clipline = GameDetectionEvent::from_detected_with_elevation(
            Some(&game),
            Err("token query failed".to_string()),
            |_| Ok(true),
        );
        assert!(!unknown_clipline.elevated_hotkeys_blocked);
    }

    #[test]
    fn diagnostic_window_event_filter_drops_move_and_resize_noise() {
        assert!(!should_log_window_event(&WindowEvent::Moved(
            tauri::PhysicalPosition::new(10, 20)
        )));
        assert!(!should_log_window_event(&WindowEvent::Resized(
            tauri::PhysicalSize::new(800, 600)
        )));
        assert!(should_log_window_event(&WindowEvent::Focused(true)));
        assert!(should_log_window_event(&WindowEvent::Destroyed));
    }
}
