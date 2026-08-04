use clipline_desktop::{
    ui_event_channel, ApplyEventOutcome, DesktopSnapshot, RecorderEvent, UiEvent, UiEventReceiver,
    UiEventSendError, UiEventSender, UiEventSink, WindowLifecycleSnapshot,
};
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager, Runtime};

use super::DesktopState;

const WINDOW_LIFECYCLE_EVENT: &str = "window-lifecycle";
const CLOUD_UPLOAD_PROGRESS_EVENT: &str = "cloud-upload-progress";
pub const DESKTOP_EVENT_SEQUENCE_EVENT: &str = "desktop-event-sequence";

#[derive(Clone, serde::Serialize)]
struct DesktopEventSequence {
    event_sequence: u64,
    snapshot_revision: clipline_desktop::Revision,
    window_lifecycle: WindowLifecycleSnapshot,
    lifecycle_revision: String,
    notices: Vec<DesktopNoticePresentation>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct DesktopNoticePresentation {
    /// Decimal text keeps the full `u64` identity exact across JavaScript IPC.
    pub id: String,
    pub message: String,
}

pub fn pending_notice_presentations<S>(
    snapshot: &DesktopSnapshot<S>,
) -> Vec<DesktopNoticePresentation> {
    snapshot
        .notices
        .iter()
        .map(|notice| DesktopNoticePresentation {
            id: notice.id.to_string(),
            message: notice.message.clone(),
        })
        .collect()
}

impl DesktopEventSequence {
    fn from_snapshot(
        event_sequence: u64,
        snapshot: &DesktopSnapshot<crate::settings::AppSettings>,
    ) -> Self {
        Self {
            event_sequence,
            snapshot_revision: snapshot.revision,
            window_lifecycle: snapshot.lifecycle,
            lifecycle_revision: snapshot.lifecycle.revision.get().to_string(),
            notices: pending_notice_presentations(snapshot),
        }
    }
}

#[derive(Clone)]
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
                    match state.apply_sequenced(update.sequence, update.event.clone()) {
                        Ok(ApplyEventOutcome::Applied { .. }) => {
                            let snapshot = state.snapshot();
                            emit_sequence(&app, update.sequence, &snapshot);
                            for emission in tauri_emissions(&update.event) {
                                if let Err(error) = app.emit(emission.name, emission.payload) {
                                    tracing::error!(
                                        event = "tauri_ui_event_emit_failed",
                                        name = emission.name,
                                        error = %error
                                    );
                                }
                            }
                        }
                        Ok(ApplyEventOutcome::Unchanged) => {
                            let snapshot = state.snapshot();
                            emit_sequence(&app, update.sequence, &snapshot);
                            if should_emit_unchanged_event(&update.event) {
                                for emission in tauri_emissions(&update.event) {
                                    if let Err(error) = app.emit(emission.name, emission.payload) {
                                        tracing::error!(
                                            event = "tauri_ui_event_emit_failed",
                                            name = emission.name,
                                            error = %error
                                        );
                                    }
                                }
                            }
                        }
                        Ok(ApplyEventOutcome::Stale) => {
                            let snapshot = state.snapshot();
                            emit_sequence(&app, update.sequence, &snapshot);
                        }
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

fn should_emit_unchanged_event(event: &UiEvent) -> bool {
    matches!(event, UiEvent::MicTestStopped { .. })
}

fn emit_sequence<R: Runtime>(
    app: &AppHandle<R>,
    sequence: u64,
    snapshot: &DesktopSnapshot<crate::settings::AppSettings>,
) {
    if let Err(error) = app.emit(
        DESKTOP_EVENT_SEQUENCE_EVENT,
        DesktopEventSequence::from_snapshot(sequence, snapshot),
    ) {
        tracing::error!(
            event = "tauri_ui_sequence_emit_failed",
            sequence,
            error = %error
        );
    }
}

struct TauriEmission {
    name: &'static str,
    payload: Value,
}

fn tauri_emissions(event: &UiEvent) -> Vec<TauriEmission> {
    let emission = |name, payload| TauriEmission { name, payload };
    let one = |name, payload| vec![emission(name, payload)];
    match event {
        UiEvent::Recorder { event, .. } => match event {
            RecorderEvent::MediaRootResolved { .. } => Vec::new(),
            RecorderEvent::Status { .. } => serde_json::to_value(event)
                .map_or_else(|_| Vec::new(), |payload| one("status", payload)),
            RecorderEvent::Saved { .. } => serde_json::to_value(event)
                .map_or_else(|_| Vec::new(), |payload| one("saved", payload)),
            RecorderEvent::Error { message } => one("error", Value::String(message.clone())),
        },
        UiEvent::WindowLifecycle { snapshot } => serde_json::to_value(snapshot).map_or_else(
            |_| Vec::new(),
            |payload| one(WINDOW_LIFECYCLE_EVENT, payload),
        ),
        UiEvent::MicMonitor { monitor, .. } => serde_json::to_value(monitor)
            .map_or_else(|_| Vec::new(), |payload| one("mic-test", payload)),
        UiEvent::MicTestError { message, .. } => {
            one("mic-test-error", Value::String(message.clone()))
        }
        UiEvent::MicTestStopped { .. } => one("mic-test-stopped", Value::Null),
        UiEvent::GameDetection { detection, .. } => serde_json::to_value(detection)
            .map_or_else(|_| Vec::new(), |payload| one("game-detection", payload)),
        UiEvent::CloudAccountChanged { .. } => Vec::new(),
        UiEvent::CloudUploadProgress { progress, .. } => serde_json::to_value(progress)
            .map_or_else(
                |_| Vec::new(),
                |payload| one(CLOUD_UPLOAD_PROGRESS_EVENT, payload),
            ),
        // The shipping command response already removes the compatibility
        // record. This native snapshot event has no legacy WebView name.
        UiEvent::CloudUploadRemoved { .. } => Vec::new(),
        UiEvent::EnrichmentUpdated { .. } => one("osu-enrichment-updated", Value::Null),
        // The compact catalog summary is native-shell state. The shipping
        // WebView already owns its rich catalog model and has no legacy event
        // contract for this migration-only projection.
        UiEvent::CatalogSummaryChanged { .. } => Vec::new(),
        // Native Settings models retrieve the full bounded catalog directly;
        // the compatibility WebView continues using its existing commands.
        UiEvent::SettingsProbeChanged { .. } => Vec::new(),
        UiEvent::UserError { message } => one("error", Value::String(message.clone())),
    }
}

#[cfg(test)]
mod tests {
    use clipline_desktop::{
        CatalogSummarySnapshot, CatalogSummarySource, GameDetection, Generation, RecorderEvent,
        Revision, UiEvent, WindowLifecycleMode, WindowLifecycleSnapshot,
    };
    use serde_json::json;

    use super::{should_emit_unchanged_event, tauri_emissions, DesktopEventSequence};
    use crate::desktop::DesktopState;
    use crate::settings::AppSettings;

    #[test]
    fn sequence_payload_carries_exact_pending_notice_ids_and_lifecycle() {
        let state = DesktopState::new(AppSettings::default(), vec!["warning".into()]).unwrap();
        let foreground =
            WindowLifecycleSnapshot::new(Revision::new(7), WindowLifecycleMode::Foreground);
        state
            .apply_event(UiEvent::WindowLifecycle {
                snapshot: foreground,
            })
            .unwrap();
        let snapshot = state.snapshot();
        let payload = DesktopEventSequence::from_snapshot(9, &snapshot);

        assert_eq!(payload.event_sequence, 9);
        assert_eq!(payload.window_lifecycle, foreground);
        assert_eq!(payload.lifecycle_revision, "7");
        assert_eq!(payload.notices.len(), 1);
        assert_eq!(payload.notices[0].id, snapshot.notices[0].id.to_string());
        assert_eq!(payload.notices[0].message, "warning");
    }

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
        let mut emissions = tauri_emissions(&status);
        let emission = emissions.remove(0);
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
        let mut emissions = tauri_emissions(&error);
        let emission = emissions.remove(0);
        assert_eq!(emission.name, "error");
        assert_eq!(emission.payload, json!("failed"));
    }

    #[test]
    fn microphone_failure_preserves_legacy_error_then_stopped_order() {
        let mut emissions = tauri_emissions(&UiEvent::MicTestError {
            generation: Generation::new(2),
            message: "device lost".into(),
        });
        emissions.extend(tauri_emissions(&UiEvent::MicTestStopped {
            generation: Generation::new(2),
        }));
        assert_eq!(emissions.len(), 2);
        assert_eq!(emissions[0].name, "mic-test-error");
        assert_eq!(emissions[1].name, "mic-test-stopped");
    }

    #[test]
    fn accepted_unchanged_microphone_stop_is_the_only_forwarded_unchanged_event() {
        assert!(should_emit_unchanged_event(&UiEvent::MicTestStopped {
            generation: Generation::new(2),
        }));
        assert!(!should_emit_unchanged_event(&UiEvent::MicMonitor {
            generation: Generation::new(2),
            monitor: clipline_desktop::MicMonitor::new(0.0, 0.0, Vec::new()).unwrap(),
        }));
    }

    #[test]
    fn game_detection_keeps_legacy_field_names_and_recording_mode() {
        let mut emissions = tauri_emissions(&UiEvent::GameDetection {
            generation: Generation::new(3),
            detection: GameDetection {
                active: true,
                name: Some("osu!".into()),
                window_title: Some("osu! - player".into()),
                process_id: Some(42),
                process_instance_id: Some("42:100".into()),
                exe_name: Some("osu!.exe".into()),
                recording_mode: Some("replays_only".into()),
                elevated_hotkeys_blocked: false,
            },
        });
        let emission = emissions.remove(0);
        assert_eq!(emission.name, "game-detection");
        assert_eq!(
            emission.payload,
            json!({
                "active": true,
                "name": "osu!",
                "window_title": "osu! - player",
                "process_id": 42,
                "process_instance_id": "42:100",
                "exe_name": "osu!.exe",
                "recording_mode": "replays_only",
                "elevated_hotkeys_blocked": false
            })
        );
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

    #[test]
    fn native_catalog_summary_does_not_invent_a_legacy_webview_event() {
        assert!(tauri_emissions(&UiEvent::CatalogSummaryChanged {
            summary: CatalogSummarySnapshot {
                revision: Revision::new(1),
                source: CatalogSummarySource::Cloud,
                active: true,
            },
        })
        .is_empty());
    }
}
