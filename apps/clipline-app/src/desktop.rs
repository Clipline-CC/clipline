//! Application-owned desktop state, independent of the active UI framework.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use clipline_desktop::{
    ApplyEventOutcome, ControllerError, DesktopController, DesktopSnapshot, DispatchOutcome,
    UiAction, UiEvent,
};

use crate::settings::AppSettings;

pub mod tauri_sink;

pub struct DesktopState(Mutex<DesktopStateInner>);

struct DesktopStateInner {
    controller: DesktopController<AppSettings>,
    event_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DesktopBootstrap {
    pub event_sequence: u64,
    pub snapshot: DesktopSnapshot<AppSettings>,
}

#[derive(Default)]
pub struct ProducerGenerations {
    game_detection: AtomicU64,
    cloud_upload: AtomicU64,
    enrichment: AtomicU64,
}

impl ProducerGenerations {
    pub fn next_game_detection(&self) -> Result<clipline_desktop::Generation, String> {
        next_generation(&self.game_detection, "game detection")
    }

    pub fn next_cloud_upload(&self) -> Result<clipline_desktop::Generation, String> {
        next_generation(&self.cloud_upload, "cloud upload")
    }

    pub fn next_enrichment(&self) -> Result<clipline_desktop::Generation, String> {
        next_generation(&self.enrichment, "enrichment")
    }
}

fn next_generation(
    counter: &AtomicU64,
    domain: &str,
) -> Result<clipline_desktop::Generation, String> {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(1)
        })
        .map(|previous| clipline_desktop::Generation::new(previous + 1))
        .map_err(|_| format!("{domain} generation exhausted"))
}

impl DesktopState {
    pub fn new(settings: AppSettings, warnings: Vec<String>) -> Result<Self, ControllerError> {
        DesktopController::new(settings, warnings).map(|controller| {
            Self(Mutex::new(DesktopStateInner {
                controller,
                event_sequence: 0,
            }))
        })
    }

    pub fn snapshot(&self) -> DesktopSnapshot<AppSettings> {
        match self.0.lock() {
            Ok(inner) => inner.controller.snapshot(),
            Err(poisoned) => poisoned.into_inner().controller.snapshot(),
        }
    }

    pub fn bootstrap(&self) -> DesktopBootstrap {
        let inner = match self.0.lock() {
            Ok(inner) => inner,
            Err(poisoned) => poisoned.into_inner(),
        };
        DesktopBootstrap {
            event_sequence: inner.event_sequence,
            snapshot: inner.controller.snapshot(),
        }
    }

    pub fn apply_event(&self, event: UiEvent) -> Result<ApplyEventOutcome, String> {
        self.0
            .lock()
            .map_err(|_| "desktop state lock poisoned".to_owned())?
            .controller
            .apply_event(event)
            .map_err(|error| error.to_string())
    }

    pub fn apply_sequenced(
        &self,
        sequence: u64,
        event: UiEvent,
    ) -> Result<ApplyEventOutcome, String> {
        let mut inner = self
            .0
            .lock()
            .map_err(|_| "desktop state lock poisoned".to_owned())?;
        if sequence <= inner.event_sequence {
            return Err(format!(
                "desktop event sequence {sequence} did not advance past {}",
                inner.event_sequence
            ));
        }
        let outcome = inner
            .controller
            .apply_event(event)
            .map_err(|error| error.to_string())?;
        inner.event_sequence = sequence;
        Ok(outcome)
    }

    pub fn dispatch(&self, action: UiAction) -> Result<DispatchOutcome, String> {
        self.0
            .lock()
            .map_err(|_| "desktop state lock poisoned".to_owned())?
            .controller
            .dispatch(action)
            .map_err(|error| error.to_string())
    }

    pub fn replace_settings(&self, settings: AppSettings) -> Result<bool, String> {
        self.0
            .lock()
            .map_err(|_| "desktop state lock poisoned".to_owned())?
            .controller
            .replace_settings(settings)
            .map_err(|error| error.to_string())
    }

    pub fn set_recorder_desired(&self, desired: bool) -> Result<bool, String> {
        self.0
            .lock()
            .map_err(|_| "desktop state lock poisoned".to_owned())?
            .controller
            .set_recorder_desired(desired)
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_state_keeps_the_exact_settings_value() {
        let settings = AppSettings::default();
        let state = DesktopState::new(settings.clone(), vec!["warning".into()]).unwrap();
        let snapshot = state.snapshot();
        assert_eq!(snapshot.settings, settings);
        assert_eq!(snapshot.notices[0].message, "warning");
    }

    #[test]
    fn producer_generations_are_monotonic_and_domain_scoped() {
        let generations = ProducerGenerations::default();
        assert_eq!(generations.next_game_detection().unwrap().get(), 1);
        assert_eq!(generations.next_game_detection().unwrap().get(), 2);
        assert_eq!(generations.next_cloud_upload().unwrap().get(), 1);
        assert_eq!(generations.next_enrichment().unwrap().get(), 1);
    }

    #[test]
    fn sequenced_state_rebuilds_at_the_last_applied_event() {
        let state = DesktopState::new(AppSettings::default(), Vec::new()).unwrap();
        let event = UiEvent::WindowLifecycle {
            snapshot: clipline_desktop::WindowLifecycleSnapshot::new(
                clipline_desktop::Revision::new(1),
                clipline_desktop::WindowLifecycleMode::Foreground,
            ),
        };

        state.apply_sequenced(7, event).unwrap();
        let bootstrap = state.bootstrap();
        assert_eq!(bootstrap.event_sequence, 7);
        assert_eq!(
            bootstrap.snapshot.lifecycle.mode,
            clipline_desktop::WindowLifecycleMode::Foreground
        );
        assert!(state
            .apply_sequenced(
                7,
                UiEvent::WindowLifecycle {
                    snapshot: clipline_desktop::WindowLifecycleSnapshot::new(
                        clipline_desktop::Revision::new(2),
                        clipline_desktop::WindowLifecycleMode::Tray,
                    ),
                }
            )
            .is_err());
        assert_eq!(state.bootstrap(), bootstrap);
    }

    #[test]
    fn repeated_bootstrap_keeps_durable_effect_state_without_consuming_it() {
        let state = DesktopState::new(AppSettings::default(), vec!["warning".into()]).unwrap();
        state
            .apply_sequenced(
                1,
                UiEvent::Recorder {
                    generation: clipline_desktop::Generation::new(1),
                    event: clipline_desktop::RecorderEvent::Saved {
                        path: r"C:\clip.mp4".into(),
                        seconds: 5.0,
                        recording_start_unix: None,
                        recording_end_unix: None,
                        markers: 0,
                        full_session: false,
                        gc_deleted: 0,
                        gc_freed_bytes: 0,
                        storage_total_bytes: 10,
                        storage_quota_bytes: None,
                        storage_over_quota: false,
                    },
                },
            )
            .unwrap();

        let first = state.bootstrap();
        let rebuilt = state.bootstrap();
        assert_eq!(rebuilt, first);
        assert_eq!(rebuilt.snapshot.notices[0].message, "warning");
        assert_eq!(
            rebuilt.snapshot.latest_saved.as_ref().unwrap().path,
            r"C:\clip.mp4"
        );
    }
}
