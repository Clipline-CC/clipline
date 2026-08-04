use serde::{Deserialize, Serialize};

use crate::{ProbeToken, WindowLifecycleMode};

/// An application-facing request from any desktop frontend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MicrophoneMonitorOutput {
    TauriCompatibilityPcm,
    NativeRenderer,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MicrophoneMonitorRequest {
    pub device_id: Option<String>,
    pub volume: f32,
    pub mono: bool,
    pub output: MicrophoneMonitorOutput,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UiAction {
    SaveReplay,
    SetRecording { recording: bool },
    SetLifecycle { mode: WindowLifecycleMode },
    RequestSettingsProbe { token: ProbeToken },
    StartMicrophoneMonitor { request: MicrophoneMonitorRequest },
    StopMicrophoneMonitor,
    AcknowledgeNotice { notice_id: u64 },
}

/// A shell-independent effect produced by a [`UiAction`].
#[derive(Debug, Clone, PartialEq)]
pub enum UiEffect {
    RequestSaveReplay,
    SetRecording { recording: bool },
    SetLifecycle { mode: WindowLifecycleMode },
    RequestSettingsProbe { token: ProbeToken },
    StartMicrophoneMonitor { request: MicrophoneMonitorRequest },
    StopMicrophoneMonitor,
    None,
}

impl UiAction {
    #[must_use]
    pub fn effect(&self) -> UiEffect {
        match self {
            Self::SaveReplay => UiEffect::RequestSaveReplay,
            Self::SetRecording { recording } => UiEffect::SetRecording {
                recording: *recording,
            },
            Self::SetLifecycle { mode } => UiEffect::SetLifecycle { mode: *mode },
            Self::RequestSettingsProbe { token } => {
                UiEffect::RequestSettingsProbe { token: *token }
            }
            Self::StartMicrophoneMonitor { request } => UiEffect::StartMicrophoneMonitor {
                request: request.clone(),
            },
            Self::StopMicrophoneMonitor => UiEffect::StopMicrophoneMonitor,
            Self::AcknowledgeNotice { .. } => UiEffect::None,
        }
    }
}
