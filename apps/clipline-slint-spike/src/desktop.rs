//! Slint event-loop adapter for the framework-neutral desktop contract.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use clipline_desktop::{
    ui_event_channel, ApplyEventOutcome, DesktopController, DesktopSnapshot, Revision, UiEvent,
    UiEventPublishOutcome, UiEventReceiver, UiEventSendError, UiEventSender, MAX_ACTIVE_UPLOADS,
};

use crate::{CliplineSpike, DesktopUploadItem};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopUploadProjection {
    pub local_clip_id: String,
    pub status: String,
    pub progress: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopProjection {
    pub revision: Revision,
    pub recorder_label: String,
    pub notice: String,
    pub uploads: Vec<DesktopUploadProjection>,
}

impl DesktopProjection {
    #[must_use]
    pub fn from_snapshot<S>(snapshot: &DesktopSnapshot<S>) -> Self {
        let recorder = &snapshot.recorder.status;
        let recorder_label = if recorder.recording {
            if recorder.encoder.is_empty() {
                "RECORDING".to_owned()
            } else {
                format!("RECORDING · {}", recorder.encoder)
            }
        } else if recorder.waiting_for_game {
            "WAITING FOR GAME".to_owned()
        } else if snapshot.recorder.desired {
            "STARTING".to_owned()
        } else {
            "STOPPED".to_owned()
        };
        let notice = snapshot
            .notices
            .last()
            .map_or_else(String::new, |notice| notice.message.clone());
        let uploads = snapshot
            .uploads
            .iter()
            .take(MAX_ACTIVE_UPLOADS)
            .map(|upload| {
                let progress = &upload.progress;
                DesktopUploadProjection {
                    local_clip_id: progress.local_clip_id.clone(),
                    status: progress.upload_status.clone(),
                    progress: format!(
                        "{} / {} bytes",
                        progress.received_size_bytes, progress.file_size_bytes
                    ),
                }
            })
            .collect();
        Self {
            revision: snapshot.revision,
            recorder_label,
            notice,
            uploads,
        }
    }
}

#[derive(Clone, Default)]
pub struct RevisionGate(Arc<AtomicU64>);

impl RevisionGate {
    pub fn post(&self, revision: u64) {
        self.0.store(revision, Ordering::Release);
    }

    #[must_use]
    pub fn is_current(&self, revision: u64) -> bool {
        self.0.load(Ordering::Acquire) == revision
    }
}

pub struct SlintDesktopAdapter {
    sender: UiEventSender,
    consumer_alive: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl SlintDesktopAdapter {
    pub fn start(window: slint::Weak<CliplineSpike>) -> Result<Self, String> {
        let (sender, receiver) = ui_event_channel();
        let consumer_alive = Arc::new(AtomicBool::new(true));
        let worker = spawn_consumer(receiver, window, Arc::clone(&consumer_alive))?;
        Ok(Self {
            sender,
            consumer_alive,
            worker: Some(worker),
        })
    }

    pub fn try_publish(&self, event: UiEvent) -> Result<UiEventPublishOutcome, UiEventSendError> {
        self.sender.try_publish(event)
    }
}

impl Drop for SlintDesktopAdapter {
    fn drop(&mut self) {
        self.consumer_alive.store(false, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn spawn_consumer(
    receiver: UiEventReceiver,
    window: slint::Weak<CliplineSpike>,
    consumer_alive: Arc<AtomicBool>,
) -> Result<JoinHandle<()>, String> {
    thread::Builder::new()
        .name("clipline-slint-desktop-events".to_owned())
        .spawn(move || {
            let mut controller =
                DesktopController::new((), Vec::new()).expect("empty desktop snapshot is valid");
            let gate = RevisionGate::default();
            while consumer_alive.load(Ordering::Acquire) {
                let update = match receiver.wait_recv(Duration::from_millis(50)) {
                    Ok(Some(update)) => update,
                    Ok(None) => continue,
                    Err(_) => break,
                };
                let Ok(outcome) = controller.apply_event(update.event) else {
                    continue;
                };
                if !matches!(outcome, ApplyEventOutcome::Applied { .. }) {
                    continue;
                }
                let projection = DesktopProjection::from_snapshot(&controller.snapshot());
                let revision = projection.revision.get();
                gate.post(revision);
                let gate = gate.clone();
                let weak = window.clone();
                let posted_consumer_alive = Arc::clone(&consumer_alive);
                if slint::invoke_from_event_loop(move || {
                    if !gate.is_current(revision) {
                        return;
                    }
                    let Some(window) = weak.upgrade() else {
                        posted_consumer_alive.store(false, Ordering::Release);
                        return;
                    };
                    apply_projection(&window, projection);
                })
                .is_err()
                {
                    consumer_alive.store(false, Ordering::Release);
                    break;
                }
            }
        })
        .map_err(|error| format!("spawn Slint desktop event consumer: {error}"))
}

fn apply_projection(window: &CliplineSpike, projection: DesktopProjection) {
    window.set_recorder_state(projection.recorder_label.into());
    window.set_desktop_notice(projection.notice.into());
    let uploads = projection
        .uploads
        .into_iter()
        .take(MAX_ACTIVE_UPLOADS)
        .map(|upload| DesktopUploadItem {
            local_clip_id: upload.local_clip_id.into(),
            status: upload.status.into(),
            progress: upload.progress.into(),
        })
        .collect::<Vec<_>>();
    window.set_desktop_uploads(slint::ModelRc::new(slint::VecModel::from(uploads)));
}
