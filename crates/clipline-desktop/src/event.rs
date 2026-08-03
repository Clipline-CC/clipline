use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{CloudAccountOwner, Generation, WindowLifecycleSnapshot};

pub const MAX_MIC_MONITOR_SAMPLES: usize = 4_096;

#[derive(Debug, Clone, PartialEq, Error)]
pub enum EventPayloadError {
    #[error("microphone levels must be finite and non-negative")]
    InvalidMicrophoneLevel,
    #[error("microphone sample count {actual} exceeds bound {maximum}")]
    MicrophoneSamplesTooLarge { actual: usize, maximum: usize },
}

/// Recorder payload kept JSON-compatible with the shipping Tauri events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecorderEvent {
    MediaRootResolved {
        path: String,
        fell_back: bool,
    },
    Status {
        recording: bool,
        #[serde(default)]
        waiting_for_game: bool,
        segments: usize,
        buffered_s: f64,
        buffered_mb: f64,
        #[serde(default)]
        full_session: bool,
        #[serde(default)]
        encoder: String,
        #[serde(default)]
        capture_backend: String,
    },
    Saved {
        path: String,
        seconds: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recording_start_unix: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recording_end_unix: Option<i64>,
        markers: usize,
        #[serde(default)]
        full_session: bool,
        gc_deleted: usize,
        gc_freed_bytes: u64,
        storage_total_bytes: u64,
        storage_quota_bytes: Option<u64>,
        storage_over_quota: bool,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MicMonitor {
    pub rms: f32,
    pub peak: f32,
    pub sample_count: usize,
    pub samples: Vec<i16>,
}

impl MicMonitor {
    pub fn new(rms: f32, peak: f32, samples: Vec<i16>) -> Result<Self, EventPayloadError> {
        let sample_count = samples.len();
        Self::from_parts(rms, peak, sample_count, samples)
    }

    pub fn from_parts(
        rms: f32,
        peak: f32,
        sample_count: usize,
        samples: Vec<i16>,
    ) -> Result<Self, EventPayloadError> {
        if !rms.is_finite() || !peak.is_finite() || rms < 0.0 || peak < 0.0 {
            return Err(EventPayloadError::InvalidMicrophoneLevel);
        }
        if samples.len() > MAX_MIC_MONITOR_SAMPLES {
            return Err(EventPayloadError::MicrophoneSamplesTooLarge {
                actual: samples.len(),
                maximum: MAX_MIC_MONITOR_SAMPLES,
            });
        }
        Ok(Self {
            rms,
            peak,
            sample_count,
            samples,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameDetection {
    pub active: bool,
    pub name: Option<String>,
    pub window_title: Option<String>,
    pub process_id: Option<u32>,
    pub process_instance_id: Option<String>,
    pub exe_name: Option<String>,
    pub recording_mode: Option<String>,
    pub elevated_hotkeys_blocked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudUploadProgress {
    pub local_clip_id: String,
    pub path: String,
    pub upload_status: String,
    pub received_size_bytes: u64,
    pub file_size_bytes: u64,
    pub remote_clip_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
    pub error: Option<String>,
}

/// Byte-only updates are coalescable and may only change byte counters.
/// State updates are durable barriers and may replace status/identity/error
/// fields and optionally create foreground feedback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloudUploadUpdateKind {
    Bytes,
    State,
}

/// An owned update published by an application producer to any desktop frontend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UiEvent {
    Recorder {
        generation: Generation,
        event: RecorderEvent,
    },
    WindowLifecycle {
        snapshot: WindowLifecycleSnapshot,
    },
    MicMonitor {
        generation: Generation,
        monitor: MicMonitor,
    },
    MicTestError {
        generation: Generation,
        message: String,
    },
    MicTestStopped {
        generation: Generation,
    },
    GameDetection {
        generation: Generation,
        detection: GameDetection,
    },
    CloudAccountChanged {
        account: Option<CloudAccountOwner>,
    },
    CloudUploadProgress {
        account: CloudAccountOwner,
        generation: Generation,
        update: CloudUploadUpdateKind,
        progress: CloudUploadProgress,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        notice: Option<String>,
    },
    EnrichmentUpdated {
        generation: Generation,
    },
    UserError {
        message: String,
    },
}

impl UiEvent {
    #[must_use]
    pub const fn generation(&self) -> Option<Generation> {
        match self {
            Self::Recorder { generation, .. }
            | Self::MicMonitor { generation, .. }
            | Self::MicTestError { generation, .. }
            | Self::MicTestStopped { generation }
            | Self::GameDetection { generation, .. }
            | Self::CloudUploadProgress { generation, .. }
            | Self::EnrichmentUpdated { generation } => Some(*generation),
            Self::WindowLifecycle { .. }
            | Self::CloudAccountChanged { .. }
            | Self::UserError { .. } => None,
        }
    }
}
