//! Framework-neutral desktop UI contract for Clipline.

mod action;
mod event;
mod snapshot;

pub use action::{UiAction, UiEffect};
pub use event::{
    CloudUploadProgress, EventPayloadError, GameDetection, MicMonitor, RecorderEvent, UiEvent,
    MAX_MIC_MONITOR_SAMPLES,
};
pub use snapshot::{
    Generation, GenerationError, Revision, WindowLifecycleMode, WindowLifecycleSnapshot,
};
