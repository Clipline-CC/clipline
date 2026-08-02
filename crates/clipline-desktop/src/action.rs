use serde::{Deserialize, Serialize};

use crate::WindowLifecycleMode;

/// An application-facing request from any desktop frontend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UiAction {
    SaveReplay,
    SetRecording { recording: bool },
    SetLifecycle { mode: WindowLifecycleMode },
    AcknowledgeNotice { notice_id: u64 },
}

/// A shell-independent effect produced by a [`UiAction`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiEffect {
    RequestSaveReplay,
    SetRecording { recording: bool },
    SetLifecycle { mode: WindowLifecycleMode },
    None,
}

impl UiAction {
    #[must_use]
    pub const fn effect(self) -> UiEffect {
        match self {
            Self::SaveReplay => UiEffect::RequestSaveReplay,
            Self::SetRecording { recording } => UiEffect::SetRecording { recording },
            Self::SetLifecycle { mode } => UiEffect::SetLifecycle { mode },
            Self::AcknowledgeNotice { .. } => UiEffect::None,
        }
    }
}
