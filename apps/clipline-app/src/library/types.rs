use super::*;

pub struct StorageSettings {
    quota_bytes: Mutex<Option<u64>>,
    media_dir: Mutex<PathBuf>,
}

#[derive(Clone, Default)]
pub struct ClipboardExportState {
    generation: Arc<AtomicU64>,
}

impl ClipboardExportState {
    pub(crate) fn begin(&self) -> ClipboardExportJob {
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        ClipboardExportJob {
            generation,
            current: Arc::clone(&self.generation),
        }
    }

    pub fn cancel(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }
}

pub(crate) struct ClipboardExportJob {
    pub(crate) generation: u64,
    pub(crate) current: Arc<AtomicU64>,
}

impl ClipboardExportJob {
    pub(crate) fn is_cancelled(&self) -> bool {
        self.current.load(Ordering::Acquire) != self.generation
    }

    pub(crate) fn ensure_active(&self) -> Result<(), String> {
        if self.is_cancelled() {
            Err("shareable clipboard export cancelled".into())
        } else {
            Ok(())
        }
    }
}

impl StorageSettings {
    pub fn new(quota_bytes: Option<u64>, media_dir: PathBuf) -> Self {
        Self {
            quota_bytes: Mutex::new(quota_bytes),
            media_dir: Mutex::new(media_dir),
        }
    }

    pub fn quota_bytes(&self) -> Option<u64> {
        match self.quota_bytes.lock() {
            Ok(q) => *q,
            Err(e) => {
                tracing::error!(event = "storage_quota_lock_poisoned", error = %e);
                None
            }
        }
    }

    pub fn set_quota_bytes(&self, quota_bytes: Option<u64>) {
        match self.quota_bytes.lock() {
            Ok(mut q) => *q = quota_bytes,
            Err(e) => tracing::error!(event = "storage_quota_set_lock_poisoned", error = %e),
        }
    }

    pub fn media_dir(&self) -> PathBuf {
        match self.media_dir.lock() {
            Ok(dir) => dir.clone(),
            Err(e) => {
                tracing::error!(event = "media_directory_lock_poisoned", error = %e);
                default_clips_dir()
            }
        }
    }

    pub fn set_media_dir(&self, media_dir: PathBuf) {
        match self.media_dir.lock() {
            Ok(mut dir) => *dir = media_dir,
            Err(e) => tracing::error!(event = "media_directory_set_lock_poisoned", error = %e),
        }
    }

    pub(crate) fn clips_dir(&self) -> Result<PathBuf, String> {
        clips_dir(&self.media_dir())
    }
}

/// The game a clip's session folder is attributed to (see
/// `clipline-session.json`). Drives the library's per-clip game icon.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct ClipGame {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<clipline_lol::LeagueQueue>,
}

#[derive(serde::Serialize)]
pub struct ClipInfo {
    pub path: String,
    pub name: String,
    pub title: Option<String>,
    pub kind: String,
    /// User favorite; favorites are never auto-deleted by quota GC.
    pub favorite: bool,
    /// Session folder name; None for legacy clips at the library root.
    pub session: Option<String>,
    pub size_mb: f64,
    pub modified_unix: u64,
    pub duration_s: Option<f64>,
    pub markers: Option<ClipMarkers>,
    /// Game this clip's session belongs to, if recorded under a detected game.
    pub game: Option<ClipGame>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<ClipGroup>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_group: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_group_fingerprint: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ClipGroup {
    pub name: String,
    pub order: u32,
}

#[derive(serde::Serialize)]
pub struct LocalClipScan {
    pub clips: Vec<ClipInfo>,
    pub warnings: Vec<String>,
}

#[derive(serde::Serialize)]
pub struct StorageInfo {
    pub clip_count: usize,
    pub total_bytes: u64,
    pub quota_bytes: Option<u64>,
    pub over_quota: bool,
}

#[derive(serde::Serialize)]
pub struct ExportedClipInfo {
    pub path: String,
    pub name: String,
    pub size_mb: f64,
    pub modified_unix: u64,
    pub requested_start_s: f64,
    pub requested_end_s: f64,
    pub aligned_start_s: f64,
    pub aligned_end_s: f64,
    pub duration_s: f64,
    pub markers: Option<ClipMarkers>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<ClipGroup>,
}

#[derive(serde::Serialize)]
pub struct RenamedClipInfo {
    pub old_path: String,
    pub path: String,
    pub name: String,
    pub title: Option<String>,
    pub kind: String,
}

#[derive(serde::Serialize)]
pub struct SetClipFavoriteInfo {
    pub path: String,
    pub favorite: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ShareAudioExportMode {
    Remux(Vec<u32>),
    Mix(Vec<u32>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ShareVideoExportMode {
    Copy,
    Encode {
        encoder: String,
        backend: EncoderBackend,
    },
}

pub(crate) const SHARE_H264_BITRATE_BPS: u32 = 8_000_000;
pub(crate) const SHARE_H264_BUFSIZE_BITS: u64 = 16_000_000;

#[cfg(test)]
mod tests {
    use super::*;
        #[test]
        fn clipboard_export_generation_cancels_only_existing_jobs() {
            let state = ClipboardExportState::default();
            let first = state.begin();
            assert!(!first.is_cancelled());

            state.cancel();
            assert!(first.is_cancelled());

            let second = state.begin();
            assert!(!second.is_cancelled());
            state.cancel();
            assert!(second.is_cancelled());
        }
}
