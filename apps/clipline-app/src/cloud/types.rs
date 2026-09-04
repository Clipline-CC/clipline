//! Shared Cloud request/response types and small cross-module value types.
use super::*;

#[derive(Debug, Deserialize)]
pub struct CloudConnectRequest {
    pub host_url: String,
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub device_name: Option<String>,
    #[serde(default)]
    pub plain_http_confirmed: bool,
    #[serde(default)]
    pub default_visibility: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UploadClipCommandRequest {
    pub path: String,
    #[serde(default)]
    pub visibility: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "audioTrackIds")]
    pub audio_track_ids: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct SyncCloudClipStatusRequest {
    pub path: String,
}

#[derive(Debug, Serialize)]
pub struct CloudConnectionStatus {
    pub connected: bool,
    pub token_present: bool,
    pub host_url: String,
    pub public_url: Option<String>,
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub user_id: Option<String>,
    pub default_visibility: String,
    pub delete_local_after_upload: bool,
    pub auto_upload_rules: bool,
}

#[derive(Debug, Serialize, Clone)]
pub struct CloudUserProfile {
    pub user_id: String,
    pub username: String,
    pub display_name: Option<String>,
    pub profile_url: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct CloudUploadProgressEvent {
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

#[derive(Debug, Serialize)]
pub struct CloudUploadResult {
    pub record: CloudUploadRecord,
    pub clip: Option<ClipDetailResponse>,
    pub local_deleted: bool,
}

#[derive(Debug)]
pub(crate) enum ReadyClipOutcome {
    Ready(ClipDetailResponse),
    Failed(ClipDetailResponse),
    TimedOut,
}

#[derive(Debug, Serialize)]
pub struct CloudClipStatusSyncResult {
    pub path: String,
    pub record: Option<CloudUploadRecord>,
    pub removed: bool,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct CloudLibraryClip {
    pub remote_clip_id: String,
    pub local_clip_id: Option<String>,
    pub path: String,
    pub title: String,
    pub remote_url: String,
    pub visibility: String,
    pub upload_status: String,
    pub updated_at_unix: u64,
    pub uploaded_at_unix: Option<u64>,
    pub duration_ms: Option<i64>,
    pub file_size_bytes: Option<i64>,
    pub source_type: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CloudLibraryListResult {
    pub clips: Vec<CloudLibraryClip>,
    pub truncated: bool,
}

#[derive(Debug, Deserialize)]
pub struct CloudClipAssetRequest {
    pub remote_clip_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub duration_ms: Option<i64>,
    #[serde(default)]
    pub file_size_bytes: Option<i64>,
    #[serde(default)]
    pub updated_at_unix: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct CachedCloudClip {
    pub path: String,
    pub name: String,
    pub size_mb: f64,
    pub modified_unix: u64,
    pub duration_s: Option<f64>,
}

pub(crate) struct CloudAssetDownload<'a> {
    pub(crate) remote_clip_id: &'a str,
    pub(crate) asset: &'a str,
    pub(crate) extension: &'a str,
    pub(crate) version: Option<u64>,
    pub(crate) expected_size_bytes: Option<i64>,
    pub(crate) max_size_bytes: u64,
    pub(crate) missing_ok: bool,
}

#[derive(Clone)]
pub(crate) struct CachedCloudUserAvatar {
    pub(crate) key: String,
    pub(crate) etag: Option<String>,
    pub(crate) data_url: String,
}

pub(crate) struct OwnedCloudCacheTemp {
    path: PathBuf,
    armed: bool,
}

impl OwnedCloudCacheTemp {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for OwnedCloudCacheTemp {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MissingRemoteSyncAction {
    Keep,
    ConfirmMissing,
    Remove,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_progress_omits_absent_share_url() {
        let event = CloudUploadProgressEvent {
            local_clip_id: "local-1".into(),
            path: "D:\\Videos\\clip.mp4".into(),
            upload_status: "processing".into(),
            received_size_bytes: 10,
            file_size_bytes: 20,
            remote_clip_id: Some("remote-1".into()),
            remote_url: None,
            error: None,
        };

        let serialized = serde_json::to_value(event).expect("serialize upload progress");

        assert!(
            serialized.get("remote_url").is_none(),
            "an absent share URL must not erase a previously refreshed URL"
        );
    }

    #[test]
    fn cloud_upload_result_serializes_confirmed_local_deletion() {
        let result = CloudUploadResult {
            record: upload_record("local", "clip.mp4", "uploaded_private", 1),
            clip: None,
            local_deleted: true,
        };

        let value = serde_json::to_value(result).unwrap();
        assert_eq!(value["local_deleted"], true);
    }

}