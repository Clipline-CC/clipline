//! Application-owned desktop state, independent of the active UI framework.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use clipline_desktop::{
    ApplyEventOutcome, ControllerError, DesktopController, DesktopSnapshot, DispatchOutcome,
    UiAction, UiEvent,
};

use crate::settings::AppSettings;

pub mod tauri_sink;

pub struct DesktopState(Mutex<DesktopController<AppSettings>>);

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
        DesktopController::new(settings, warnings).map(|controller| Self(Mutex::new(controller)))
    }

    pub fn snapshot(&self) -> DesktopSnapshot<AppSettings> {
        match self.0.lock() {
            Ok(controller) => controller.snapshot(),
            Err(poisoned) => poisoned.into_inner().snapshot(),
        }
    }

    pub fn apply_event(&self, event: UiEvent) -> Result<ApplyEventOutcome, String> {
        self.0
            .lock()
            .map_err(|_| "desktop state lock poisoned".to_owned())?
            .apply_event(event)
            .map_err(|error| error.to_string())
    }

    pub fn dispatch(&self, action: UiAction) -> Result<DispatchOutcome, String> {
        self.0
            .lock()
            .map_err(|_| "desktop state lock poisoned".to_owned())?
            .dispatch(action)
            .map_err(|error| error.to_string())
    }

    pub fn replace_settings(&self, settings: AppSettings) -> Result<bool, String> {
        self.0
            .lock()
            .map_err(|_| "desktop state lock poisoned".to_owned())?
            .replace_settings(settings)
            .map_err(|error| error.to_string())
    }

    pub fn set_recorder_desired(&self, desired: bool) -> Result<bool, String> {
        self.0
            .lock()
            .map_err(|_| "desktop state lock poisoned".to_owned())?
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
}
