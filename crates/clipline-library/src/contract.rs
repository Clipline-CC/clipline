use serde::{Deserialize, Serialize};
use thiserror::Error;

use clipline_shell::FileIdentity;

use crate::{
    ClipDetailOwner, ClipDetailRequest, ClipDetailResult, ClipPathIdentity, DeletedClipsReport,
    LocalClipFilter, LocalClipGrouping, LocalClipSort, MarkerSidecarSummary, PosterWorkToken,
    RenamedClipInfo, MAX_CATALOG_IDENTITY_BYTES, MAX_CLIP_DETAIL_AUDIO_TRACKS,
    MAX_CLIP_DETAIL_FIELD_BYTES, MAX_CLIP_DETAIL_MARKERS, MAX_CLIP_SIDECAR_PLAYS,
    MAX_UPLOAD_DESCRIPTION_UTF16, MAX_UPLOAD_TITLE_UTF16,
};

pub const MAX_CATALOG_PAGE_ROWS: usize = 60;
pub const MAX_CLOUD_SERVER_PAGE: u32 = 1_000_000;
pub const MAX_DECODED_PAGE_IMAGES: usize = 32;
pub const MAX_POSTER_RESULT_ENTRIES: usize = 120;
pub const MAX_LOCAL_INDEX_ROWS: usize = 10_000;
pub const MAX_CLOUD_INDEX_ROWS: usize = 10_000;
pub const MAX_UPLOAD_SUMMARIES: usize = 16;
pub const MAX_CATALOG_WARNINGS: usize = 256;
pub const MAX_MUTATION_ITEMS: usize = MAX_LOCAL_INDEX_ROWS;
pub const MAX_MUTATION_PATH_BYTES: usize = 1024 * 1024;
pub const MAX_MUTATION_ERROR_BYTES: usize = 1024 * 1024;
pub const MAX_CATALOG_STRING_BYTES: usize = MAX_CATALOG_IDENTITY_BYTES;
pub const MAX_FOREGROUND_MESSAGE_BYTES: usize = 64 * 1024;
/// Maximum owned memory attributed to one authoritative local-index completion.
///
/// This is intentionally smaller than the theoretical sum of every per-field
/// bound. It prevents 10,000 individually valid rows from producing an
/// unbounded aggregate allocation.
pub const MAX_LOCAL_INDEX_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;

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
checked_generation!(PosterGeneration, "poster_generation");

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
bounded_id!(ClientClipId, "client_clip_id");
bounded_id!(RemoteClipId, "remote_clip_id");

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

/// Stable Cloud catalog ownership independent of any window attachment or
/// request. Accepted metadata may be reprojected after a window rebuild while
/// still rejecting a replacement login for the same account.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CloudCatalogOwner {
    pub account_key: CloudAccountKey,
    pub account_generation: CloudAccountGeneration,
}

impl CloudCatalogOwner {
    #[must_use]
    pub fn from_work_token(token: &CloudWorkToken) -> Self {
        Self {
            account_key: token.account_key.clone(),
            account_generation: token.account_generation,
        }
    }
}

/// Opaque registry handle that keeps one accepted Cloud media cache entry
/// protected for playback. Zero is reserved so a missing lease cannot be
/// confused with an owned path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct CloudMediaLeaseId(u64);

impl CloudMediaLeaseId {
    pub fn new(value: u64) -> Result<Self, PayloadBoundsError> {
        if value == 0 {
            Err(PayloadBoundsError::Invalid {
                field: "cloud_media.lease_id",
            })
        } else {
            Ok(Self(value))
        }
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for CloudMediaLeaseId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(u64::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DurableUploadToken {
    pub account_key: CloudAccountKey,
    pub account_generation: CloudAccountGeneration,
    pub upload_generation: UploadGeneration,
    pub local_clip_id: LocalClipId,
    pub source_path: ClipPathIdentity,
}

/// Stable catalog identity accepted from the UI.
///
/// Local authority is the validated path identity captured by the scan. Cloud
/// authority includes the exact account generation so reconnecting the same
/// account cannot make a stale row current again.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum CatalogItemIdentity {
    Local {
        path: ClipPathIdentity,
    },
    Cloud {
        account_key: CloudAccountKey,
        account_generation: CloudAccountGeneration,
        remote_clip_id: RemoteClipId,
    },
}

impl CatalogItemIdentity {
    #[must_use]
    pub const fn source(&self) -> CatalogSource {
        match self {
            Self::Local { .. } => CatalogSource::Local,
            Self::Cloud { .. } => CatalogSource::Cloud,
        }
    }

    #[must_use]
    pub const fn local_path(&self) -> Option<&ClipPathIdentity> {
        match self {
            Self::Local { path } => Some(path),
            Self::Cloud { .. } => None,
        }
    }

    #[must_use]
    pub fn matches_cloud_owner(&self, token: &CloudWorkToken) -> bool {
        matches!(
            self,
            Self::Cloud {
                account_key,
                account_generation,
                ..
            } if account_key == &token.account_key
                && *account_generation == token.account_generation
        )
    }

    #[must_use]
    pub fn matches_cloud_catalog_owner(&self, owner: &CloudCatalogOwner) -> bool {
        matches!(
            self,
            Self::Cloud {
                account_key,
                account_generation,
                ..
            } if account_key == &owner.account_key
                && *account_generation == owner.account_generation
        )
    }
}

/// Exact ownership fence for downloading, accepting, and opening one Cloud
/// clip. The window request prevents a late download from opening after the
/// foreground changes; the account fields prevent a replacement login from
/// inheriting the result.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CloudReviewMediaOwner {
    pub token: CloudWorkToken,
    pub item: CatalogItemIdentity,
}

/// Stable cache identity for one Cloud thumbnail revision.
///
/// The version is copied from the accepted Cloud row's `updated_at_unix`.
/// Keeping it in the descriptor prevents a cached thumbnail from surviving a
/// server-side replacement of the same remote clip id.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CloudThumbnailDescriptor {
    pub item: CatalogItemIdentity,
    pub version: u64,
}

impl CloudThumbnailDescriptor {
    pub fn new(item: CatalogItemIdentity, version: u64) -> Result<Self, PayloadBoundsError> {
        let descriptor = Self { item, version };
        descriptor.validate_bounds()?;
        Ok(descriptor)
    }

    pub fn validate_bounds(&self) -> Result<(), PayloadBoundsError> {
        if self.item.source() != CatalogSource::Cloud {
            return Err(PayloadBoundsError::Invalid {
                field: "cloud_thumbnail.item",
            });
        }
        Ok(())
    }
}

/// Exact owner executors must echo for a Cloud thumbnail completion.
///
/// The stable descriptor fences the account and asset version; the work token
/// additionally fences the window attachment, foreground, and request.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CloudThumbnailOwner {
    pub token: CloudWorkToken,
    pub descriptor: CloudThumbnailDescriptor,
}

impl CloudThumbnailOwner {
    pub fn new(
        token: CloudWorkToken,
        descriptor: CloudThumbnailDescriptor,
    ) -> Result<Self, PayloadBoundsError> {
        let owner = Self { token, descriptor };
        owner.validate_bounds()?;
        Ok(owner)
    }

    pub fn validate_bounds(&self) -> Result<(), PayloadBoundsError> {
        self.descriptor.validate_bounds()?;
        validate_cloud_item_owner(&self.descriptor.item, &self.token)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CloudThumbnailRequest {
    pub owner: CloudThumbnailOwner,
}

impl CloudThumbnailRequest {
    pub fn new(owner: CloudThumbnailOwner) -> Result<Self, PayloadBoundsError> {
        let request = Self { owner };
        request.validate_bounds()?;
        Ok(request)
    }

    pub fn validate_bounds(&self) -> Result<(), PayloadBoundsError> {
        self.owner.validate_bounds()
    }
}

/// Read-only, page-bounded ownership manifest for the native thumbnail image
/// owner. It contains no cache paths or mutable Cloud page rows; every entry
/// is fenced to the same accepted account, window, foreground, request, and
/// server-side asset version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CloudThumbnailManifest {
    token: CloudWorkToken,
    page: CloudPageNumber,
    owners: Vec<CloudThumbnailOwner>,
}

impl CloudThumbnailManifest {
    pub fn new(
        token: CloudWorkToken,
        page: CloudPageNumber,
        owners: Vec<CloudThumbnailOwner>,
    ) -> Result<Self, PayloadBoundsError> {
        let manifest = Self {
            token,
            page,
            owners,
        };
        manifest.validate_bounds()?;
        Ok(manifest)
    }

    #[must_use]
    pub const fn token(&self) -> &CloudWorkToken {
        &self.token
    }

    #[must_use]
    pub const fn page(&self) -> CloudPageNumber {
        self.page
    }

    #[must_use]
    pub fn owners(&self) -> &[CloudThumbnailOwner] {
        &self.owners
    }

    pub fn validate_bounds(&self) -> Result<(), PayloadBoundsError> {
        check_len(
            "cloud_thumbnail_manifest.owners",
            self.owners.len(),
            MAX_CATALOG_PAGE_ROWS,
        )?;
        for owner in &self.owners {
            owner.validate_bounds()?;
            if owner.token != self.token {
                return Err(PayloadBoundsError::Invalid {
                    field: "cloud_thumbnail_manifest.owner",
                });
            }
        }
        Ok(())
    }
}

/// Versioned cache request for one accepted Cloud media row.
///
/// The asset version and expected byte size are copied from the exact row the
/// controller resolved. Executors must not invent a version (for example `0`)
/// because doing so can reuse media from an older server revision.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CloudReviewMediaRequest {
    pub owner: CloudReviewMediaOwner,
    pub version: u64,
    pub expected_size_bytes: Option<u64>,
}

impl CloudReviewMediaRequest {
    pub fn new(
        owner: CloudReviewMediaOwner,
        version: u64,
        expected_size_bytes: Option<u64>,
    ) -> Result<Self, PayloadBoundsError> {
        let request = Self {
            owner,
            version,
            expected_size_bytes,
        };
        request.validate_bounds()?;
        Ok(request)
    }

    pub fn validate_bounds(&self) -> Result<(), PayloadBoundsError> {
        self.owner.validate_bounds()
    }
}

impl CloudReviewMediaOwner {
    pub fn new(
        token: CloudWorkToken,
        item: CatalogItemIdentity,
    ) -> Result<Self, PayloadBoundsError> {
        let owner = Self { token, item };
        owner.validate_bounds()?;
        Ok(owner)
    }

    pub fn validate_bounds(&self) -> Result<(), PayloadBoundsError> {
        validate_cloud_item_owner(&self.item, &self.token)
    }

    #[must_use]
    pub fn stable_catalog_owner(&self) -> CloudCatalogOwner {
        CloudCatalogOwner::from_work_token(&self.token)
    }
}

/// A cache path whose eviction protection is held by the executor under the
/// opaque lease id. The controller must emit `ReleaseCloudReviewMedia` when a
/// prepared result is stale or when playback stops retaining the path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedCloudReviewMedia {
    pub path: String,
    pub lease_id: CloudMediaLeaseId,
}

impl PreparedCloudReviewMedia {
    pub fn new(
        path: impl Into<String>,
        lease_id: CloudMediaLeaseId,
    ) -> Result<Self, PayloadBoundsError> {
        let media = Self {
            path: path.into(),
            lease_id,
        };
        media.validate_bounds()?;
        Ok(media)
    }

    pub fn validate_bounds(&self) -> Result<(), PayloadBoundsError> {
        if self.path.trim().is_empty() {
            return Err(PayloadBoundsError::Invalid {
                field: "cloud_media.path",
            });
        }
        check_string("cloud_media.path", &self.path)
    }
}

/// Exact owner of a fallible catalog operation. Each variant contains every
/// fence needed to terminate only the request that emitted the corresponding
/// effect; failures never clear newer work.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CatalogOperationOwner {
    LocalRefresh {
        token: WindowWorkToken,
        revision: CatalogRevision,
    },
    CloudRefresh {
        token: CloudWorkToken,
        revision: CatalogRevision,
        page: CloudPageNumber,
    },
    ClipDetail {
        owner: ClipDetailOwner,
    },
    RenameTitle {
        token: WindowWorkToken,
        target: ClipPathIdentity,
    },
    RenameFile {
        token: WindowWorkToken,
        target: ClipPathIdentity,
    },
    Delete {
        token: WindowWorkToken,
        targets: Vec<ClipPathIdentity>,
    },
    CloudReviewMedia {
        owner: CloudReviewMediaOwner,
    },
}

impl CatalogOperationOwner {
    pub fn validate_bounds(&self) -> Result<(), PayloadBoundsError> {
        match self {
            Self::LocalRefresh { .. }
            | Self::CloudRefresh { .. }
            | Self::ClipDetail { .. }
            | Self::RenameTitle { .. }
            | Self::RenameFile { .. } => Ok(()),
            Self::Delete { targets, .. } => {
                if targets.is_empty() {
                    return Err(PayloadBoundsError::Invalid {
                        field: "operation.delete.targets",
                    });
                }
                check_len(
                    "operation.delete.targets",
                    targets.len(),
                    MAX_MUTATION_ITEMS,
                )?;
                let path_bytes = targets.iter().fold(0_usize, |total, path| {
                    total.saturating_add(path.as_str().len())
                });
                check_len(
                    "operation.delete.target_path_bytes",
                    path_bytes,
                    MAX_MUTATION_PATH_BYTES,
                )
            }
            Self::CloudReviewMedia { owner } => owner.validate_bounds(),
        }
    }

    #[must_use]
    pub const fn cloud_token(&self) -> Option<&CloudWorkToken> {
        match self {
            Self::CloudRefresh { token, .. } => Some(token),
            Self::CloudReviewMedia { owner } => Some(&owner.token),
            Self::LocalRefresh { .. }
            | Self::ClipDetail { .. }
            | Self::RenameTitle { .. }
            | Self::RenameFile { .. }
            | Self::Delete { .. } => None,
        }
    }

    fn estimated_byte_size(&self) -> usize {
        match self {
            Self::Delete { targets, .. } => targets
                .iter()
                .map(ClipPathIdentity::owned_capacity)
                .fold(0_usize, usize::saturating_add)
                .saturating_add(
                    targets
                        .capacity()
                        .saturating_mul(std::mem::size_of::<ClipPathIdentity>()),
                ),
            Self::CloudRefresh { token, .. } => token.account_key.0.capacity(),
            Self::RenameTitle { target, .. } | Self::RenameFile { target, .. } => {
                target.owned_capacity()
            }
            Self::CloudReviewMedia { owner } => owner
                .token
                .account_key
                .0
                .capacity()
                .saturating_add(match &owner.item {
                    CatalogItemIdentity::Cloud {
                        account_key,
                        remote_clip_id,
                        ..
                    } => account_key
                        .0
                        .capacity()
                        .saturating_add(remote_clip_id.0.capacity()),
                    CatalogItemIdentity::Local { path } => path.owned_capacity(),
                }),
            Self::ClipDetail { owner } => owner.item().owned_capacity(),
            Self::LocalRefresh { .. } => 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogSource {
    Local,
    Cloud,
}

/// A zero-based page in the bounded local projection.
///
/// Cloud pages use the distinct one-based [`CloudPageNumber`] type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct LocalPageIndex(u32);

impl LocalPageIndex {
    pub fn new(value: u32) -> Result<Self, PayloadBoundsError> {
        let maximum = MAX_LOCAL_INDEX_ROWS.saturating_sub(1) / MAX_CATALOG_PAGE_ROWS;
        if value as usize > maximum {
            return Err(PayloadBoundsError::TooLarge {
                field: "local_page.index",
                actual: value as usize,
                maximum,
            });
        }
        Ok(Self(value))
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl<'de> Deserialize<'de> for LocalPageIndex {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = u32::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
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
    /// Stable identity captured while the native scanner held the clip open.
    ///
    /// This is intentionally absent from the compatibility JSON row. Paths are
    /// display/reconciliation values; native mutation effects copy this fence
    /// and compare it again immediately before filesystem access.
    #[serde(skip)]
    pub file_identity: Option<FileIdentity>,
    #[serde(default, skip_serializing_if = "MarkerSidecarSummary::is_empty")]
    pub marker_summary: MarkerSidecarSummary,
}

impl LocalClipItem {
    #[must_use]
    pub fn path_identity(&self) -> Option<ClipPathIdentity> {
        ClipPathIdentity::from_text(&self.path)
    }

    pub(crate) fn validate_bounds(&self) -> Result<(), PayloadBoundsError> {
        check_string("local.path", &self.path)?;
        if self.path_identity().is_none() {
            return Err(PayloadBoundsError::Invalid {
                field: "local.path_identity",
            });
        }
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
        if self.marker_count > MAX_CLIP_DETAIL_MARKERS
            || self.marker_summary.review_marker_count > MAX_CLIP_DETAIL_MARKERS
            || self.marker_summary.audio_track_count > MAX_CLIP_DETAIL_AUDIO_TRACKS
            || self.marker_summary.plays.total > MAX_CLIP_SIDECAR_PLAYS
            || self.marker_summary.plays.passed > self.marker_summary.plays.total
            || self.marker_summary.plays.failed > self.marker_summary.plays.total
            || self.marker_summary.plays.incomplete > self.marker_summary.plays.total
            || self
                .marker_summary
                .plays
                .passed
                .saturating_add(self.marker_summary.plays.failed)
                .saturating_add(self.marker_summary.plays.incomplete)
                != self.marker_summary.plays.total
        {
            return Err(PayloadBoundsError::Invalid {
                field: "local.marker_summary",
            });
        }
        if !self.marker_summary.duration_s.is_finite()
            || self.marker_summary.duration_s < 0.0
            || self.marker_summary.marker_digest.len() > MAX_CATALOG_STRING_BYTES
            || self.marker_summary.search_text.len() > MAX_CATALOG_STRING_BYTES
            || self
                .marker_summary
                .player_summary
                .as_ref()
                .is_some_and(|summary| summary.champion_name.len() > MAX_CATALOG_STRING_BYTES)
        {
            return Err(PayloadBoundsError::Invalid {
                field: "local.marker_summary",
            });
        }
        Ok(())
    }

    fn estimated_byte_size(&self) -> usize {
        let game_bytes = self.game.as_ref().map_or(0, |game| {
            game.id.capacity().saturating_add(game.name.capacity())
        });
        let player_bytes = self
            .marker_summary
            .player_summary
            .as_ref()
            .map_or(0, |summary| summary.champion_name.capacity());
        std::mem::size_of::<Self>()
            .saturating_add(self.path.capacity())
            .saturating_add(self.name.capacity())
            .saturating_add(self.title.as_ref().map_or(0, String::capacity))
            .saturating_add(self.kind.capacity())
            .saturating_add(self.session.as_ref().map_or(0, String::capacity))
            .saturating_add(game_bytes)
            .saturating_add(self.marker_summary.marker_digest.capacity())
            .saturating_add(self.marker_summary.search_text.capacity())
            .saturating_add(player_bytes)
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

    pub(crate) fn validate_bounds(&self) -> Result<(), PayloadBoundsError> {
        check_string("cloud.remote_clip_id", &self.remote_clip_id)?;
        if RemoteClipId::new(self.remote_clip_id.clone()).is_err() {
            return Err(PayloadBoundsError::Invalid {
                field: "cloud.remote_clip_id",
            });
        }
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

    fn estimated_byte_size(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(self.remote_clip_id.capacity())
            .saturating_add(self.local_clip_id.as_ref().map_or(0, String::capacity))
            .saturating_add(self.path.capacity())
            .saturating_add(self.title.capacity())
            .saturating_add(self.remote_url.capacity())
            .saturating_add(self.visibility.capacity())
            .saturating_add(self.upload_status.capacity())
            .saturating_add(self.source_type.as_ref().map_or(0, String::capacity))
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

impl UploadSummary {
    pub(crate) fn validate_bounds(&self) -> Result<(), PayloadBoundsError> {
        check_string("upload.local_clip_id", &self.local_clip_id)?;
        check_string("upload.path", &self.path)?;
        check_string("upload.status", &self.upload_status)?;
        if let Some(remote_clip_id) = &self.remote_clip_id {
            check_string("upload.remote_clip_id", remote_clip_id)?;
        }
        if let Some(remote_url) = &self.remote_url {
            check_string("upload.remote_url", remote_url)?;
        }
        if let Some(error) = &self.error {
            check_string("upload.error", error)?;
        }
        Ok(())
    }
}

/// One bounded upload row paired with the exact durable job identity that may
/// be canceled. Keeping the token beside the summary prevents UI adapters from
/// rebuilding upload authority from paths or process-local shadow state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CatalogUploadProjection {
    pub token: DurableUploadToken,
    pub summary: UploadSummary,
}

impl CatalogUploadProjection {
    pub fn new(
        token: DurableUploadToken,
        summary: UploadSummary,
    ) -> Result<Self, PayloadBoundsError> {
        let projection = Self { token, summary };
        projection.validate_bounds()?;
        Ok(projection)
    }

    pub fn validate_bounds(&self) -> Result<(), PayloadBoundsError> {
        self.summary.validate_bounds()?;
        if self.summary.local_clip_id != self.token.local_clip_id.as_str() {
            return Err(PayloadBoundsError::Invalid {
                field: "upload.local_clip_id_mismatch",
            });
        }
        if ClipPathIdentity::from_text(&self.summary.path).as_ref() != Some(&self.token.source_path)
        {
            return Err(PayloadBoundsError::Invalid {
                field: "upload.path_mismatch",
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogWarning {
    pub code: String,
    pub message: String,
    pub path: Option<String>,
}

impl CatalogWarning {
    fn validate_bounds(&self) -> Result<(), PayloadBoundsError> {
        check_string("warning.code", &self.code)?;
        check_string("warning.message", &self.message)?;
        if let Some(path) = &self.path {
            check_string("warning.path", path)?;
        }
        Ok(())
    }

    fn estimated_byte_size(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(self.code.capacity())
            .saturating_add(self.message.capacity())
            .saturating_add(self.path.as_ref().map_or(0, String::capacity))
    }
}

/// Complete authoritative local scan result.
///
/// This deliberately differs from a presentation page: consumers may prune
/// vanished identities only after accepting one non-truncated completion.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LocalIndexCompletion {
    pub token: WindowWorkToken,
    pub revision: CatalogRevision,
    pub truncated: bool,
    pub items: Vec<LocalClipItem>,
    pub warnings: Vec<CatalogWarning>,
}

impl LocalIndexCompletion {
    pub fn new(
        token: WindowWorkToken,
        revision: CatalogRevision,
        truncated: bool,
        items: Vec<LocalClipItem>,
        warnings: Vec<CatalogWarning>,
    ) -> Result<Self, PayloadBoundsError> {
        let completion = Self {
            token,
            revision,
            truncated,
            items,
            warnings,
        };
        completion.validate_bounds()?;
        Ok(completion)
    }

    pub fn validate_bounds(&self) -> Result<(), PayloadBoundsError> {
        check_len("local_index.items", self.items.len(), MAX_LOCAL_INDEX_ROWS)?;
        check_len(
            "local_index.warnings",
            self.warnings.len(),
            MAX_CATALOG_WARNINGS,
        )?;
        for item in &self.items {
            item.validate_bounds()?;
        }
        for warning in &self.warnings {
            warning.validate_bounds()?;
        }
        check_len(
            "local_index.payload_bytes",
            self.estimated_byte_size(),
            MAX_LOCAL_INDEX_PAYLOAD_BYTES,
        )
    }

    #[must_use]
    pub fn estimated_byte_size(&self) -> usize {
        let live = self
            .items
            .iter()
            .fold(std::mem::size_of::<Self>(), |total, item| {
                total.saturating_add(item.estimated_byte_size())
            })
            .saturating_add(
                self.warnings
                    .iter()
                    .map(CatalogWarning::estimated_byte_size)
                    .fold(0_usize, usize::saturating_add),
            );
        live.saturating_add(
            self.items
                .capacity()
                .saturating_sub(self.items.len())
                .saturating_mul(std::mem::size_of::<LocalClipItem>()),
        )
        .saturating_add(
            self.warnings
                .capacity()
                .saturating_sub(self.warnings.len())
                .saturating_mul(std::mem::size_of::<CatalogWarning>()),
        )
    }
}

impl<'de> Deserialize<'de> for LocalIndexCompletion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawCompletion {
            token: WindowWorkToken,
            revision: CatalogRevision,
            truncated: bool,
            items: Vec<LocalClipItem>,
            warnings: Vec<CatalogWarning>,
        }

        let raw = RawCompletion::deserialize(deserializer)?;
        Self::new(
            raw.token,
            raw.revision,
            raw.truncated,
            raw.items,
            raw.warnings,
        )
        .map_err(serde::de::Error::custom)
    }
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
    pub fn validate_shape(&self, maximum_total: usize) -> Result<(), PayloadBoundsError> {
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

/// A one-based Clipline Cloud server page.
///
/// The pinned API clamps page numbers rather than rejecting them, so the
/// client validates the same upper bound before issuing a request. Keeping
/// this distinct from the zero-based local gallery page also prevents the
/// adapter from silently requesting the wrong server offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct CloudPageNumber(u32);

impl CloudPageNumber {
    pub fn new(value: u32) -> Result<Self, PayloadBoundsError> {
        if value == 0 {
            return Err(PayloadBoundsError::Invalid {
                field: "cloud_page.number",
            });
        }
        if value > MAX_CLOUD_SERVER_PAGE {
            return Err(PayloadBoundsError::TooLarge {
                field: "cloud_page.number",
                actual: value as usize,
                maximum: MAX_CLOUD_SERVER_PAGE as usize,
            });
        }
        Ok(Self(value))
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    pub fn checked_next(self) -> Result<Self, PayloadBoundsError> {
        let next = self.0.checked_add(1).ok_or(PayloadBoundsError::TooLarge {
            field: "cloud_page.number",
            actual: usize::MAX,
            maximum: MAX_CLOUD_SERVER_PAGE as usize,
        })?;
        Self::new(next)
    }

    pub fn checked_previous(self) -> Result<Self, PayloadBoundsError> {
        self.0
            .checked_sub(1)
            .filter(|previous| *previous != 0)
            .map(Self)
            .ok_or(PayloadBoundsError::Invalid {
                field: "cloud_page.previous",
            })
    }
}

impl<'de> Deserialize<'de> for CloudPageNumber {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = u32::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CloudNextPage {
    Probe { page: CloudPageNumber },
    Terminal,
}

/// Server-page truth without an invented total or page count.
///
/// A full page can only say that probing the following page is reasonable.
/// If that probe is empty, the controller retains the preceding page and
/// consumes `PastEnd` to disable its conservative Next affordance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CloudPageOutcome {
    Page {
        page: CloudPageNumber,
        items: Vec<CloudLibraryItem>,
        next: CloudNextPage,
    },
    PastEnd {
        requested_page: CloudPageNumber,
        fallback_page: CloudPageNumber,
    },
}

impl CloudPageOutcome {
    #[must_use]
    pub fn has_previous(&self) -> bool {
        match self {
            Self::Page { page, .. } => page.get() > 1,
            Self::PastEnd { fallback_page, .. } => fallback_page.get() > 1,
        }
    }

    #[must_use]
    pub fn items(&self) -> &[CloudLibraryItem] {
        match self {
            Self::Page { items, .. } => items,
            Self::PastEnd { .. } => &[],
        }
    }

    fn validate_shape(&self) -> Result<(), PayloadBoundsError> {
        match self {
            Self::Page { page, items, next } => {
                check_len("cloud_page.items", items.len(), MAX_CATALOG_PAGE_ROWS)?;
                if items.is_empty() && page.get() > 1 {
                    return Err(PayloadBoundsError::Invalid {
                        field: "cloud_page.empty_nonfirst",
                    });
                }
                let expected_next =
                    if items.len() == MAX_CATALOG_PAGE_ROWS && page.get() < MAX_CLOUD_SERVER_PAGE {
                        CloudNextPage::Probe {
                            page: page.checked_next()?,
                        }
                    } else {
                        CloudNextPage::Terminal
                    };
                if *next != expected_next {
                    return Err(PayloadBoundsError::Invalid {
                        field: "cloud_page.next",
                    });
                }
                Ok(())
            }
            Self::PastEnd {
                requested_page,
                fallback_page,
            } => {
                if requested_page.get() <= 1 || requested_page.checked_previous()? != *fallback_page
                {
                    return Err(PayloadBoundsError::Invalid {
                        field: "cloud_page.fallback",
                    });
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CloudListPageCompletion {
    pub token: CloudWorkToken,
    pub revision: CatalogRevision,
    pub outcome: CloudPageOutcome,
    pub warnings: Vec<CatalogWarning>,
}

impl CloudListPageCompletion {
    pub fn page(
        token: CloudWorkToken,
        revision: CatalogRevision,
        page: CloudPageNumber,
        items: Vec<CloudLibraryItem>,
        warnings: Vec<CatalogWarning>,
    ) -> Result<Self, PayloadBoundsError> {
        let next = if items.len() == MAX_CATALOG_PAGE_ROWS && page.get() < MAX_CLOUD_SERVER_PAGE {
            CloudNextPage::Probe {
                page: page.checked_next()?,
            }
        } else {
            CloudNextPage::Terminal
        };
        let completion = Self {
            token,
            revision,
            outcome: CloudPageOutcome::Page { page, items, next },
            warnings,
        };
        completion.validate_bounds()?;
        Ok(completion)
    }

    pub fn past_end(
        token: CloudWorkToken,
        revision: CatalogRevision,
        requested_page: CloudPageNumber,
        warnings: Vec<CatalogWarning>,
    ) -> Result<Self, PayloadBoundsError> {
        let fallback_page = requested_page.checked_previous()?;
        let completion = Self {
            token,
            revision,
            outcome: CloudPageOutcome::PastEnd {
                requested_page,
                fallback_page,
            },
            warnings,
        };
        completion.validate_bounds()?;
        Ok(completion)
    }

    fn validate_bounds(&self) -> Result<(), PayloadBoundsError> {
        self.outcome.validate_shape()?;
        check_len(
            "cloud_page.warnings",
            self.warnings.len(),
            MAX_CATALOG_WARNINGS,
        )?;
        for warning in &self.warnings {
            check_string("warning.code", &warning.code)?;
            check_string("warning.message", &warning.message)?;
            if let Some(path) = &warning.path {
                check_string("warning.path", path)?;
            }
        }
        for item in self.outcome.items() {
            item.validate_bounds()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresentationRow {
    pub identity: CatalogItemIdentity,
    pub path: String,
    pub title: String,
    pub subtitle: String,
    pub duration: String,
    pub kind: String,
    pub selected: bool,
    pub active: bool,
    pub game_badge: Option<String>,
    pub marker_badge: Option<String>,
    pub outcome_badge: Option<String>,
    pub upload_badge: Option<String>,
    pub poster: PresentationPoster,
    pub warning: Option<String>,
}

/// Fixed-shape poster state retained by a presentation row.
///
/// Rows never retain decoded images or a variable badge collection. The
/// desktop adapter resolves a ready path into a window-scoped image later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PresentationPoster {
    Queued,
    Ready { path: String },
    Missing,
    Failed { message: String },
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

/// A local item after the controller resolved its accepted display path.
/// UI actions never construct this value; filesystem adapters still perform
/// canonical containment validation before use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedLocalClip {
    pub identity: ClipPathIdentity,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_file_identity: Option<FileIdentity>,
}

impl ResolvedLocalClip {
    pub fn new(
        identity: ClipPathIdentity,
        path: impl Into<String>,
    ) -> Result<Self, PayloadBoundsError> {
        Self::with_file_identity(identity, path, None)
    }

    pub fn with_file_identity(
        identity: ClipPathIdentity,
        path: impl Into<String>,
        expected_file_identity: Option<FileIdentity>,
    ) -> Result<Self, PayloadBoundsError> {
        let target = Self {
            identity,
            path: path.into(),
            expected_file_identity,
        };
        target.validate_bounds()?;
        Ok(target)
    }

    fn validate_bounds(&self) -> Result<(), PayloadBoundsError> {
        check_string("resolved_local.path", &self.path)?;
        if ClipPathIdentity::from_text(&self.path).as_ref() != Some(&self.identity) {
            return Err(PayloadBoundsError::Invalid {
                field: "resolved_local.identity",
            });
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for ResolvedLocalClip {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawTarget {
            identity: ClipPathIdentity,
            path: String,
            #[serde(default)]
            expected_file_identity: Option<FileIdentity>,
        }

        let raw = RawTarget::deserialize(deserializer)?;
        Self::with_file_identity(raw.identity, raw.path, raw.expected_file_identity)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogDialogTextField {
    Title,
    FileName,
    Description,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogUploadVisibility {
    Private,
    Public,
    Unlisted,
}

/// Saved Cloud defaults used to initialize the upload dialog. These values
/// belong to account/settings context, not to a window-owned form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogCloudPreferences {
    pub default_visibility: CatalogUploadVisibility,
    pub delete_local_after_upload: bool,
}

impl Default for CatalogCloudPreferences {
    fn default() -> Self {
        Self {
            default_visibility: CatalogUploadVisibility::Private,
            delete_local_after_upload: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CatalogAction {
    Refresh,
    SetSource {
        source: CatalogSource,
    },
    SetQuery {
        query: String,
    },
    SetLocalFilter {
        filter: LocalClipFilter,
    },
    SetLocalSort {
        sort: LocalClipSort,
    },
    SetLocalGrouping {
        grouping: LocalClipGrouping,
    },
    SetLocalPage {
        page: LocalPageIndex,
    },
    PreviousPage,
    NextPage,
    EnterSelection,
    ExitSelection,
    ToggleSelection {
        item: CatalogItemIdentity,
    },
    SelectVisiblePage,
    ClearSelection,
    OpenItem {
        item: CatalogItemIdentity,
    },
    CloseActive,
    OpenContext {
        item: CatalogItemIdentity,
    },
    CloseContext,
    OpenRenameTitle {
        item: CatalogItemIdentity,
    },
    OpenRenameFile {
        item: CatalogItemIdentity,
    },
    OpenDelete {
        item: CatalogItemIdentity,
    },
    OpenDeleteSelection,
    OpenUpload {
        item: CatalogItemIdentity,
    },
    OpenCancelUpload {
        token: DurableUploadToken,
    },
    SetDialogText {
        field: CatalogDialogTextField,
        value: String,
    },
    SetUploadVisibility {
        visibility: CatalogUploadVisibility,
    },
    SetUploadAudioTrack {
        track_id: String,
        selected: bool,
    },
    SetDeleteLocalAfterUpload {
        enabled: bool,
    },
    ConfirmDialog,
    CancelDialog,
    Reveal {
        item: CatalogItemIdentity,
    },
    OpenInBrowser {
        item: CatalogItemIdentity,
    },
    CopyPublicLink {
        item: CatalogItemIdentity,
    },
    CancelUpload {
        token: DurableUploadToken,
    },
    Escape,
}

#[derive(Deserialize)]
#[serde(remote = "CatalogAction", tag = "kind", rename_all = "snake_case")]
enum CatalogActionDef {
    Refresh,
    SetSource {
        source: CatalogSource,
    },
    SetQuery {
        query: String,
    },
    SetLocalFilter {
        filter: LocalClipFilter,
    },
    SetLocalSort {
        sort: LocalClipSort,
    },
    SetLocalGrouping {
        grouping: LocalClipGrouping,
    },
    SetLocalPage {
        page: LocalPageIndex,
    },
    PreviousPage,
    NextPage,
    EnterSelection,
    ExitSelection,
    ToggleSelection {
        item: CatalogItemIdentity,
    },
    SelectVisiblePage,
    ClearSelection,
    OpenItem {
        item: CatalogItemIdentity,
    },
    CloseActive,
    OpenContext {
        item: CatalogItemIdentity,
    },
    CloseContext,
    OpenRenameTitle {
        item: CatalogItemIdentity,
    },
    OpenRenameFile {
        item: CatalogItemIdentity,
    },
    OpenDelete {
        item: CatalogItemIdentity,
    },
    OpenDeleteSelection,
    OpenUpload {
        item: CatalogItemIdentity,
    },
    OpenCancelUpload {
        token: DurableUploadToken,
    },
    SetDialogText {
        field: CatalogDialogTextField,
        value: String,
    },
    SetUploadVisibility {
        visibility: CatalogUploadVisibility,
    },
    SetUploadAudioTrack {
        track_id: String,
        selected: bool,
    },
    SetDeleteLocalAfterUpload {
        enabled: bool,
    },
    ConfirmDialog,
    CancelDialog,
    Reveal {
        item: CatalogItemIdentity,
    },
    OpenInBrowser {
        item: CatalogItemIdentity,
    },
    CopyPublicLink {
        item: CatalogItemIdentity,
    },
    CancelUpload {
        token: DurableUploadToken,
    },
    Escape,
}

impl<'de> Deserialize<'de> for CatalogAction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let action = CatalogActionDef::deserialize(deserializer)?;
        action.validate_bounds().map_err(serde::de::Error::custom)?;
        Ok(action)
    }
}

impl CatalogAction {
    pub fn validate_bounds(&self) -> Result<(), PayloadBoundsError> {
        match self {
            Self::Refresh
            | Self::SetSource { .. }
            | Self::SetLocalFilter { .. }
            | Self::SetLocalSort { .. }
            | Self::SetLocalGrouping { .. }
            | Self::SetLocalPage { .. }
            | Self::PreviousPage
            | Self::NextPage
            | Self::EnterSelection
            | Self::ExitSelection
            | Self::ToggleSelection { .. }
            | Self::SelectVisiblePage
            | Self::ClearSelection
            | Self::OpenItem { .. }
            | Self::CloseActive
            | Self::OpenContext { .. }
            | Self::CloseContext
            | Self::OpenRenameTitle { .. }
            | Self::OpenRenameFile { .. }
            | Self::OpenDelete { .. }
            | Self::OpenDeleteSelection
            | Self::OpenUpload { .. }
            | Self::OpenCancelUpload { .. }
            | Self::SetUploadVisibility { .. }
            | Self::SetDeleteLocalAfterUpload { .. }
            | Self::ConfirmDialog
            | Self::CancelDialog
            | Self::Reveal { .. }
            | Self::OpenInBrowser { .. }
            | Self::CopyPublicLink { .. }
            | Self::CancelUpload { .. }
            | Self::Escape => Ok(()),
            Self::SetQuery { query } => check_string("query", query),
            Self::SetDialogText { value, .. } => check_string("dialog.value", value),
            Self::SetUploadAudioTrack { track_id, .. } => {
                check_string("dialog.audio_track_id", track_id)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CatalogUploadOptions {
    pub title: Option<String>,
    pub description: Option<String>,
    pub visibility: CatalogUploadVisibility,
    pub audio_track_ids: Vec<String>,
    pub delete_local_after_upload: bool,
}

impl CatalogUploadOptions {
    fn validate_bounds(&self) -> Result<(), PayloadBoundsError> {
        if let Some(title) = &self.title {
            check_len(
                "upload_options.title_utf16",
                title.encode_utf16().count(),
                MAX_UPLOAD_TITLE_UTF16,
            )?;
            check_len(
                "upload_options.title",
                title.len(),
                MAX_CLIP_DETAIL_FIELD_BYTES,
            )?;
        }
        if let Some(description) = &self.description {
            check_len(
                "upload_options.description_utf16",
                description.encode_utf16().count(),
                MAX_UPLOAD_DESCRIPTION_UTF16,
            )?;
            check_len(
                "upload_options.description",
                description.len(),
                MAX_CLIP_DETAIL_FIELD_BYTES,
            )?;
        }
        check_len(
            "upload_options.audio_track_ids",
            self.audio_track_ids.len(),
            MAX_CLIP_DETAIL_AUDIO_TRACKS,
        )?;
        let mut unique = std::collections::BTreeSet::new();
        for id in &self.audio_track_ids {
            if id.trim().is_empty() {
                return Err(PayloadBoundsError::Invalid {
                    field: "upload_options.audio_track_id",
                });
            }
            check_len(
                "upload_options.audio_track_id",
                id.len(),
                MAX_CLIP_DETAIL_FIELD_BYTES,
            )?;
            if !unique.insert(id.as_str()) {
                return Err(PayloadBoundsError::Invalid {
                    field: "upload_options.duplicate_audio_track_id",
                });
            }
        }
        Ok(())
    }
}

/// Side effects emitted only after the reducer resolves stable identities
/// against accepted catalog metadata.
/// Reducer output. Executors receive this value directly and validate it before
/// performing work; it is intentionally not a deserialization boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CatalogEffect {
    RefreshLocal {
        token: WindowWorkToken,
        revision: CatalogRevision,
    },
    RefreshCloud {
        token: CloudWorkToken,
        revision: CatalogRevision,
        page: CloudPageNumber,
        query: String,
    },
    LoadClipDetail {
        token: WindowWorkToken,
        request: ClipDetailRequest,
        target: ResolvedLocalClip,
        title: String,
        description: String,
    },
    OpenLocalReview {
        token: WindowWorkToken,
        target: ResolvedLocalClip,
    },
    PrepareCloudReviewMedia {
        request: CloudReviewMediaRequest,
    },
    CancelCloudReviewMedia {
        owner: CloudReviewMediaOwner,
    },
    LoadCloudThumbnail {
        request: CloudThumbnailRequest,
    },
    OpenPreparedCloudReview {
        owner: CloudReviewMediaOwner,
        media: PreparedCloudReviewMedia,
    },
    ReleaseCloudReviewMedia {
        lease_id: CloudMediaLeaseId,
    },
    CloseReview {
        token: WindowWorkToken,
    },
    RenameTitle {
        token: WindowWorkToken,
        target: ResolvedLocalClip,
        title: String,
    },
    RenameFile {
        token: WindowWorkToken,
        target: ResolvedLocalClip,
        file_name: String,
    },
    Delete {
        token: WindowWorkToken,
        targets: Vec<ResolvedLocalClip>,
    },
    StartUpload {
        token: WindowWorkToken,
        owner: CloudCatalogOwner,
        target: ResolvedLocalClip,
        options: CatalogUploadOptions,
    },
    CancelUpload {
        token: DurableUploadToken,
    },
    Reveal {
        token: WindowWorkToken,
        target: ResolvedLocalClip,
    },
    OpenInBrowser {
        token: CloudWorkToken,
        item: CatalogItemIdentity,
    },
    CopyPublicLink {
        token: CloudWorkToken,
        item: CatalogItemIdentity,
        url: String,
    },
}

impl CatalogEffect {
    /// Validates the effect against the exact account context that is current
    /// at the handoff boundary. Upload work is durable and window-independent,
    /// so accepting it under a replacement login would otherwise cross the
    /// account-generation fence.
    pub fn validate_for_cloud_owner(
        &self,
        current_owner: Option<&CloudCatalogOwner>,
    ) -> Result<(), PayloadBoundsError> {
        self.validate_bounds()?;
        if let Self::StartUpload { owner, .. } = self {
            if current_owner != Some(owner) {
                return Err(PayloadBoundsError::Invalid {
                    field: "upload.cloud_owner",
                });
            }
        }
        Ok(())
    }

    /// Derives the exact owner executors must echo in `OperationFailed`.
    /// Effects without controller-owned fallible work return `None`.
    pub fn operation_owner(&self) -> Result<Option<CatalogOperationOwner>, PayloadBoundsError> {
        self.validate_bounds()?;
        Ok(match self {
            Self::RefreshLocal { token, revision } => Some(CatalogOperationOwner::LocalRefresh {
                token: *token,
                revision: *revision,
            }),
            Self::RefreshCloud {
                token,
                revision,
                page,
                ..
            } => Some(CatalogOperationOwner::CloudRefresh {
                token: token.clone(),
                revision: *revision,
                page: *page,
            }),
            Self::LoadClipDetail { request, .. } => Some(CatalogOperationOwner::ClipDetail {
                owner: request.owner().clone(),
            }),
            Self::PrepareCloudReviewMedia { request } => {
                Some(CatalogOperationOwner::CloudReviewMedia {
                    owner: request.owner.clone(),
                })
            }
            Self::RenameTitle { token, target, .. } => Some(CatalogOperationOwner::RenameTitle {
                token: *token,
                target: target.identity.clone(),
            }),
            Self::RenameFile { token, target, .. } => Some(CatalogOperationOwner::RenameFile {
                token: *token,
                target: target.identity.clone(),
            }),
            Self::Delete { token, targets } => Some(CatalogOperationOwner::Delete {
                token: *token,
                targets: targets
                    .iter()
                    .map(|target| target.identity.clone())
                    .collect(),
            }),
            Self::OpenLocalReview { .. }
            | Self::CancelCloudReviewMedia { .. }
            | Self::LoadCloudThumbnail { .. }
            | Self::OpenPreparedCloudReview { .. }
            | Self::ReleaseCloudReviewMedia { .. }
            | Self::CloseReview { .. }
            | Self::StartUpload { .. }
            | Self::CancelUpload { .. }
            | Self::Reveal { .. }
            | Self::OpenInBrowser { .. }
            | Self::CopyPublicLink { .. } => None,
        })
    }

    pub fn validate_bounds(&self) -> Result<(), PayloadBoundsError> {
        match self {
            Self::RefreshLocal { .. }
            | Self::CloseReview { .. }
            | Self::CancelUpload { .. }
            | Self::ReleaseCloudReviewMedia { .. } => Ok(()),
            Self::RefreshCloud { query, .. } => check_string("cloud_refresh.query", query),
            Self::LoadClipDetail {
                token,
                request,
                target,
                title,
                description,
            } => {
                target.validate_bounds()?;
                check_string("clip_detail.title", title)?;
                check_string("clip_detail.description", description)?;
                if request.item() != &target.identity || request.owner().window() != *token {
                    return Err(PayloadBoundsError::Invalid {
                        field: "clip_detail.owner",
                    });
                }
                Ok(())
            }
            Self::OpenLocalReview { target, .. } | Self::Reveal { target, .. } => {
                target.validate_bounds()
            }
            Self::PrepareCloudReviewMedia { request } => request.validate_bounds(),
            Self::CancelCloudReviewMedia { owner } => owner.validate_bounds(),
            Self::LoadCloudThumbnail { request } => request.validate_bounds(),
            Self::OpenPreparedCloudReview { owner, media } => {
                owner.validate_bounds()?;
                media.validate_bounds()
            }
            Self::RenameTitle { target, title, .. } => {
                target.validate_bounds()?;
                check_string("rename_title.title", title)
            }
            Self::RenameFile {
                target, file_name, ..
            } => {
                target.validate_bounds()?;
                check_string("rename_file.file_name", file_name)
            }
            Self::Delete { targets, .. } => validate_resolved_targets(targets),
            Self::StartUpload {
                owner,
                target,
                options,
                ..
            } => {
                validate_cloud_catalog_owner(owner)?;
                target.validate_bounds()?;
                options.validate_bounds()
            }
            Self::OpenInBrowser { token, item } => validate_cloud_item_owner(item, token),
            Self::CopyPublicLink { token, item, url } => {
                validate_cloud_item_owner(item, token)?;
                if url.trim().is_empty() {
                    return Err(PayloadBoundsError::Invalid {
                        field: "cloud_link.url",
                    });
                }
                check_string("cloud_link.url", url)
            }
        }
    }
}

fn validate_cloud_catalog_owner(owner: &CloudCatalogOwner) -> Result<(), PayloadBoundsError> {
    if owner.account_key.as_str().trim().is_empty()
        || owner.account_key.as_str().len() > MAX_CATALOG_STRING_BYTES
    {
        Err(PayloadBoundsError::Invalid {
            field: "cloud_catalog_owner.account_key",
        })
    } else {
        Ok(())
    }
}

fn validate_cloud_item_owner(
    item: &CatalogItemIdentity,
    token: &CloudWorkToken,
) -> Result<(), PayloadBoundsError> {
    if item.matches_cloud_owner(token) {
        Ok(())
    } else {
        Err(PayloadBoundsError::Invalid {
            field: "cloud_item.owner",
        })
    }
}

fn validate_resolved_targets(targets: &[ResolvedLocalClip]) -> Result<(), PayloadBoundsError> {
    check_len("mutation.targets", targets.len(), MAX_MUTATION_ITEMS)?;
    let mut path_bytes = 0_usize;
    for target in targets {
        target.validate_bounds()?;
        path_bytes = path_bytes.saturating_add(target.path.len());
    }
    check_len(
        "mutation.target_path_bytes",
        path_bytes,
        MAX_MUTATION_PATH_BYTES,
    )
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
    LocalIndex(LocalIndexCompletion),
    CloudPage(CloudListPageCompletion),
    ClipDetail(ClipDetailResult),
    OperationFailed {
        owner: CatalogOperationOwner,
        message: String,
    },
    CloudReviewMediaPrepared {
        owner: CloudReviewMediaOwner,
        media: PreparedCloudReviewMedia,
    },
    CloudThumbnail {
        owner: CloudThumbnailOwner,
        status: PosterStatus,
    },
    Poster {
        token: PosterWorkToken,
        poster: PosterResult,
    },
    UploadByteProgress {
        token: DurableUploadToken,
        progress: UploadSummary,
    },
    RenameCompleted {
        token: WindowWorkToken,
        result: RenamedClipInfo,
    },
    DeleteCompleted {
        token: WindowWorkToken,
        report: DeletedClipsReport,
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
            Self::OperationFailed { .. }
                | Self::CloudReviewMediaPrepared { .. }
                | Self::RenameCompleted { .. }
                | Self::DeleteCompleted { .. }
                | Self::UploadCompleted { .. }
                | Self::ForegroundFeedback { .. }
        )
    }

    pub fn validate_bounds(&self) -> Result<(), PayloadBoundsError> {
        match self {
            Self::LocalIndex(completion) => completion.validate_bounds(),
            Self::CloudPage(completion) => completion.validate_bounds(),
            Self::ClipDetail(_) => Ok(()),
            Self::OperationFailed { owner, message } => {
                owner.validate_bounds()?;
                check_len(
                    "operation_failure.message",
                    message.len(),
                    MAX_FOREGROUND_MESSAGE_BYTES,
                )
            }
            Self::CloudReviewMediaPrepared { owner, media } => {
                owner.validate_bounds()?;
                media.validate_bounds()
            }
            Self::CloudThumbnail { owner, status } => {
                owner.validate_bounds()?;
                validate_poster_status("cloud_thumbnail", status)
            }
            Self::Poster { token, poster } => {
                if poster.path != token.path {
                    return Err(PayloadBoundsError::Invalid {
                        field: "poster.path_mismatch",
                    });
                }
                validate_poster_status("poster", &poster.status)
            }
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
            Self::RenameCompleted { result, .. } => validate_renamed_clip(result),
            Self::DeleteCompleted { report, .. } => validate_deleted_report(report),
            Self::ForegroundFeedback { message, .. } => check_len(
                "foreground_feedback.message",
                message.len(),
                MAX_FOREGROUND_MESSAGE_BYTES,
            ),
        }
    }

    /// Non-allocating estimate of memory owned by this queued result.
    ///
    /// The queue combines this estimate with an entry cap. Every dynamic field
    /// is either counted directly here or already constrained by its neutral
    /// contract constructor.
    #[must_use]
    pub fn estimated_byte_size(&self) -> usize {
        let payload = match self {
            Self::LocalIndex(completion) => completion.estimated_byte_size(),
            Self::CloudPage(completion) => estimated_cloud_completion_bytes(completion),
            Self::ClipDetail(completion) => estimated_clip_detail_bytes(completion),
            Self::OperationFailed { owner, message } => owner
                .estimated_byte_size()
                .saturating_add(message.capacity()),
            Self::CloudReviewMediaPrepared { owner, media } => owner
                .token
                .account_key
                .0
                .capacity()
                .saturating_add(match &owner.item {
                    CatalogItemIdentity::Cloud {
                        account_key,
                        remote_clip_id,
                        ..
                    } => account_key
                        .0
                        .capacity()
                        .saturating_add(remote_clip_id.0.capacity()),
                    CatalogItemIdentity::Local { path } => path.owned_capacity(),
                })
                .saturating_add(media.path.capacity()),
            Self::CloudThumbnail { owner, status } => estimated_cloud_thumbnail_owner_bytes(owner)
                .saturating_add(estimated_poster_status_bytes(status)),
            Self::Poster { token, poster } => token
                .path
                .owned_capacity()
                .saturating_add(poster.path.owned_capacity())
                .saturating_add(estimated_poster_status_bytes(&poster.status)),
            Self::UploadByteProgress { token, progress }
            | Self::UploadCompleted {
                token,
                result: progress,
            } => estimated_upload_summary_bytes(token, progress),
            Self::RenameCompleted { result, .. } => result
                .old_path
                .capacity()
                .saturating_add(result.path.capacity())
                .saturating_add(result.name.capacity())
                .saturating_add(result.title.as_ref().map_or(0, String::capacity))
                .saturating_add(result.kind.capacity()),
            Self::DeleteCompleted { report, .. } => report
                .deleted
                .iter()
                .map(String::capacity)
                .chain(
                    report
                        .failed
                        .iter()
                        .map(|(path, message)| path.capacity().saturating_add(message.capacity())),
                )
                .fold(0_usize, usize::saturating_add)
                .saturating_add(
                    report
                        .deleted
                        .capacity()
                        .saturating_mul(std::mem::size_of::<String>()),
                )
                .saturating_add(
                    report
                        .failed
                        .capacity()
                        .saturating_mul(std::mem::size_of::<(String, String)>()),
                ),
            Self::ForegroundFeedback { message, .. } => message.capacity(),
        };
        std::mem::size_of::<Self>().saturating_add(payload)
    }
}

fn validate_poster_status(
    context: &'static str,
    status: &PosterStatus,
) -> Result<(), PayloadBoundsError> {
    match status {
        PosterStatus::Queued | PosterStatus::Missing => Ok(()),
        PosterStatus::Ready { path } => {
            if path.trim().is_empty() {
                return Err(PayloadBoundsError::Invalid { field: context });
            }
            check_string("poster.ready_path", path)
        }
        PosterStatus::Failed { message } => check_string("poster.message", message),
    }
}

fn estimated_poster_status_bytes(status: &PosterStatus) -> usize {
    match status {
        PosterStatus::Ready { path } => path.capacity(),
        PosterStatus::Failed { message } => message.capacity(),
        PosterStatus::Queued | PosterStatus::Missing => 0,
    }
}

fn estimated_cloud_thumbnail_owner_bytes(owner: &CloudThumbnailOwner) -> usize {
    owner
        .token
        .account_key
        .0
        .capacity()
        .saturating_add(match &owner.descriptor.item {
            CatalogItemIdentity::Cloud {
                account_key,
                remote_clip_id,
                ..
            } => account_key
                .0
                .capacity()
                .saturating_add(remote_clip_id.0.capacity()),
            CatalogItemIdentity::Local { path } => path.owned_capacity(),
        })
}

fn validate_renamed_clip(result: &RenamedClipInfo) -> Result<(), PayloadBoundsError> {
    check_string("rename_result.old_path", &result.old_path)?;
    check_string("rename_result.path", &result.path)?;
    check_string("rename_result.name", &result.name)?;
    if let Some(title) = &result.title {
        check_string("rename_result.title", title)?;
    }
    check_string("rename_result.kind", &result.kind)?;
    if ClipPathIdentity::from_text(&result.old_path).is_none()
        || ClipPathIdentity::from_text(&result.path).is_none()
    {
        return Err(PayloadBoundsError::Invalid {
            field: "rename_result.path_identity",
        });
    }
    Ok(())
}

fn validate_deleted_report(report: &DeletedClipsReport) -> Result<(), PayloadBoundsError> {
    let items = report.deleted.len().saturating_add(report.failed.len());
    check_len("delete_result.items", items, MAX_MUTATION_ITEMS)?;
    let mut path_bytes = 0_usize;
    let mut error_bytes = 0_usize;
    for path in &report.deleted {
        check_string("delete_result.path", path)?;
        if ClipPathIdentity::from_text(path).is_none() {
            return Err(PayloadBoundsError::Invalid {
                field: "delete_result.path_identity",
            });
        }
        path_bytes = path_bytes.saturating_add(path.len());
    }
    for (path, message) in &report.failed {
        check_string("delete_result.path", path)?;
        check_string("delete_result.error", message)?;
        if ClipPathIdentity::from_text(path).is_none() {
            return Err(PayloadBoundsError::Invalid {
                field: "delete_result.path_identity",
            });
        }
        path_bytes = path_bytes.saturating_add(path.len());
        error_bytes = error_bytes.saturating_add(message.len());
    }
    check_len(
        "delete_result.path_bytes",
        path_bytes,
        MAX_MUTATION_PATH_BYTES,
    )?;
    check_len(
        "delete_result.error_bytes",
        error_bytes,
        MAX_MUTATION_ERROR_BYTES,
    )
}

fn estimated_cloud_completion_bytes(completion: &CloudListPageCompletion) -> usize {
    let items = match &completion.outcome {
        CloudPageOutcome::Page { items, .. } => items
            .iter()
            .map(CloudLibraryItem::estimated_byte_size)
            .fold(0_usize, usize::saturating_add)
            .saturating_add(
                items
                    .capacity()
                    .saturating_sub(items.len())
                    .saturating_mul(std::mem::size_of::<CloudLibraryItem>()),
            ),
        CloudPageOutcome::PastEnd { .. } => 0,
    };
    let warnings = completion
        .warnings
        .iter()
        .map(CatalogWarning::estimated_byte_size)
        .fold(0_usize, usize::saturating_add);
    std::mem::size_of::<CloudListPageCompletion>()
        .saturating_add(completion.token.account_key.0.capacity())
        .saturating_add(items)
        .saturating_add(warnings)
        .saturating_add(
            completion
                .warnings
                .capacity()
                .saturating_sub(completion.warnings.len())
                .saturating_mul(std::mem::size_of::<CatalogWarning>()),
        )
}

fn estimated_clip_detail_bytes(completion: &ClipDetailResult) -> usize {
    completion.estimated_owned_bytes()
}

fn estimated_upload_summary_bytes(token: &DurableUploadToken, summary: &UploadSummary) -> usize {
    std::mem::size_of::<UploadSummary>()
        .saturating_add(token.account_key.0.capacity())
        .saturating_add(token.local_clip_id.0.capacity())
        .saturating_add(token.source_path.owned_capacity())
        .saturating_add(summary.local_clip_id.capacity())
        .saturating_add(summary.path.capacity())
        .saturating_add(summary.upload_status.capacity())
        .saturating_add(summary.remote_clip_id.as_ref().map_or(0, String::capacity))
        .saturating_add(summary.remote_url.as_ref().map_or(0, String::capacity))
        .saturating_add(summary.error.as_ref().map_or(0, String::capacity))
}
