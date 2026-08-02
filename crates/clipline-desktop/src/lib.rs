//! Framework-neutral desktop UI contract for Clipline.

mod action;
mod channel;
mod event;
mod snapshot;

pub use action::{UiAction, UiEffect};
pub use channel::{
    ui_event_channel, SequencedUiEvent, UiEventPublishOutcome, UiEventReceiveError,
    UiEventReceiver, UiEventSendError, UiEventSender, UiEventSink, UI_EVENT_CAPACITY,
};
pub use event::{
    CloudUploadProgress, EventPayloadError, GameDetection, MicMonitor, RecorderEvent, UiEvent,
    MAX_MIC_MONITOR_SAMPLES,
};
pub use snapshot::{
    Generation, GenerationError, Revision, WindowLifecycleMode, WindowLifecycleSnapshot,
};
