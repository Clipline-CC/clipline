use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{ClipPathIdentity, MAX_CATALOG_IDENTITY_BYTES};

pub const MAX_CATALOG_PAGE_ROWS: usize = 60;
pub const MAX_DECODED_PAGE_IMAGES: usize = 32;
pub const MAX_POSTER_RESULT_ENTRIES: usize = 120;
pub const MAX_LOCAL_INDEX_ROWS: usize = 10_000;
pub const MAX_CLOUD_INDEX_ROWS: usize = 10_000;
pub const MAX_UPLOAD_SUMMARIES: usize = 16;
pub const MAX_CATALOG_WARNINGS: usize = 256;
pub const MAX_MUTATION_ITEMS: usize = MAX_LOCAL_INDEX_ROWS;
pub const MAX_MUTATION_PATH_BYTES: usize = 1024 * 1024;
pub const MAX_CATALOG_STRING_BYTES: usize = MAX_CATALOG_IDENTITY_BYTES;
pub const MAX_FOREGROUND_MESSAGE_BYTES: usize = 64 * 1024;

const _: () = assert!(MAX_DECODED_PAGE_IMAGES <= MAX_CATALOG_PAGE_ROWS);
const _: () = assert!(MAX_CATALOG_PAGE_ROWS <= MAX_POSTER_RESULT_ENTRIES);
const _: () = assert!(MAX_UPLOAD_SUMMARIES <= MAX_CATALOG_PAGE_ROWS);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum GenerationError {
    #[error("{counter} is exhausted")]
    Exhausted { counter: &'static str },
}

macro_rules! checked_generation {
    ($name:ident, $counter:literal) => {
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
                    None => Err(GenerationError::Exhausted { counter: $counter }),
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

checked_generation!(CatalogRevision, "catalog_revision");
checked_generation!(RequestGeneration, "request_generation");
checked_generation!(ForegroundGeneration, "foreground_generation");
checked_generation!(CloudAccountGeneration, "cloud_account_generation");
checked_generation!(WindowAttachmentGeneration, "window_attachment_generation");
checked_generation!(UploadGeneration, "upload_generation");

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IdentityError {
    #[error("{field} must not be empty")]
    Empty { field: &'static str },
    #[error("{field} is {actual} bytes; maximum is {maximum}")]
    TooLong {
        field: &'static str,
        actual: usize,
        maximum: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PayloadBoundsError {
    #[error("{field} contains {actual} bytes or entries; maximum is {maximum}")]
    TooLarge {
        field: &'static str,
        actual: usize,
        maximum: usize,
    },
    #[error("{field} is invalid")]
    Invalid { field: &'static str },
}

fn check_len(field: &'static str, actual: usize, maximum: usize) -> Result<(), PayloadBoundsError> {
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

fn check_string(field: &'static str, value: &str) -> Result<(), PayloadBoundsError> {
    check_len(field, value.len(), MAX_CATALOG_STRING_BYTES)
}

macro_rules! bounded_id {
    ($name:ident, $field:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, IdentityError> {
                let value = value.into();
                if value.trim().is_empty() {
                    return Err(IdentityError::Empty { field: $field });
                }
                if value.len() > MAX_CATALOG_STRING_BYTES {
                    return Err(IdentityError::TooLong {
                        field: $field,
                        actual: value.len(),
                        maximum: MAX_CATALOG_STRING_BYTES,
                    });
                }
                Ok(Self(value))
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

bounded_id!(CloudAccountKey, "cloud_account_key");
bounded_id!(LocalClipId, "local_clip_id");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WindowWorkToken {
    pub attachment: WindowAttachmentGeneration,
    pub foreground: ForegroundGeneration,
    pub request: RequestGeneration,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CloudWorkToken {
    pub window: WindowWorkToken,
    pub account_key: CloudAccountKey,
    pub account_generation: CloudAccountGeneration,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DurableUploadToken {
    pub account_key: CloudAccountKey,
    pub account_generation: CloudAccountGeneration,
    pub upload_generation: UploadGeneration,
    pub local_clip_id: LocalClipId,
    pub source_path: ClipPathIdentity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogSource {
    Local,
    Cloud,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipGame {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocalClipItem {
    pub path: String,
    pub name: String,
    pub title: Option<String>,
    pub kind: String,
    pub session: Option<String>,
    pub size_mb: f64,
    pub modified_unix: u64,
    pub duration_s: Option<f64>,
    pub marker_count: usize,
    pub game: Option<ClipGame>,
}

impl LocalClipItem {
    #[must_use]
    pub fn path_identity(&self) -> Option<ClipPathIdentity> {
        ClipPathIdentity::from_text(&self.path)
    }

    fn validate_bounds(&self) -> Result<(), PayloadBoundsError> {
        check_string("local.path", &self.path)?;
        check_string("local.name", &self.name)?;
        if let Some(title) = &self.title {
            check_string("local.title", title)?;
        }
        check_string("local.kind", &self.kind)?;
        if let Some(session) = &self.session {
            check_string("local.session", session)?;
        }
        if let Some(game) = &self.game {
            check_string("local.game.id", &game.id)?;
            check_string("local.game.name", &game.name)?;
        }
        if !self.size_mb.is_finite() || self.size_mb < 0.0 {
            return Err(PayloadBoundsError::Invalid {
                field: "local.size_mb",
            });
        }
        if self
            .duration_s
            .is_some_and(|duration| !duration.is_finite() || duration < 0.0)
        {
            return Err(PayloadBoundsError::Invalid {
                field: "local.duration_s",
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CloudLibraryItem {
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

impl CloudLibraryItem {
    #[must_use]
    pub fn path_identity(&self) -> Option<ClipPathIdentity> {
        ClipPathIdentity::from_text(&self.path)
    }

    fn validate_bounds(&self) -> Result<(), PayloadBoundsError> {
        check_string("cloud.remote_clip_id", &self.remote_clip_id)?;
        if let Some(local_clip_id) = &self.local_clip_id {
            check_string("cloud.local_clip_id", local_clip_id)?;
        }
        check_string("cloud.path", &self.path)?;
        check_string("cloud.title", &self.title)?;
        check_string("cloud.remote_url", &self.remote_url)?;
        check_string("cloud.visibility", &self.visibility)?;
        check_string("cloud.upload_status", &self.upload_status)?;
        if let Some(source_type) = &self.source_type {
            check_string("cloud.source_type", source_type)?;
        }
        if self.duration_ms.is_some_and(|duration| duration < 0) {
            return Err(PayloadBoundsError::Invalid {
                field: "cloud.duration_ms",
            });
        }
        if self.file_size_bytes.is_some_and(|size| size < 0) {
            return Err(PayloadBoundsError::Invalid {
                field: "cloud.file_size_bytes",
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudAccountSnapshot {
    pub account_key: CloudAccountKey,
    pub generation: CloudAccountGeneration,
    pub connected: bool,
    pub host_url: String,
    pub public_url: Option<String>,
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub user_id: Option<String>,
    pub default_visibility: String,
    pub delete_local_after_upload: bool,
    pub auto_upload_rules: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadSummary {
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogWarning {
    pub code: String,
    pub message: String,
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogPage<T> {
    pub source: CatalogSource,
    pub revision: CatalogRevision,
    pub page: u32,
    pub page_size: usize,
    pub total: usize,
    pub has_next: bool,
    pub truncated: bool,
    pub items: Vec<T>,
    pub warnings: Vec<CatalogWarning>,
}

impl<T> CatalogPage<T> {
    fn validate_shape(&self, maximum_total: usize) -> Result<(), PayloadBoundsError> {
        if self.page_size == 0 {
            return Err(PayloadBoundsError::Invalid { field: "page_size" });
        }
        check_len("page_size", self.page_size, MAX_CATALOG_PAGE_ROWS)?;
        check_len("page.items", self.items.len(), MAX_CATALOG_PAGE_ROWS)?;
        check_len("page.total", self.total, maximum_total)?;
        if self.items.len() > self.page_size {
            return Err(PayloadBoundsError::Invalid {
                field: "page.items_exceed_page_size",
            });
        }
        if self.items.len() > self.total {
            return Err(PayloadBoundsError::Invalid {
                field: "page.items_exceed_total",
            });
        }
        check_len("page.warnings", self.warnings.len(), MAX_CATALOG_WARNINGS)?;
        for warning in &self.warnings {
            check_string("warning.code", &warning.code)?;
            check_string("warning.message", &warning.message)?;
            if let Some(path) = &warning.path {
                check_string("warning.path", path)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresentationRow {
    pub id: String,
    pub path: String,
    pub title: String,
    pub subtitle: String,
    pub duration: String,
    pub kind: String,
    pub selected: bool,
    pub active: bool,
    pub upload_status: Option<String>,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationFailure {
    pub path: ClipPathIdentity,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationReport {
    pub succeeded: Vec<ClipPathIdentity>,
    pub failed: Vec<MutationFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CatalogAction {
    Refresh,
    SetSource {
        source: CatalogSource,
    },
    SetQuery {
        query: String,
    },
    SetPage {
        page: u32,
    },
    OpenClip {
        source: CatalogSource,
        path: String,
    },
    SelectClip {
        path: ClipPathIdentity,
        selected: bool,
    },
    RenameTitle {
        path: String,
        title: String,
    },
    RenameFile {
        path: String,
        file_name: String,
    },
    Delete {
        paths: Vec<String>,
    },
    Reveal {
        path: String,
    },
    Upload {
        path: String,
    },
    CancelUpload {
        local_clip_id: String,
    },
}

impl CatalogAction {
    pub fn validate_bounds(&self) -> Result<(), PayloadBoundsError> {
        match self {
            Self::Refresh | Self::SetSource { .. } | Self::SetPage { .. } => Ok(()),
            Self::SetQuery { query } => check_string("query", query),
            Self::OpenClip { path, .. } | Self::Reveal { path } | Self::Upload { path } => {
                check_string("path", path)
            }
            Self::SelectClip { .. } => Ok(()),
            Self::RenameTitle { path, title } => {
                check_string("rename_title.path", path)?;
                check_string("rename_title.title", title)
            }
            Self::RenameFile { path, file_name } => {
                check_string("rename_file.path", path)?;
                check_string("rename_file.file_name", file_name)
            }
            Self::Delete { paths } => {
                check_len("delete.paths", paths.len(), MAX_MUTATION_ITEMS)?;
                let mut path_bytes = 0_usize;
                for path in paths {
                    check_string("delete.path", path)?;
                    path_bytes =
                        path_bytes
                            .checked_add(path.len())
                            .ok_or(PayloadBoundsError::TooLarge {
                                field: "delete.path_bytes",
                                actual: usize::MAX,
                                maximum: MAX_MUTATION_PATH_BYTES,
                            })?;
                }
                check_len("delete.path_bytes", path_bytes, MAX_MUTATION_PATH_BYTES)?;
                Ok(())
            }
            Self::CancelUpload { local_clip_id } => {
                check_string("cancel_upload.local_clip_id", local_clip_id)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PosterStatus {
    Queued,
    Ready { path: String },
    Missing,
    Failed { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PosterResult {
    pub path: ClipPathIdentity,
    pub status: PosterStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CatalogResult {
    LocalPage {
        token: WindowWorkToken,
        page: CatalogPage<LocalClipItem>,
    },
    CloudPage {
        token: CloudWorkToken,
        page: CatalogPage<CloudLibraryItem>,
    },
    Poster {
        token: WindowWorkToken,
        poster: PosterResult,
    },
    UploadByteProgress {
        token: DurableUploadToken,
        progress: UploadSummary,
    },
    MutationCompleted {
        token: WindowWorkToken,
        report: MutationReport,
    },
    UploadCompleted {
        token: DurableUploadToken,
        result: UploadSummary,
    },
    ForegroundFeedback {
        token: WindowWorkToken,
        message: String,
    },
}

impl CatalogResult {
    #[must_use]
    pub const fn is_barrier(&self) -> bool {
        matches!(
            self,
            Self::MutationCompleted { .. }
                | Self::UploadCompleted { .. }
                | Self::ForegroundFeedback { .. }
        )
    }

    pub fn validate_bounds(&self) -> Result<(), PayloadBoundsError> {
        match self {
            Self::LocalPage { page, .. } => {
                page.validate_shape(MAX_LOCAL_INDEX_ROWS)?;
                for item in &page.items {
                    item.validate_bounds()?;
                }
                Ok(())
            }
            Self::CloudPage { page, .. } => {
                page.validate_shape(MAX_CLOUD_INDEX_ROWS)?;
                for item in &page.items {
                    item.validate_bounds()?;
                }
                Ok(())
            }
            Self::Poster { poster, .. } => match &poster.status {
                PosterStatus::Queued | PosterStatus::Missing => Ok(()),
                PosterStatus::Ready { path } => check_string("poster.ready_path", path),
                PosterStatus::Failed { message } => check_string("poster.message", message),
            },
            Self::UploadByteProgress { token, progress }
            | Self::UploadCompleted {
                token,
                result: progress,
            } => {
                check_string("upload.local_clip_id", &progress.local_clip_id)?;
                check_string("upload.path", &progress.path)?;
                check_string("upload.upload_status", &progress.upload_status)?;
                if let Some(remote_clip_id) = &progress.remote_clip_id {
                    check_string("upload.remote_clip_id", remote_clip_id)?;
                }
                if let Some(remote_url) = &progress.remote_url {
                    check_string("upload.remote_url", remote_url)?;
                }
                if let Some(error) = &progress.error {
                    check_string("upload.error", error)?;
                }
                if progress.local_clip_id != token.local_clip_id.as_str() {
                    return Err(PayloadBoundsError::Invalid {
                        field: "upload.local_clip_id_mismatch",
                    });
                }
                if ClipPathIdentity::from_text(&progress.path).as_ref() != Some(&token.source_path)
                {
                    return Err(PayloadBoundsError::Invalid {
                        field: "upload.path_mismatch",
                    });
                }
                Ok(())
            }
            Self::MutationCompleted { report, .. } => {
                let actual = report.succeeded.len().saturating_add(report.failed.len());
                check_len("mutation.items", actual, MAX_MUTATION_ITEMS)?;
                for failure in &report.failed {
                    check_string("mutation.failure", &failure.message)?;
                }
                Ok(())
            }
            Self::ForegroundFeedback { message, .. } => check_len(
                "foreground_feedback.message",
                message.len(),
                MAX_FOREGROUND_MESSAGE_BYTES,
            ),
        }
    }
}
