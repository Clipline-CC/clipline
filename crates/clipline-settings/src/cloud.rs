//! Clipline Cloud connection settings and per-clip upload records.
//! Normalization trims/repairs hand-edited values; validation enforces the
//! enumerated visibility/upload-status strings so the frontend never sees
//! a malformed state.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const CLOUD_CREDENTIAL_PREFIX: &str = "Clipline Cloud";
/// Upload identities share the neutral Library contract's 16 KiB identity bound.
pub const MAX_CLOUD_UPLOAD_ID_BYTES: usize = 16 * 1024;
/// Paths are user-controlled and may include a long Windows verbatim prefix.
pub const MAX_CLOUD_UPLOAD_PATH_BYTES: usize = 1024 * 1024;
/// Remote URLs are persisted only as bounded display/navigation metadata.
pub const MAX_CLOUD_UPLOAD_URL_BYTES: usize = 64 * 1024;
/// Transport errors are bounded before they enter durable settings.
pub const MAX_CLOUD_UPLOAD_ERROR_BYTES: usize = 64 * 1024;

pub fn cloud_credential_target(host_url: &str, user_id: &str) -> String {
    format!(
        "{CLOUD_CREDENTIAL_PREFIX}:{}:{}",
        host_url.trim().trim_end_matches('/'),
        user_id.trim()
    )
}

fn default_cloud_visibility() -> String {
    "private".to_string()
}

fn default_upload_status() -> String {
    "not_uploaded".to_string()
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CloudUploadRecord {
    pub local_clip_id: String,
    /// Stable server-facing identity of the prepared payload. Legacy records
    /// predate the split between source admission and payload identity.
    #[serde(default)]
    pub client_clip_id: Option<String>,
    /// Exact durable upload owner. `None` identifies a legacy record that has
    /// not yet been advanced through the account-safe upload service.
    #[serde(default)]
    pub upload_generation: Option<u64>,
    pub path: String,
    #[serde(default)]
    pub remote_clip_id: Option<String>,
    #[serde(default)]
    pub remote_url: Option<String>,
    #[serde(default = "default_cloud_visibility")]
    pub visibility: String,
    #[serde(default = "default_upload_status")]
    pub upload_status: String,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub updated_at_unix: u64,
}

impl CloudUploadRecord {
    pub fn normalize(&mut self) {
        self.local_clip_id = self.local_clip_id.trim().to_string();
        self.client_clip_id = self
            .client_clip_id
            .take()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        self.path = self.path.trim().to_string();
        self.remote_clip_id = self
            .remote_clip_id
            .take()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        self.remote_url = self
            .remote_url
            .take()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        self.visibility = normalize_cloud_visibility(&self.visibility);
        self.upload_status = normalize_upload_status(&self.upload_status);
        self.error = self
            .error
            .take()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        clear_non_shareable_remote_url(self);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CloudSettings {
    #[serde(default)]
    pub host_url: String,
    #[serde(default)]
    pub public_url: Option<String>,
    #[serde(default)]
    pub connected_user_id: Option<String>,
    #[serde(default)]
    pub connected_username: Option<String>,
    #[serde(default)]
    pub connected_display_name: Option<String>,
    #[serde(default)]
    pub credential_target: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credential_cleanup_targets: Vec<String>,
    #[serde(default = "default_cloud_visibility")]
    pub default_visibility: String,
    #[serde(default)]
    pub delete_local_after_upload: bool,
    #[serde(default)]
    pub auto_upload_rules: bool,
    /// Highest durable upload generation admitted by this settings profile.
    /// The profile-wide scalar survives record and account replacement so a
    /// process restart cannot recreate an earlier durable upload token.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub upload_generation_sequence: u64,
    #[serde(default)]
    pub uploads: BTreeMap<String, CloudUploadRecord>,
}

impl Default for CloudSettings {
    fn default() -> Self {
        Self {
            host_url: String::new(),
            public_url: None,
            connected_user_id: None,
            connected_username: None,
            connected_display_name: None,
            credential_target: None,
            credential_cleanup_targets: Vec::new(),
            default_visibility: default_cloud_visibility(),
            delete_local_after_upload: false,
            auto_upload_rules: false,
            upload_generation_sequence: 0,
            uploads: BTreeMap::new(),
        }
    }
}

impl CloudSettings {
    pub fn connected(&self) -> bool {
        !self.host_url.trim().is_empty()
            && self.connected_user_id.is_some()
            && self.credential_target.is_some()
    }

    pub fn normalize(&mut self) {
        self.host_url = self.host_url.trim().trim_end_matches('/').to_string();
        self.public_url = self
            .public_url
            .take()
            .map(|value| value.trim().trim_end_matches('/').to_string())
            .filter(|value| !value.is_empty());
        self.connected_user_id = self
            .connected_user_id
            .take()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        self.connected_username = self
            .connected_username
            .take()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        self.connected_display_name = self
            .connected_display_name
            .take()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        self.credential_target = self
            .credential_target
            .take()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        self.credential_cleanup_targets = std::mem::take(&mut self.credential_cleanup_targets)
            .into_iter()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .collect();
        self.credential_cleanup_targets.sort();
        self.credential_cleanup_targets.dedup();
        self.default_visibility = normalize_cloud_visibility(&self.default_visibility);
        self.uploads = std::mem::take(&mut self.uploads)
            .into_iter()
            .filter_map(|(key, mut record)| {
                record.normalize();
                (!record.local_clip_id.is_empty())
                    .then(|| (normalize_cloud_upload_key(&key, &record), record))
            })
            .collect();
        self.upload_generation_sequence = self
            .uploads
            .values()
            .filter_map(|record| record.upload_generation)
            .fold(self.upload_generation_sequence, u64::max);
    }

    pub fn validate(&self) -> Result<(), String> {
        validate_cloud_visibility(&self.default_visibility)?;
        for (key, record) in &self.uploads {
            validate_bounded_required("cloud upload key", key, MAX_CLOUD_UPLOAD_ID_BYTES)?;
            validate_cloud_visibility(&record.visibility)?;
            validate_upload_status(&record.upload_status)?;
            validate_bounded_required(
                "cloud upload record local_clip_id",
                &record.local_clip_id,
                MAX_CLOUD_UPLOAD_ID_BYTES,
            )?;
            validate_bounded_optional(
                "cloud upload record client_clip_id",
                record.client_clip_id.as_deref(),
                MAX_CLOUD_UPLOAD_ID_BYTES,
            )?;
            validate_bounded_optional(
                "cloud upload record remote_clip_id",
                record.remote_clip_id.as_deref(),
                MAX_CLOUD_UPLOAD_ID_BYTES,
            )?;
            validate_bounded_optional(
                "cloud upload record remote_url",
                record.remote_url.as_deref(),
                MAX_CLOUD_UPLOAD_URL_BYTES,
            )?;
            validate_bounded_optional(
                "cloud upload record error",
                record.error.as_deref(),
                MAX_CLOUD_UPLOAD_ERROR_BYTES,
            )?;
            if record.path.len() > MAX_CLOUD_UPLOAD_PATH_BYTES {
                return Err(format!(
                    "cloud upload record path is {} bytes; maximum is {MAX_CLOUD_UPLOAD_PATH_BYTES}",
                    record.path.len()
                ));
            }
        }
        if self
            .uploads
            .values()
            .filter_map(|record| record.upload_generation)
            .any(|generation| generation > self.upload_generation_sequence)
        {
            return Err("cloud upload generation sequence is older than a durable record".into());
        }
        Ok(())
    }
}

fn validate_bounded_required(name: &str, value: &str, maximum: usize) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{name} is missing"));
    }
    if value.len() > maximum {
        return Err(format!(
            "{name} is {} bytes; maximum is {maximum}",
            value.len()
        ));
    }
    Ok(())
}

fn validate_bounded_optional(
    name: &str,
    value: Option<&str>,
    maximum: usize,
) -> Result<(), String> {
    let Some(value) = value else {
        return Ok(());
    };
    validate_bounded_required(name, value, maximum)
}

fn normalize_cloud_upload_key(key: &str, record: &CloudUploadRecord) -> String {
    let key = key.trim();
    if key.is_empty() {
        record.local_clip_id.clone()
    } else {
        key.to_string()
    }
}

fn clear_non_shareable_remote_url(record: &mut CloudUploadRecord) {
    if record.visibility == "private" {
        record.remote_url = None;
        return;
    }
    let (Some(remote_clip_id), Some(remote_url)) = (
        record.remote_clip_id.as_deref(),
        record.remote_url.as_deref(),
    ) else {
        return;
    };
    if remote_url
        .trim_end_matches('/')
        .ends_with(&format!("/clip/{remote_clip_id}"))
    {
        record.remote_url = None;
    }
}

pub fn normalize_cloud_visibility(value: &str) -> String {
    match value {
        "public" => "public".to_string(),
        "unlisted" => "unlisted".to_string(),
        _ => "private".to_string(),
    }
}

fn validate_cloud_visibility(value: &str) -> Result<(), String> {
    match value {
        "private" | "public" | "unlisted" => Ok(()),
        _ => Err("cloud visibility must be private, public, or unlisted".into()),
    }
}

fn normalize_upload_status(value: &str) -> String {
    match value {
        "queued"
        | "uploading"
        | "processing"
        | "uploaded_private"
        | "uploaded_public"
        | "uploaded_processing"
        | "failed"
        | "retrying"
        | "canceled" => value.to_string(),
        _ => default_upload_status(),
    }
}

fn validate_upload_status(value: &str) -> Result<(), String> {
    match value {
        "not_uploaded"
        | "queued"
        | "uploading"
        | "processing"
        | "uploaded_private"
        | "uploaded_public"
        | "uploaded_processing"
        | "failed"
        | "retrying"
        | "canceled" => Ok(()),
        _ => Err("cloud upload status is invalid".into()),
    }
}
