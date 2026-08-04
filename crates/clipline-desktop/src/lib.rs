//! Framework-neutral desktop UI contract for Clipline.

pub use clipline_settings::{
    ProbeKind, ProbePhase, ProbeRequestGeneration, ProbeSessionOwner, ProbeSummary, ProbeToken,
    SettingsAttachmentGeneration, SettingsForegroundGeneration, SettingsSessionGeneration,
};

mod action;
mod channel;
mod controller;
mod event;
mod snapshot;

pub use action::{MicrophoneMonitorOutput, MicrophoneMonitorRequest, UiAction, UiEffect};
pub use channel::{
    ui_event_channel, SequencedUiEvent, UiEventPublishOutcome, UiEventReceiveError,
    UiEventReceiver, UiEventSendError, UiEventSender, UiEventSink, UI_EVENT_CAPACITY,
    UI_EVENT_MAX_BUFFERED,
};
pub use controller::{
    ApplyEventOutcome, ControllerError, DesktopController, DispatchOutcome,
    DESKTOP_SNAPSHOT_SCHEMA_VERSION, MAX_ACTIVE_UPLOADS, MAX_NOTICE_MESSAGE_BYTES,
    MAX_PENDING_NOTICES, MAX_SETTINGS_PROBE_SUMMARIES,
};
pub use event::{
    CloudUploadProgress, CloudUploadUpdateKind, EventPayloadError, GameDetection, MicMonitor,
    RecorderEvent, UiEvent, MAX_MIC_MONITOR_SAMPLES,
};
pub use snapshot::{
    CatalogSummarySnapshot, CatalogSummarySource, CloudAccountOwner, CloudAccountOwnerError,
    CloudAccountScope, CloudUploadSnapshot, DesktopSnapshot, GameSnapshot, Generation,
    GenerationError, MediaRootSnapshot, MicrophonePhase, MicrophoneSnapshot, Notice, NoticeKind,
    RecorderSnapshot, RecorderStatus, Revision, SavedReplay, StorageStatus, WindowLifecycleMode,
    WindowLifecycleSnapshot, MAX_CLOUD_ACCOUNT_KEY_BYTES,
};
