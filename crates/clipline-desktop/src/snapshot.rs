use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{CloudUploadProgress, GameDetection, MicMonitor};

pub const MAX_CLOUD_ACCOUNT_KEY_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum GenerationError {
    #[error("generation is exhausted")]
    Exhausted,
}

macro_rules! checked_counter {
    ($name:ident) => {
        #[derive(
            Debug,
            Default,
            Clone,
            Copy,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            Serialize,
            Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(u64);

        impl $name {
            pub const INITIAL: Self = Self(0);

            #[must_use]
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            #[must_use]
            pub const fn get(self) -> u64 {
                self.0
            }

            pub const fn checked_next(self) -> Result<Self, GenerationError> {
                match self.0.checked_add(1) {
                    Some(value) => Ok(Self(value)),
                    None => Err(GenerationError::Exhausted),
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

checked_counter!(Generation);
checked_counter!(Revision);
checked_counter!(CloudAccountScope);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CloudAccountOwnerError {
    #[error("cloud account key must not be empty")]
    EmptyKey,
    #[error("cloud account key is {actual} bytes; maximum is {maximum}")]
    KeyTooLarge { actual: usize, maximum: usize },
}

/// Exact ownership fence for cloud work and desktop state.
///
/// The stable key prevents two different accounts at the same generation from
/// sharing state. The generation prevents a disconnected/reconnected account
/// from accepting work created by an earlier session with the same key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct CloudAccountOwner {
    account_key: String,
    account_generation: CloudAccountScope,
}

impl CloudAccountOwner {
    pub fn new(
        account_key: impl Into<String>,
        account_generation: CloudAccountScope,
    ) -> Result<Self, CloudAccountOwnerError> {
        let account_key = account_key.into();
        if account_key.trim().is_empty() {
            return Err(CloudAccountOwnerError::EmptyKey);
        }
        if account_key.len() > MAX_CLOUD_ACCOUNT_KEY_BYTES {
            return Err(CloudAccountOwnerError::KeyTooLarge {
                actual: account_key.len(),
                maximum: MAX_CLOUD_ACCOUNT_KEY_BYTES,
            });
        }
        Ok(Self {
            account_key,
            account_generation,
        })
    }

    #[must_use]
    pub fn account_key(&self) -> &str {
        &self.account_key
    }

    #[must_use]
    pub const fn account_generation(&self) -> CloudAccountScope {
        self.account_generation
    }
}

impl<'de> Deserialize<'de> for CloudAccountOwner {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireOwner {
            account_key: String,
            account_generation: CloudAccountScope,
        }

        let owner = WireOwner::deserialize(deserializer)?;
        Self::new(owner.account_key, owner.account_generation).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowLifecycleMode {
    Foreground,
    #[default]
    Tray,
    Taskbar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowLifecycleSnapshot {
    pub revision: Revision,
    pub mode: WindowLifecycleMode,
    pub backgrounded: bool,
}

impl WindowLifecycleSnapshot {
    #[must_use]
    pub const fn new(revision: Revision, mode: WindowLifecycleMode) -> Self {
        Self {
            revision,
            mode,
            backgrounded: !matches!(mode, WindowLifecycleMode::Foreground),
        }
    }
}

impl Default for WindowLifecycleSnapshot {
    fn default() -> Self {
        Self::new(Revision::INITIAL, WindowLifecycleMode::Tray)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecorderStatus {
    pub recording: bool,
    pub waiting_for_game: bool,
    pub segments: usize,
    pub buffered_s: f64,
    pub buffered_mb: f64,
    pub full_session: bool,
    pub encoder: String,
    pub capture_backend: String,
}

impl Default for RecorderStatus {
    fn default() -> Self {
        Self {
            recording: false,
            waiting_for_game: false,
            segments: 0,
            buffered_s: 0.0,
            buffered_mb: 0.0,
            full_session: false,
            encoder: String::new(),
            capture_backend: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecorderSnapshot {
    pub generation: Generation,
    pub desired: bool,
    pub status: RecorderStatus,
}

impl Default for RecorderSnapshot {
    fn default() -> Self {
        Self {
            generation: Generation::INITIAL,
            desired: false,
            status: RecorderStatus::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedReplay {
    pub path: String,
    pub seconds: f64,
    pub recording_start_unix: Option<i64>,
    pub recording_end_unix: Option<i64>,
    pub markers: usize,
    pub full_session: bool,
    pub gc_deleted: usize,
    pub gc_freed_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageStatus {
    pub total_bytes: u64,
    pub quota_bytes: Option<u64>,
    pub over_quota: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaRootSnapshot {
    pub path: String,
    pub fell_back: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameSnapshot {
    pub generation: Generation,
    pub detection: Option<GameDetection>,
}

impl Default for GameSnapshot {
    fn default() -> Self {
        Self {
            generation: Generation::INITIAL,
            detection: None,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MicrophonePhase {
    #[default]
    Stopped,
    Monitoring,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MicrophoneSnapshot {
    pub generation: Generation,
    pub phase: MicrophonePhase,
    pub monitor: Option<MicMonitor>,
    pub error: Option<String>,
}

impl Default for MicrophoneSnapshot {
    fn default() -> Self {
        Self {
            generation: Generation::INITIAL,
            phase: MicrophonePhase::Stopped,
            monitor: None,
            error: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudUploadSnapshot {
    pub account: CloudAccountOwner,
    pub generation: Generation,
    pub progress: CloudUploadProgress,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeKind {
    StartupWarning,
    Error,
    CloudUpload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notice {
    pub id: u64,
    pub kind: NoticeKind,
    pub message: String,
    pub created_revision: Revision,
    pub account: Option<CloudAccountOwner>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DesktopSnapshot<S> {
    pub schema_version: u32,
    pub revision: Revision,
    pub settings_revision: Revision,
    pub settings: S,
    pub lifecycle: WindowLifecycleSnapshot,
    pub recorder: RecorderSnapshot,
    pub storage: Option<StorageStatus>,
    pub media_root: Option<MediaRootSnapshot>,
    pub latest_saved: Option<SavedReplay>,
    pub game: GameSnapshot,
    pub microphone: MicrophoneSnapshot,
    pub cloud_account_generation: CloudAccountScope,
    pub current_cloud_account: Option<CloudAccountOwner>,
    pub uploads: Vec<CloudUploadSnapshot>,
    pub library_revision: Revision,
    pub enrichment_generation: Generation,
    pub notices: Vec<Notice>,
    pub notice_sequence: u64,
}
