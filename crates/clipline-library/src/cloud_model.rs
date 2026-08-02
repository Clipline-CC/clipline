use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{
    CloudAccountKey, CloudLibraryItem, IdentityError, PayloadBoundsError, RequestGeneration,
    MAX_CATALOG_STRING_BYTES, MAX_CLOUD_INDEX_ROWS,
};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudAccountFields {
    pub host_url: String,
    pub connected_user_id: String,
    pub credential_target: String,
}

pub fn account_key(fields: &CloudAccountFields) -> Result<CloudAccountKey, IdentityError> {
    CloudAccountKey::new(format!(
        "{}|{}|{}",
        fields.host_url, fields.connected_user_id, fields.credential_target
    ))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudRequest {
    pub generation: RequestGeneration,
    pub account_key: CloudAccountKey,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CloudRequestGate {
    generation: RequestGeneration,
}

impl CloudRequestGate {
    pub fn begin(
        &mut self,
        account_key: CloudAccountKey,
    ) -> Result<CloudRequest, crate::GenerationError> {
        self.generation = self.generation.checked_next()?;
        Ok(CloudRequest {
            generation: self.generation,
            account_key,
        })
    }

    pub fn invalidate(&mut self) -> Result<RequestGeneration, crate::GenerationError> {
        self.generation = self.generation.checked_next()?;
        Ok(self.generation)
    }

    #[must_use]
    pub fn is_current(&self, request: &CloudRequest, account_key: &CloudAccountKey) -> bool {
        request.generation == self.generation && &request.account_key == account_key
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudUploadRecord {
    pub local_clip_id: String,
    pub path: String,
    pub remote_clip_id: Option<String>,
    pub remote_url: Option<String>,
    pub visibility: String,
    pub upload_status: String,
    pub error: Option<String>,
    pub updated_at_unix: u64,
}

impl CloudUploadRecord {
    fn validate(&self) -> Result<(), PayloadBoundsError> {
        check_string("upload.local_clip_id", &self.local_clip_id)?;
        check_string("upload.path", &self.path)?;
        check_optional_string("upload.remote_clip_id", self.remote_clip_id.as_deref())?;
        check_optional_string("upload.remote_url", self.remote_url.as_deref())?;
        check_string("upload.visibility", &self.visibility)?;
        check_string("upload.upload_status", &self.upload_status)?;
        check_optional_string("upload.error", self.error.as_deref())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudSettingsModel {
    pub host_url: Option<String>,
    pub public_url: Option<String>,
    pub connected_user_id: Option<String>,
    pub connected_username: Option<String>,
    pub connected_display_name: Option<String>,
    pub credential_target: Option<String>,
    pub default_visibility: String,
    pub delete_local_after_upload: bool,
    pub auto_upload_rules: bool,
    pub uploads: BTreeMap<String, CloudUploadRecord>,
}

impl CloudSettingsModel {
    /// Replaces only fields owned by the backend. The caller retains the rest of
    /// the whole settings document and all draft-only Cloud preferences.
    pub fn merge_backend_owned(&mut self, backend: &Self) -> Result<(), PayloadBoundsError> {
        backend.validate_backend_owned()?;
        self.host_url.clone_from(&backend.host_url);
        self.public_url.clone_from(&backend.public_url);
        self.connected_user_id
            .clone_from(&backend.connected_user_id);
        self.connected_username
            .clone_from(&backend.connected_username);
        self.connected_display_name
            .clone_from(&backend.connected_display_name);
        self.credential_target
            .clone_from(&backend.credential_target);
        self.uploads.clone_from(&backend.uploads);
        Ok(())
    }

    fn validate_backend_owned(&self) -> Result<(), PayloadBoundsError> {
        check_optional_string("cloud.host_url", self.host_url.as_deref())?;
        check_optional_string("cloud.public_url", self.public_url.as_deref())?;
        check_optional_string("cloud.connected_user_id", self.connected_user_id.as_deref())?;
        check_optional_string(
            "cloud.connected_username",
            self.connected_username.as_deref(),
        )?;
        check_optional_string(
            "cloud.connected_display_name",
            self.connected_display_name.as_deref(),
        )?;
        check_optional_string("cloud.credential_target", self.credential_target.as_deref())?;
        check_count("cloud.uploads", self.uploads.len(), MAX_CLOUD_INDEX_ROWS)?;
        for (key, record) in &self.uploads {
            check_string("cloud.upload_key", key)?;
            record.validate()?;
        }
        Ok(())
    }
}

#[must_use]
pub fn plain_http_confirmed(active_origin: &str, confirmed_origin: &str, checked: bool) -> bool {
    !active_origin.is_empty() && checked && active_origin == confirmed_origin
}

#[must_use]
pub fn record_uploaded(record: &CloudUploadRecord) -> bool {
    record
        .remote_clip_id
        .as_ref()
        .is_some_and(|remote_id| !remote_id.is_empty())
        && record.upload_status.starts_with("uploaded_")
}

#[must_use]
pub fn share_url(record: &CloudUploadRecord) -> &str {
    if record_uploaded(record) && record.visibility != "private" {
        record.remote_url.as_deref().unwrap_or_default()
    } else {
        ""
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum NullablePatch<T> {
    #[default]
    Unchanged,
    Value(Option<T>),
}

impl<T> Serialize for NullablePatch<T>
where
    T: Serialize,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            // Fields skip this variant. Serializing it directly still has a
            // deterministic nullable representation rather than an enum tag.
            Self::Unchanged => serializer.serialize_none(),
            Self::Value(value) => value.serialize(serializer),
        }
    }
}

impl<T> NullablePatch<T> {
    #[must_use]
    pub const fn is_unchanged(&self) -> bool {
        matches!(self, Self::Unchanged)
    }

    fn apply_to(&self, current: &Option<T>) -> Option<T>
    where
        T: Clone,
    {
        match self {
            Self::Unchanged => current.clone(),
            Self::Value(value) => value.clone(),
        }
    }
}

impl<'de, T> Deserialize<'de> for NullablePatch<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Option::<T>::deserialize(deserializer).map(Self::Value)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadProgressPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_clip_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "NullablePatch::is_unchanged")]
    pub remote_clip_id: NullablePatch<String>,
    #[serde(default, skip_serializing_if = "NullablePatch::is_unchanged")]
    pub remote_url: NullablePatch<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_status: Option<String>,
    #[serde(default, skip_serializing_if = "NullablePatch::is_unchanged")]
    pub error: NullablePatch<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub received_size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_size_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressReconciliation {
    pub record: CloudUploadRecord,
    #[serde(rename = "renderRequired")]
    pub render_required: bool,
}

pub fn reconcile_upload_progress(
    current: &CloudUploadRecord,
    progress: &UploadProgressPatch,
    default_visibility: &str,
    now_unix: u64,
) -> Result<ProgressReconciliation, PayloadBoundsError> {
    let mut record = CloudUploadRecord {
        local_clip_id: progress
            .local_clip_id
            .clone()
            .unwrap_or_else(|| current.local_clip_id.clone()),
        path: progress
            .path
            .clone()
            .unwrap_or_else(|| current.path.clone()),
        remote_clip_id: progress.remote_clip_id.apply_to(&current.remote_clip_id),
        remote_url: progress.remote_url.apply_to(&current.remote_url),
        visibility: if current.visibility.is_empty() {
            if default_visibility.is_empty() {
                "private".into()
            } else {
                default_visibility.into()
            }
        } else {
            current.visibility.clone()
        },
        upload_status: progress
            .upload_status
            .clone()
            .unwrap_or_else(|| current.upload_status.clone()),
        error: progress.error.apply_to(&current.error),
        updated_at_unix: current.updated_at_unix,
    };
    if record.upload_status.is_empty() {
        record.upload_status = "not_uploaded".into();
    }
    record.validate()?;

    let render_required = current.local_clip_id != record.local_clip_id
        || current.path != record.path
        || current.remote_clip_id != record.remote_clip_id
        || current.remote_url != record.remote_url
        || current.visibility != record.visibility
        || current.upload_status != record.upload_status
        || current.error != record.error;
    if render_required || record.updated_at_unix == 0 {
        record.updated_at_unix = now_unix;
    }
    Ok(ProgressReconciliation {
        record,
        render_required,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudEntry {
    pub local_clip_id: String,
    pub path: String,
    pub title: String,
    pub remote_url: String,
    pub visibility: String,
    pub upload_status: String,
    pub updated_at_unix: u64,
    pub local_available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_clip_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_size_bytes: Option<i64>,
}

pub fn merge_cloud_library_entries(
    uploads: &[CloudUploadRecord],
    local_paths: &[String],
    cloud_clips: &[CloudLibraryItem],
    cloud_list_authoritative: bool,
) -> Result<Vec<CloudEntry>, PayloadBoundsError> {
    check_count("cloud.uploads", uploads.len(), MAX_CLOUD_INDEX_ROWS)?;
    check_count("cloud.local_paths", local_paths.len(), MAX_CLOUD_INDEX_ROWS)?;
    check_count("cloud.clips", cloud_clips.len(), MAX_CLOUD_INDEX_ROWS)?;
    for upload in uploads {
        upload.validate()?;
    }
    for path in local_paths {
        check_string("cloud.local_path", path)?;
    }

    let mut uploads_by_local_id = BTreeMap::new();
    for upload in uploads {
        if !upload.local_clip_id.is_empty() {
            uploads_by_local_id.insert(upload.local_clip_id.as_str(), upload);
        }
    }
    let mut seen_local_ids = BTreeSet::new();
    let mut seen_remote_ids = BTreeSet::new();
    let mut entries = Vec::new();

    for clip in cloud_clips {
        validate_cloud_item(clip)?;
        if clip.remote_clip_id.is_empty() && clip.remote_url.is_empty() {
            continue;
        }
        let local_id = clip.local_clip_id.as_deref().unwrap_or_default();
        let upload = uploads_by_local_id.get(local_id).copied();
        let path = if clip.path.is_empty() {
            upload.map_or_else(String::new, |record| record.path.clone())
        } else {
            clip.path.clone()
        };
        if !local_id.is_empty() {
            seen_local_ids.insert(local_id.to_owned());
        }
        if !clip.remote_clip_id.is_empty() {
            seen_remote_ids.insert(clip.remote_clip_id.clone());
        }
        let title = first_nonempty([
            clip.title.as_str(),
            clip_name_stem(&path).as_str(),
            clip.remote_clip_id.as_str(),
            "Cloud clip",
        ]);
        entries.push(CloudEntry {
            local_clip_id: local_id.to_owned(),
            path: path.clone(),
            title,
            remote_url: clip.remote_url.clone(),
            visibility: valid_visibility(&clip.visibility)
                .unwrap_or("private")
                .into(),
            upload_status: if clip.upload_status.is_empty() {
                "uploaded_processing".into()
            } else {
                clip.upload_status.clone()
            },
            updated_at_unix: clip.updated_at_unix,
            local_available: local_path_available(local_paths, &path),
            // PlayerCore always emits this field for authoritative server rows,
            // including remote-URL-only rows where the stable id is empty.
            remote_clip_id: Some(clip.remote_clip_id.clone()),
            duration_ms: clip.duration_ms,
            file_size_bytes: clip.file_size_bytes,
        });
    }

    for record in uploads {
        let remote_id = record.remote_clip_id.as_deref().unwrap_or_default();
        let remote_url = record.remote_url.as_deref().unwrap_or_default();
        if remote_id.is_empty() && remote_url.is_empty() {
            continue;
        }
        if (!record.local_clip_id.is_empty() && seen_local_ids.contains(&record.local_clip_id))
            || (!remote_id.is_empty() && seen_remote_ids.contains(remote_id))
        {
            continue;
        }
        let active = matches!(
            record.upload_status.as_str(),
            "queued" | "uploading" | "processing" | "retrying" | "uploaded_processing"
        );
        if (cloud_list_authoritative && !active)
            || matches!(record.upload_status.as_str(), "failed" | "not_uploaded")
        {
            continue;
        }
        let status = if record.upload_status.is_empty() {
            "processing"
        } else {
            &record.upload_status
        };
        let title_stem = clip_name_stem(&record.path);
        let title = first_nonempty([title_stem.as_str(), remote_id, "Cloud clip"]);
        entries.push(CloudEntry {
            local_clip_id: record.local_clip_id.clone(),
            path: record.path.clone(),
            title,
            remote_url: remote_url.into(),
            visibility: valid_visibility(&record.visibility)
                .unwrap_or(if status == "uploaded_private" {
                    "private"
                } else {
                    "public"
                })
                .into(),
            upload_status: status.into(),
            updated_at_unix: record.updated_at_unix,
            local_available: local_path_available(local_paths, &record.path),
            remote_clip_id: (!remote_id.is_empty()).then(|| remote_id.into()),
            duration_ms: None,
            file_size_bytes: None,
        });
    }

    check_count("cloud.entries", entries.len(), MAX_CLOUD_INDEX_ROWS)?;
    entries.sort_by(|left, right| {
        right
            .updated_at_unix
            .cmp(&left.updated_at_unix)
            .then_with(|| left.title.cmp(&right.title))
            .then_with(|| left.local_clip_id.cmp(&right.local_clip_id))
            .then_with(|| left.remote_clip_id.cmp(&right.remote_clip_id))
    });
    Ok(entries)
}

fn local_path_available(local_paths: &[String], path: &str) -> bool {
    !path.is_empty()
        && local_paths
            .iter()
            .any(|local_path| crate::ClipPathIdentity::same(local_path, path))
}

fn validate_cloud_item(clip: &CloudLibraryItem) -> Result<(), PayloadBoundsError> {
    check_string("cloud.remote_clip_id", &clip.remote_clip_id)?;
    check_optional_string("cloud.local_clip_id", clip.local_clip_id.as_deref())?;
    check_string("cloud.path", &clip.path)?;
    check_string("cloud.title", &clip.title)?;
    check_string("cloud.remote_url", &clip.remote_url)?;
    check_string("cloud.visibility", &clip.visibility)?;
    check_string("cloud.upload_status", &clip.upload_status)?;
    check_optional_string("cloud.source_type", clip.source_type.as_deref())?;
    if clip.duration_ms.is_some_and(|duration| duration < 0) {
        return Err(PayloadBoundsError::Invalid {
            field: "cloud.duration_ms",
        });
    }
    if clip.file_size_bytes.is_some_and(|size| size < 0) {
        return Err(PayloadBoundsError::Invalid {
            field: "cloud.file_size_bytes",
        });
    }
    Ok(())
}

fn clip_name_stem(path: &str) -> String {
    let trimmed = path.trim();
    let name = trimmed
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(trimmed);
    for extension in [".mp4", ".mov", ".mkv", ".webm"] {
        if name
            .get(name.len().saturating_sub(extension.len())..)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case(extension))
        {
            return name[..name.len() - extension.len()].trim().to_owned();
        }
    }
    name.trim().to_owned()
}

fn first_nonempty<'a>(values: impl IntoIterator<Item = &'a str>) -> String {
    values
        .into_iter()
        .find(|value| !value.is_empty())
        .unwrap_or_default()
        .to_owned()
}

fn valid_visibility(value: &str) -> Option<&str> {
    matches!(value, "public" | "unlisted" | "private").then_some(value)
}

fn check_optional_string(
    field: &'static str,
    value: Option<&str>,
) -> Result<(), PayloadBoundsError> {
    value.map_or(Ok(()), |value| check_string(field, value))
}

fn check_string(field: &'static str, value: &str) -> Result<(), PayloadBoundsError> {
    check_count(field, value.len(), MAX_CATALOG_STRING_BYTES)
}

fn check_count(
    field: &'static str,
    actual: usize,
    maximum: usize,
) -> Result<(), PayloadBoundsError> {
    if actual > maximum {
        Err(PayloadBoundsError::TooLarge {
            field,
            actual,
            maximum,
        })
    } else {
        Ok(())
    }
}
