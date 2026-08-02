use clipline_desktop::{
    ui_event_channel, ApplyEventOutcome, RecorderEvent, UiEvent, UiEventReceiver, UiEventSendError,
    UiEventSender, UiEventSink,
};
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager, Runtime};

use super::DesktopState;

const WINDOW_LIFECYCLE_EVENT: &str = "window-lifecycle";
const CLOUD_UPLOAD_PROGRESS_EVENT: &str = "cloud-upload-progress";

pub struct TauriUiEventSink(UiEventSender);

impl TauriUiEventSink {
    pub fn channel() -> (Self, UiEventReceiver) {
        let (sender, receiver) = ui_event_channel();
        (Self(sender), receiver)
    }
}

impl UiEventSink for TauriUiEventSink {
    fn try_publish(
        &self,
        event: UiEvent,
    ) -> Result<clipline_desktop::UiEventPublishOutcome, UiEventSendError> {
        self.0.try_publish(event)
    }
}

pub fn spawn_event_pump<R: Runtime>(
    app: AppHandle<R>,
    receiver: UiEventReceiver,
) -> Result<(), String> {
    std::thread::Builder::new()
        .name("clipline-tauri-ui-events".to_owned())
        .spawn(move || loop {
            match receiver.wait_recv(std::time::Duration::from_millis(250)) {
                Ok(Some(update)) => {
                    let state = app.state::<DesktopState>();
                    match state.apply_event(update.event.clone()) {
                        Ok(ApplyEventOutcome::Applied { .. }) => {
                            if let Some(emission) = tauri_emission(&update.event) {
                                if let Err(error) = app.emit(emission.name, emission.payload) {
                                    tracing::error!(
                                        event = "tauri_ui_event_emit_failed",
                                        name = emission.name,
                                        error = %error
                                    );
                                }
                            }
                        }
                        Ok(ApplyEventOutcome::Unchanged | ApplyEventOutcome::Stale) => {}
                        Err(error) => tracing::error!(
                            event = "desktop_event_apply_failed",
                            error = %error
                        ),
                    }
                }
                Ok(None) => {}
                Err(_) => break,
            }
        })
        .map(|_| ())
        .map_err(|error| format!("spawn Tauri UI event pump: {error}"))
}

struct TauriEmission {
    name: &'static str,
    payload: Value,
}

fn tauri_emission(event: &UiEvent) -> Option<TauriEmission> {
    let (name, payload) = match event {
        UiEvent::Recorder { event, .. } => match event {
            RecorderEvent::MediaRootResolved { .. } => return None,
            RecorderEvent::Status { .. } => ("status", serde_json::to_value(event).ok()?),
            RecorderEvent::Saved { .. } => ("saved", serde_json::to_value(event).ok()?),
            RecorderEvent::Error { message } => ("error", Value::String(message.clone())),
        },
        UiEvent::WindowLifecycle { snapshot } => {
            (WINDOW_LIFECYCLE_EVENT, serde_json::to_value(snapshot).ok()?)
        }
        UiEvent::MicMonitor { monitor, .. } => ("mic-test", serde_json::to_value(monitor).ok()?),
        UiEvent::MicTestError { message, .. } => ("mic-test-error", Value::String(message.clone())),
        UiEvent::MicTestStopped { .. } => ("mic-test-stopped", Value::Null),
        UiEvent::GameDetection { detection, .. } => {
            ("game-detection", serde_json::to_value(detection).ok()?)
        }
        UiEvent::CloudUploadProgress { progress, .. } => (
            CLOUD_UPLOAD_PROGRESS_EVENT,
            serde_json::to_value(progress).ok()?,
        ),
        UiEvent::EnrichmentUpdated { .. } => ("osu-enrichment-updated", Value::Null),
        UiEvent::UserError { message } => ("error", Value::String(message.clone())),
    };
    Some(TauriEmission { name, payload })
}

#[cfg(test)]
mod tests {
    use clipline_desktop::{Generation, RecorderEvent, UiEvent};
    use serde_json::json;

    use super::tauri_emission;

    #[test]
    fn recorder_status_and_error_keep_legacy_event_json() {
        let status = UiEvent::Recorder {
            generation: Generation::new(4),
            event: RecorderEvent::Status {
                recording: true,
                waiting_for_game: false,
                segments: 2,
                buffered_s: 3.0,
                buffered_mb: 4.0,
                full_session: false,
                encoder: "H.264".into(),
                capture_backend: "wgc".into(),
            },
        };
        let emission = tauri_emission(&status).unwrap();
        assert_eq!(emission.name, "status");
        assert_eq!(
            emission.payload,
            json!({
                "kind": "status",
                "recording": true,
                "waiting_for_game": false,
                "segments": 2,
                "buffered_s": 3.0,
                "buffered_mb": 4.0,
                "full_session": false,
                "encoder": "H.264",
                "capture_backend": "wgc"
            })
        );

        let error = UiEvent::UserError {
            message: "failed".into(),
        };
        let emission = tauri_emission(&error).unwrap();
        assert_eq!(emission.name, "error");
        assert_eq!(emission.payload, json!("failed"));
    }

    #[test]
    fn every_shipping_event_name_is_mapped_once() {
        let source = include_str!("tauri_sink.rs");
        for name in [
            "status",
            "saved",
            "error",
            "window-lifecycle",
            "mic-test",
            "mic-test-error",
            "mic-test-stopped",
            "game-detection",
            "cloud-upload-progress",
            "osu-enrichment-updated",
        ] {
            assert!(source.contains(name), "missing Tauri mapping for {name}");
        }
    }
}
