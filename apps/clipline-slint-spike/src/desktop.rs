//! Slint event-loop adapter for the framework-neutral desktop contract.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use clipline_desktop::{
    ui_event_channel, ApplyEventOutcome, CatalogSummarySource, DesktopController, DesktopSnapshot,
    Revision, UiAction, UiEvent, UiEventPublishOutcome, UiEventReceiver, UiEventSendError,
    UiEventSender, MAX_ACTIVE_UPLOADS, MAX_PENDING_NOTICES,
};

use crate::{CliplineSpike, DesktopUploadItem, SpikeTray};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopUploadProjection {
    pub local_clip_id: String,
    pub status: String,
    pub progress: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopProjection {
    pub revision: Revision,
    pub library_revision: Revision,
    pub catalog_revision: Revision,
    pub catalog_source: CatalogSummarySource,
    pub catalog_active: bool,
    pub recorder_label: String,
    pub notice: String,
    pub notice_id: Option<u64>,
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
        let pending_notice = snapshot.notices.first();
        let notice = pending_notice.map_or_else(String::new, |notice| notice.message.clone());
        let notice_id = pending_notice.map(|notice| notice.id);
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
            library_revision: snapshot.library_revision,
            catalog_revision: snapshot.catalog.revision,
            catalog_source: snapshot.catalog.source,
            catalog_active: snapshot.catalog.active,
            recorder_label,
            notice,
            notice_id,
            uploads,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopAttachment(u64);

impl DesktopAttachment {
    #[must_use]
    pub const fn generation(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentError {
    AlreadyAttached,
    GenerationExhausted,
    StaleAttachment,
}

impl fmt::Display for AttachmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for AttachmentError {}

#[derive(Default)]
struct AttachmentGateState {
    generation: u64,
    current: Option<DesktopAttachment>,
    latest_revision: u64,
}

#[derive(Clone, Default)]
pub struct AttachmentGate(Arc<Mutex<AttachmentGateState>>);

impl AttachmentGate {
    pub fn attach(&self, revision: u64) -> Result<DesktopAttachment, AttachmentError> {
        let mut state = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        if state.current.is_some() {
            return Err(AttachmentError::AlreadyAttached);
        }
        state.generation = state
            .generation
            .checked_add(1)
            .ok_or(AttachmentError::GenerationExhausted)?;
        let attachment = DesktopAttachment(state.generation);
        state.current = Some(attachment);
        state.latest_revision = state.latest_revision.max(revision);
        Ok(attachment)
    }

    pub fn detach(&self, attachment: DesktopAttachment) -> Result<(), AttachmentError> {
        let mut state = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        if state.current != Some(attachment) {
            return Err(AttachmentError::StaleAttachment);
        }
        state.current = None;
        Ok(())
    }

    pub fn post_revision(&self, revision: u64) {
        let mut state = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        state.latest_revision = state.latest_revision.max(revision);
    }

    #[must_use]
    pub fn is_current(&self, attachment: DesktopAttachment, revision: u64) -> bool {
        let state = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        state.current == Some(attachment) && state.latest_revision == revision
    }

    #[must_use]
    fn is_latest(&self, revision: u64) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .latest_revision
            == revision
    }
}

/// Acknowledge only the exact notice that was successfully projected into the
/// current live attachment at the controller revision that carried it.
///
/// A detached/replaced window or a delayed projection cannot consume a notice
/// intended for the next foreground window.
#[must_use]
pub fn acknowledge_presented_notice<S>(
    controller: &mut DesktopController<S>,
    gate: &AttachmentGate,
    attachment: DesktopAttachment,
    presented_revision: Revision,
    notice_id: u64,
) -> bool
where
    S: Clone + PartialEq,
{
    if !gate.is_current(attachment, presented_revision.get())
        || controller.snapshot().revision != presented_revision
    {
        return false;
    }
    let Ok(outcome) = controller.dispatch(UiAction::AcknowledgeNotice { notice_id }) else {
        return false;
    };
    if outcome.changed {
        gate.post_revision(outcome.revision.get());
    }
    outcome.changed
}

/// Present one exact attachment revision, acknowledge its oldest notice only
/// after successful presentation, then rebuild and continue oldest-first.
/// The extra iteration presents the final notice-free projection.
#[must_use]
pub fn present_attached_projection_sequence<Present, Rebuild>(
    gate: &AttachmentGate,
    attachment: DesktopAttachment,
    mut projection: DesktopProjection,
    mut present: Present,
    mut rebuild_after_ack: Rebuild,
) -> usize
where
    Present: FnMut(&DesktopProjection) -> bool,
    Rebuild: FnMut(Revision, u64) -> Option<DesktopProjection>,
{
    let mut presented = 0;
    for _ in 0..=MAX_PENDING_NOTICES {
        if !gate.is_current(attachment, projection.revision.get()) || !present(&projection) {
            return presented;
        }
        presented += 1;
        let Some(notice_id) = projection.notice_id else {
            return presented;
        };
        let Some(next) = rebuild_after_ack(projection.revision, notice_id) else {
            return presented;
        };
        projection = next;
    }
    presented
}

struct DesktopRuntimeState {
    controller: DesktopController<()>,
    attachment: Option<(DesktopAttachment, slint::Weak<CliplineSpike>)>,
}

pub struct SlintDesktopAdapter {
    sender: UiEventSender,
    state: Arc<Mutex<DesktopRuntimeState>>,
    gate: AttachmentGate,
    consumer_alive: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl SlintDesktopAdapter {
    pub fn start_detached() -> Result<Self, String> {
        Self::start_inner(None)
    }

    pub fn start_with_tray(tray: slint::Weak<SpikeTray>) -> Result<Self, String> {
        Self::start_inner(Some(tray))
    }

    fn start_inner(tray: Option<slint::Weak<SpikeTray>>) -> Result<Self, String> {
        let (sender, receiver) = ui_event_channel();
        let consumer_alive = Arc::new(AtomicBool::new(true));
        let controller =
            DesktopController::new((), Vec::new()).map_err(|error| error.to_string())?;
        let initial_projection = DesktopProjection::from_snapshot(&controller.snapshot());
        if let Some(tray) = tray.as_ref().and_then(slint::Weak::upgrade) {
            apply_tray_projection(&tray, &initial_projection);
        }
        let state = Arc::new(Mutex::new(DesktopRuntimeState {
            controller,
            attachment: None,
        }));
        let gate = AttachmentGate::default();
        gate.post_revision(initial_projection.revision.get());
        let worker = spawn_consumer(
            receiver,
            Arc::clone(&state),
            gate.clone(),
            tray,
            Arc::clone(&consumer_alive),
        )?;
        Ok(Self {
            sender,
            state,
            gate,
            consumer_alive,
            worker: Some(worker),
        })
    }

    pub fn attach(
        &self,
        window: slint::Weak<CliplineSpike>,
    ) -> Result<DesktopAttachment, AttachmentError> {
        let Some(component) = window.upgrade() else {
            return Err(AttachmentError::StaleAttachment);
        };
        let (attachment, projection) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let projection = DesktopProjection::from_snapshot(&state.controller.snapshot());
            let attachment = self.gate.attach(projection.revision.get())?;
            state.attachment = Some((attachment, window));
            (attachment, projection)
        };
        let state = Arc::clone(&self.state);
        let _ = present_attached_projection_sequence(
            &self.gate,
            attachment,
            projection,
            |projection| {
                apply_projection(&component, projection);
                true
            },
            |presented_revision, notice_id| {
                let mut state = state.lock().unwrap_or_else(|poison| poison.into_inner());
                if state.attachment.as_ref().map(|current| current.0) != Some(attachment)
                    || !acknowledge_presented_notice(
                        &mut state.controller,
                        &self.gate,
                        attachment,
                        presented_revision,
                        notice_id,
                    )
                {
                    return None;
                }
                Some(DesktopProjection::from_snapshot(
                    &state.controller.snapshot(),
                ))
            },
        );
        Ok(attachment)
    }

    pub fn detach(&self, attachment: DesktopAttachment) -> Result<(), AttachmentError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if state.attachment.as_ref().map(|current| current.0) != Some(attachment) {
            return Err(AttachmentError::StaleAttachment);
        }
        self.gate.detach(attachment)?;
        state.attachment = None;
        Ok(())
    }

    #[must_use]
    pub fn projection(&self) -> DesktopProjection {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        DesktopProjection::from_snapshot(&state.controller.snapshot())
    }

    #[must_use]
    pub fn library_revision(&self) -> Revision {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .controller
            .library_revision()
    }

    pub fn try_publish(&self, event: UiEvent) -> Result<UiEventPublishOutcome, UiEventSendError> {
        self.sender.try_publish(event)
    }

    /// Cloneable process-owned producer used by native services whose work
    /// survives individual Slint window attachments.
    #[must_use]
    pub fn event_sender(&self) -> UiEventSender {
        self.sender.clone()
    }

    #[must_use]
    pub fn consumer_alive(&self) -> bool {
        self.consumer_alive.load(Ordering::Acquire)
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
    state: Arc<Mutex<DesktopRuntimeState>>,
    gate: AttachmentGate,
    tray: Option<slint::Weak<SpikeTray>>,
    consumer_alive: Arc<AtomicBool>,
) -> Result<JoinHandle<()>, String> {
    thread::Builder::new()
        .name("clipline-slint-desktop-events".to_owned())
        .spawn(move || {
            while consumer_alive.load(Ordering::Acquire) {
                let update = match receiver.wait_recv(Duration::from_millis(50)) {
                    Ok(Some(update)) => update,
                    Ok(None) => continue,
                    Err(_) => break,
                };
                let (projection, attachment) = {
                    let mut state = state.lock().unwrap_or_else(|poison| poison.into_inner());
                    let Ok(outcome) = state.controller.apply_event(update.event) else {
                        continue;
                    };
                    if !matches!(outcome, ApplyEventOutcome::Applied { .. }) {
                        continue;
                    }
                    let projection = DesktopProjection::from_snapshot(&state.controller.snapshot());
                    gate.post_revision(projection.revision.get());
                    (projection, state.attachment.clone())
                };
                let revision = projection.revision.get();
                let gate = gate.clone();
                let tray = tray.clone();
                let state = Arc::clone(&state);
                if slint::invoke_from_event_loop(move || {
                    if !gate.is_latest(revision) {
                        return;
                    }
                    if let Some(tray) = tray.and_then(|weak| weak.upgrade()) {
                        apply_tray_projection(&tray, &projection);
                    }
                    if let Some((attachment, weak)) = attachment {
                        if !gate.is_current(attachment, revision) {
                            return;
                        }
                        if let Some(window) = weak.upgrade() {
                            let _ = present_attached_projection_sequence(
                                &gate,
                                attachment,
                                projection,
                                |projection| {
                                    apply_projection(&window, projection);
                                    true
                                },
                                |presented_revision, notice_id| {
                                    let mut state =
                                        state.lock().unwrap_or_else(|poison| poison.into_inner());
                                    if state.attachment.as_ref().map(|current| current.0)
                                        != Some(attachment)
                                        || !acknowledge_presented_notice(
                                            &mut state.controller,
                                            &gate,
                                            attachment,
                                            presented_revision,
                                            notice_id,
                                        )
                                    {
                                        return None;
                                    }
                                    Some(DesktopProjection::from_snapshot(
                                        &state.controller.snapshot(),
                                    ))
                                },
                            );
                        }
                    }
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

fn apply_tray_projection(tray: &SpikeTray, projection: &DesktopProjection) {
    tray.set_recorder_state(projection.recorder_label.clone().into());
}

fn apply_projection(window: &CliplineSpike, projection: &DesktopProjection) {
    window.set_recorder_state(projection.recorder_label.clone().into());
    window.set_desktop_notice(projection.notice.clone().into());
    let uploads = projection
        .uploads
        .iter()
        .take(MAX_ACTIVE_UPLOADS)
        .map(|upload| DesktopUploadItem {
            local_clip_id: upload.local_clip_id.clone().into(),
            status: upload.status.clone().into(),
            progress: upload.progress.clone().into(),
        })
        .collect::<Vec<_>>();
    window.set_desktop_uploads(slint::ModelRc::new(slint::VecModel::from(uploads)));
}
