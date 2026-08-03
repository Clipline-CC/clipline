//! One-shot reconciliation of durable upload records with Cloud.
//!
//! Status work is account/record owned, not window owned. UI adapters may
//! discard a stale presentation result, while an exact account-generation and
//! prior-record CAS may still safely update durable state.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use clipline_settings::{MAX_CLOUD_UPLOAD_ID_BYTES, MAX_CLOUD_UPLOAD_URL_BYTES};

use crate::cloud::http::ReqwestCloudProtocol;
use crate::{
    DurableUploadToken, LocalClipId, UploadAccountOwner, UploadCancellation, UploadEndpoint,
    UploadFuture, UploadPhase, UploadRecord, UploadRecordCursor, UploadRecordError, UploadService,
    UploadWorkError,
};

pub const MAX_ACTIVE_UPLOAD_STATUS_SYNCS: usize = 2;
pub const REMOTE_NOT_FOUND_SYNC_MARKER: &str = "remote clip not found during status sync";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteClipStatus {
    pub remote_clip_id: String,
    pub visibility: String,
    pub status: String,
    pub public_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteClipObservation {
    Found(RemoteClipStatus),
    Missing,
}

pub trait UploadStatusRemotePort: Send + Sync {
    fn inspect<'a>(
        &'a self,
        endpoint: &'a UploadEndpoint,
        remote_clip_id: &'a str,
        cancellation: &'a UploadCancellation,
    ) -> UploadFuture<'a, Result<RemoteClipObservation, UploadWorkError>>;
}

#[derive(Debug, Clone, Default)]
pub struct ReqwestUploadStatusRemote;

impl ReqwestUploadStatusRemote {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl UploadStatusRemotePort for ReqwestUploadStatusRemote {
    fn inspect<'a>(
        &'a self,
        endpoint: &'a UploadEndpoint,
        remote_clip_id: &'a str,
        cancellation: &'a UploadCancellation,
    ) -> UploadFuture<'a, Result<RemoteClipObservation, UploadWorkError>> {
        Box::pin(async move {
            cancellation.check().map_err(UploadWorkError::from)?;
            let client = ReqwestCloudProtocol::new(endpoint.api().clone())?;
            let result = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(UploadWorkError::Canceled),
                result = client.get_clip(endpoint.credential().expose(), remote_clip_id) => result,
            };
            match result {
                Ok(clip) => Ok(RemoteClipObservation::Found(RemoteClipStatus {
                    remote_clip_id: clip.id,
                    visibility: clip.visibility,
                    status: clip.status,
                    public_url: clip.public_url,
                })),
                Err(error) if error.is_not_found() => Ok(RemoteClipObservation::Missing),
                Err(error) => Err(error.into()),
            }
        })
    }
}

pub trait UploadStatusRecordPort: Send + Sync {
    fn status_cursor(
        &self,
        owner: &UploadAccountOwner,
        local_clip_id: &LocalClipId,
    ) -> Result<Option<UploadRecordCursor>, UploadRecordError>;

    fn commit_status_sync(
        &self,
        expected: &UploadRecordCursor,
        replacement: UploadRecord,
    ) -> Result<UploadRecordCursor, UploadRecordError>;

    fn remove_status_sync(&self, expected: &UploadRecordCursor) -> Result<(), UploadRecordError>;

    fn is_active_token(&self, token: &DurableUploadToken) -> bool;
}

impl UploadStatusRecordPort for UploadService {
    fn status_cursor(
        &self,
        owner: &UploadAccountOwner,
        local_clip_id: &LocalClipId,
    ) -> Result<Option<UploadRecordCursor>, UploadRecordError> {
        Self::status_cursor(self, owner, local_clip_id)
    }

    fn commit_status_sync(
        &self,
        expected: &UploadRecordCursor,
        replacement: UploadRecord,
    ) -> Result<UploadRecordCursor, UploadRecordError> {
        Self::commit_status_sync(self, expected, replacement)
    }

    fn remove_status_sync(&self, expected: &UploadRecordCursor) -> Result<(), UploadRecordError> {
        Self::remove_status_sync(self, expected)
    }

    fn is_active_token(&self, token: &DurableUploadToken) -> bool {
        Self::is_active_token(self, token)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadStatusSyncOutcome {
    MissingRecord,
    Unchanged(UploadRecord),
    Updated(UploadRecord),
    Removed {
        token: DurableUploadToken,
        path: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadStatusSyncErrorKind {
    Duplicate,
    AtCapacity,
    Canceled,
    AccountChanged,
    Superseded,
    Persistence,
    Remote,
    InvalidResponse,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct UploadStatusSyncError {
    kind: UploadStatusSyncErrorKind,
    message: String,
}

impl UploadStatusSyncError {
    fn new(kind: UploadStatusSyncErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> UploadStatusSyncErrorKind {
        self.kind
    }
}

impl From<UploadRecordError> for UploadStatusSyncError {
    fn from(error: UploadRecordError) -> Self {
        use crate::UploadRecordErrorKind as Kind;
        let kind = match error.kind() {
            Kind::AccountChanged => UploadStatusSyncErrorKind::AccountChanged,
            Kind::Superseded | Kind::Contended => UploadStatusSyncErrorKind::Superseded,
            Kind::Persistence => UploadStatusSyncErrorKind::Persistence,
        };
        Self::new(kind, error.to_string())
    }
}

impl From<UploadWorkError> for UploadStatusSyncError {
    fn from(error: UploadWorkError) -> Self {
        let kind = if error.is_canceled() {
            UploadStatusSyncErrorKind::Canceled
        } else {
            UploadStatusSyncErrorKind::Remote
        };
        Self::new(kind, error.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct StatusKey {
    owner: UploadAccountOwner,
    local_clip_id: LocalClipId,
}

struct UploadStatusSyncInner {
    records: Arc<dyn UploadStatusRecordPort>,
    remote: Arc<dyn UploadStatusRemotePort>,
    active: Mutex<HashSet<StatusKey>>,
}

#[derive(Clone)]
pub struct UploadStatusSyncService {
    inner: Arc<UploadStatusSyncInner>,
}

impl std::fmt::Debug for UploadStatusSyncService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UploadStatusSyncService")
            .finish_non_exhaustive()
    }
}

impl UploadStatusSyncService {
    #[must_use]
    pub fn new(uploads: UploadService, remote: Arc<dyn UploadStatusRemotePort>) -> Self {
        Self::with_ports(Arc::new(uploads), remote)
    }

    #[must_use]
    pub fn with_ports(
        records: Arc<dyn UploadStatusRecordPort>,
        remote: Arc<dyn UploadStatusRemotePort>,
    ) -> Self {
        Self {
            inner: Arc::new(UploadStatusSyncInner {
                records,
                remote,
                active: Mutex::new(HashSet::with_capacity(MAX_ACTIVE_UPLOAD_STATUS_SYNCS)),
            }),
        }
    }

    pub async fn sync(
        &self,
        endpoint: &UploadEndpoint,
        local_clip_id: &LocalClipId,
        cancellation: &UploadCancellation,
    ) -> Result<UploadStatusSyncOutcome, UploadStatusSyncError> {
        let key = StatusKey {
            owner: endpoint.owner().clone(),
            local_clip_id: local_clip_id.clone(),
        };
        let _guard = self.acquire(key)?;
        cancellation.check().map_err(|_| {
            UploadStatusSyncError::new(
                UploadStatusSyncErrorKind::Canceled,
                "cloud status sync was canceled",
            )
        })?;
        let Some(cursor) = self
            .inner
            .records
            .status_cursor(endpoint.owner(), local_clip_id)?
        else {
            return Ok(UploadStatusSyncOutcome::MissingRecord);
        };
        if !endpoint.owner().matches(&cursor.record.token)
            || cursor.record.token.local_clip_id != *local_clip_id
        {
            return Err(invalid_response(
                "cloud status record cursor has different ownership",
            ));
        }
        if self.inner.records.is_active_token(&cursor.record.token) {
            return Ok(UploadStatusSyncOutcome::Unchanged(cursor.record));
        }
        let Some(remote_clip_id) = cursor.record.remote_clip_id.as_deref() else {
            return Ok(UploadStatusSyncOutcome::Unchanged(cursor.record));
        };
        let observation = self
            .inner
            .remote
            .inspect(endpoint, remote_clip_id, cancellation)
            .await?;
        cancellation.check().map_err(|_| {
            UploadStatusSyncError::new(
                UploadStatusSyncErrorKind::Canceled,
                "cloud status sync was canceled",
            )
        })?;
        match observation {
            RemoteClipObservation::Found(remote) => self.apply_found(cursor, remote),
            RemoteClipObservation::Missing => self.apply_missing(cursor),
        }
    }

    fn acquire(&self, key: StatusKey) -> Result<StatusGuard, UploadStatusSyncError> {
        let mut active = self.inner.active.lock().map_err(|_| {
            UploadStatusSyncError::new(
                UploadStatusSyncErrorKind::Persistence,
                "cloud status sync registry is unavailable",
            )
        })?;
        if active.contains(&key) {
            return Err(UploadStatusSyncError::new(
                UploadStatusSyncErrorKind::Duplicate,
                "cloud status sync is already active for this clip",
            ));
        }
        if active.len() >= MAX_ACTIVE_UPLOAD_STATUS_SYNCS {
            return Err(UploadStatusSyncError::new(
                UploadStatusSyncErrorKind::AtCapacity,
                "cloud status sync is at capacity",
            ));
        }
        active.insert(key.clone());
        Ok(StatusGuard {
            inner: Arc::clone(&self.inner),
            key: Some(key),
        })
    }

    fn apply_found(
        &self,
        cursor: UploadRecordCursor,
        remote: RemoteClipStatus,
    ) -> Result<UploadStatusSyncOutcome, UploadStatusSyncError> {
        validate_remote(&cursor, &remote)?;
        let mut replacement = cursor.record.clone();
        replacement.visibility = remote.visibility;
        replacement.remote_clip_id = Some(remote.remote_clip_id);
        replacement.remote_url = if replacement.visibility == "private" {
            None
        } else {
            remote.public_url
        };
        match remote.status.as_str() {
            "ready" => {
                replacement.phase = UploadPhase::Completed;
                replacement.upload_status = if replacement.visibility == "private" {
                    "uploaded_private".into()
                } else {
                    "uploaded_public".into()
                };
                replacement.error = None;
            }
            "failed" => {
                replacement.phase = UploadPhase::Failed;
                replacement.upload_status = "failed".into();
                replacement.error = Some("cloud media processing failed".into());
            }
            "created" | "uploading" | "processing" => {
                replacement.phase = UploadPhase::Abandoned;
                replacement.upload_status = "uploaded_processing".into();
                replacement.error = None;
            }
            _ => unreachable!("validated above"),
        }
        replacement.updated_at_unix = unix_now();
        let committed = self
            .inner
            .records
            .commit_status_sync(&cursor, replacement)?;
        Ok(UploadStatusSyncOutcome::Updated(committed.record))
    }

    fn apply_missing(
        &self,
        cursor: UploadRecordCursor,
    ) -> Result<UploadStatusSyncOutcome, UploadStatusSyncError> {
        if !cursor.record.upload_status.starts_with("uploaded_")
            || cursor.record.upload_status == "uploaded_processing"
        {
            return Ok(UploadStatusSyncOutcome::Unchanged(cursor.record));
        }
        if cursor.record.error.as_deref() == Some(REMOTE_NOT_FOUND_SYNC_MARKER) {
            let token = cursor.record.token.clone();
            let path = cursor.record.path.clone();
            self.inner.records.remove_status_sync(&cursor)?;
            Ok(UploadStatusSyncOutcome::Removed { token, path })
        } else {
            let mut replacement = cursor.record.clone();
            replacement.error = Some(REMOTE_NOT_FOUND_SYNC_MARKER.into());
            replacement.updated_at_unix = unix_now();
            let committed = self
                .inner
                .records
                .commit_status_sync(&cursor, replacement)?;
            Ok(UploadStatusSyncOutcome::Updated(committed.record))
        }
    }
}

fn validate_remote(
    cursor: &UploadRecordCursor,
    remote: &RemoteClipStatus,
) -> Result<(), UploadStatusSyncError> {
    if cursor.record.remote_clip_id.as_deref() != Some(remote.remote_clip_id.as_str()) {
        return Err(invalid_response(
            "cloud status returned a different remote clip",
        ));
    }
    if remote.remote_clip_id.is_empty() || remote.remote_clip_id.len() > MAX_CLOUD_UPLOAD_ID_BYTES {
        return Err(invalid_response("cloud status remote clip ID is invalid"));
    }
    if !matches!(
        remote.visibility.as_str(),
        "private" | "public" | "unlisted"
    ) {
        return Err(invalid_response("cloud status visibility is invalid"));
    }
    if !matches!(
        remote.status.as_str(),
        "created" | "uploading" | "processing" | "ready" | "failed"
    ) {
        return Err(invalid_response("cloud status processing state is invalid"));
    }
    if remote
        .public_url
        .as_ref()
        .is_some_and(|url| url.is_empty() || url.len() > MAX_CLOUD_UPLOAD_URL_BYTES)
    {
        return Err(invalid_response("cloud status public URL is invalid"));
    }
    Ok(())
}

fn invalid_response(message: &'static str) -> UploadStatusSyncError {
    UploadStatusSyncError::new(UploadStatusSyncErrorKind::InvalidResponse, message)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

struct StatusGuard {
    inner: Arc<UploadStatusSyncInner>,
    key: Option<StatusKey>,
}

impl Drop for StatusGuard {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            if let Ok(mut active) = self.inner.active.lock() {
                active.remove(&key);
            }
        }
    }
}
