//! Framework-neutral desktop UI contract for Clipline.

mod action;
mod channel;
mod controller;
mod event;
mod snapshot;

pub use action::{UiAction, UiEffect};
pub use channel::{
    ui_event_channel, SequencedUiEvent, UiEventPublishOutcome, UiEventReceiveError,
    UiEventReceiver, UiEventSendError, UiEventSender, UiEventSink, UI_EVENT_CAPACITY,
};
pub use controller::{
    ApplyEventOutcome, ControllerError, DesktopController, DispatchOutcome, MAX_ACTIVE_UPLOADS,
    MAX_PENDING_NOTICES,
};
pub use event::{
    CloudUploadProgress, EventPayloadError, GameDetection, MicMonitor, RecorderEvent, UiEvent,
    MAX_MIC_MONITOR_SAMPLES,
};
pub use snapshot::{
    CloudAccountScope, CloudUploadSnapshot, DesktopSnapshot, GameSnapshot, Generation,
    GenerationError, MediaRootSnapshot, MicrophonePhase, MicrophoneSnapshot, Notice, NoticeKind,
    RecorderSnapshot, RecorderStatus, Revision, SavedReplay, StorageStatus, WindowLifecycleMode,
    WindowLifecycleSnapshot,
};
