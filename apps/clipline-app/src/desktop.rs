//! Application-owned desktop state, independent of the active UI framework.

use std::sync::Mutex;

use clipline_desktop::{
    ApplyEventOutcome, ControllerError, DesktopController, DesktopSnapshot, UiEvent,
};

use crate::settings::AppSettings;

pub mod tauri_sink;

pub struct DesktopState(Mutex<DesktopController<AppSettings>>);

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
}
