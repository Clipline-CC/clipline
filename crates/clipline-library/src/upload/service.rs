//! Window-independent durable upload orchestration.
//!
//! Admission, source ownership, persistence, transport, and presentation are
//! separate ports. The service owns every accepted job until it reaches a
//! terminal boundary; dropping a caller or rebuilding a window never cancels
//! work. Every durable transition is an account-fenced compare-and-swap before
//! its corresponding event is published.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::ports::CloudCredential;
use crate::protocol::{
    CloudApiBase, CloudProtocolError, CreateUploadRequest, UploadProgressResponse,
};
use crate::{
    client_clip_id_for_payload, local_clip_id_for_source, ActiveFileRegistry, ClientClipId,
    CloudAccountGeneration, CloudAccountKey, DurableUploadToken, LocalClipId,
    LocalLibraryRepository, OwnedUploadTemp, UploadCancellation, UploadDeletePermit,
    UploadGeneration, UploadOwnershipError, UploadSourceLease, ValidatedClipPath,
    MAX_CATALOG_STRING_BYTES, MAX_CLIP_DETAIL_AUDIO_TRACKS, MAX_CLIP_DETAIL_FIELD_BYTES,
    MAX_UPLOAD_DESCRIPTION_UTF16, MAX_UPLOAD_SUMMARIES, MAX_UPLOAD_TITLE_UTF16,
};
use clipline_settings::{
    MAX_CLOUD_UPLOAD_ERROR_BYTES, MAX_CLOUD_UPLOAD_ID_BYTES, MAX_CLOUD_UPLOAD_PATH_BYTES,
    MAX_CLOUD_UPLOAD_URL_BYTES,
};

pub const MAX_ACTIVE_UPLOAD_JOBS: usize = MAX_UPLOAD_SUMMARIES;

pub type UploadFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Exact durable account fence. It deliberately contains no window identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UploadAccountOwner {
    pub account_key: CloudAccountKey,
    pub account_generation: CloudAccountGeneration,
}

impl UploadAccountOwner {
    #[must_use]
    pub fn new(account_key: CloudAccountKey, account_generation: CloudAccountGeneration) -> Self {
        Self {
            account_key,
            account_generation,
        }
    }

    #[must_use]
    pub fn matches(&self, token: &DurableUploadToken) -> bool {
        self.account_key == token.account_key && self.account_generation == token.account_generation
    }
}

/// Credential-bearing context owned by one detached job.
pub struct UploadEndpoint {
    owner: UploadAccountOwner,
    api: CloudApiBase,
    credential: CloudCredential,
}

impl std::fmt::Debug for UploadEndpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UploadEndpoint")
            .field("owner", &self.owner)
            .field("api", &self.api.as_str())
            .finish_non_exhaustive()
    }
}

impl UploadEndpoint {
    #[must_use]
    pub fn new(owner: UploadAccountOwner, api: CloudApiBase, credential: CloudCredential) -> Self {
        Self {
            owner,
            api,
            credential,
        }
    }

    #[must_use]
    pub const fn owner(&self) -> &UploadAccountOwner {
        &self.owner
    }

    #[must_use]
    pub const fn api(&self) -> &CloudApiBase {
        &self.api
    }

    #[must_use]
    pub const fn credential(&self) -> &CloudCredential {
        &self.credential
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadIntent {
    pub title: Option<String>,
    pub description: Option<String>,
    pub visibility: String,
    pub audio_track_ids: Option<Vec<String>>,
    pub delete_local_after_upload: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UploadRequestError {
    #[error("upload visibility must be private, public, or unlisted")]
    InvalidVisibility,
    #[error("upload title contains {actual} UTF-16 code units; maximum is {maximum}")]
    TitleTooLong { actual: usize, maximum: usize },
    #[error("upload description contains {actual} UTF-16 code units; maximum is {maximum}")]
    DescriptionTooLong { actual: usize, maximum: usize },
    #[error("upload audio selection contains {actual} tracks; maximum is {maximum}")]
    TooManyAudioTracks { actual: usize, maximum: usize },
    #[error("upload audio track id at index {index} is empty")]
    EmptyAudioTrackId { index: usize },
    #[error(
        "upload audio track id at index {index} contains {actual} bytes; maximum is {maximum}"
    )]
    AudioTrackIdTooLong {
        index: usize,
        actual: usize,
        maximum: usize,
    },
    #[error("upload audio track id {id:?} appears more than once")]
    DuplicateAudioTrackId { id: String },
    #[error("upload display path is empty")]
    EmptyDisplayPath,
    #[error("upload display path contains {actual} bytes; maximum is {maximum}")]
    DisplayPathTooLong { actual: usize, maximum: usize },
}

impl Default for UploadIntent {
    fn default() -> Self {
        Self {
            title: None,
            description: None,
            visibility: "private".into(),
            audio_track_ids: None,
            delete_local_after_upload: false,
        }
    }
}

impl UploadIntent {
    pub fn validate_for_path(&self, display_path: &str) -> Result<(), UploadRequestError> {
        if !matches!(self.visibility.as_str(), "private" | "public" | "unlisted") {
            return Err(UploadRequestError::InvalidVisibility);
        }
        if let Some(title) = &self.title {
            let actual = title.encode_utf16().count();
            if actual > MAX_UPLOAD_TITLE_UTF16 {
                return Err(UploadRequestError::TitleTooLong {
                    actual,
                    maximum: MAX_UPLOAD_TITLE_UTF16,
                });
            }
        }
        if let Some(description) = &self.description {
            let actual = description.encode_utf16().count();
            if actual > MAX_UPLOAD_DESCRIPTION_UTF16 {
                return Err(UploadRequestError::DescriptionTooLong {
                    actual,
                    maximum: MAX_UPLOAD_DESCRIPTION_UTF16,
                });
            }
        }
        if let Some(track_ids) = &self.audio_track_ids {
            if track_ids.len() > MAX_CLIP_DETAIL_AUDIO_TRACKS {
                return Err(UploadRequestError::TooManyAudioTracks {
                    actual: track_ids.len(),
                    maximum: MAX_CLIP_DETAIL_AUDIO_TRACKS,
                });
            }
            let mut unique = std::collections::BTreeSet::new();
            for (index, id) in track_ids.iter().enumerate() {
                if id.trim().is_empty() {
                    return Err(UploadRequestError::EmptyAudioTrackId { index });
                }
                if id.len() > MAX_CLIP_DETAIL_FIELD_BYTES {
                    return Err(UploadRequestError::AudioTrackIdTooLong {
                        index,
                        actual: id.len(),
                        maximum: MAX_CLIP_DETAIL_FIELD_BYTES,
                    });
                }
                if !unique.insert(id.as_str()) {
                    return Err(UploadRequestError::DuplicateAudioTrackId { id: id.clone() });
                }
            }
        }
        if display_path.is_empty() {
            return Err(UploadRequestError::EmptyDisplayPath);
        }
        if display_path.len() > MAX_CLOUD_UPLOAD_PATH_BYTES {
            return Err(UploadRequestError::DisplayPathTooLong {
                actual: display_path.len(),
                maximum: MAX_CLOUD_UPLOAD_PATH_BYTES,
            });
        }
        Ok(())
    }
}

pub struct UploadStartRequest {
    pub endpoint: UploadEndpoint,
    pub source: ValidatedClipPath,
    pub intent: UploadIntent,
}

impl std::fmt::Debug for UploadStartRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UploadStartRequest")
            .field("endpoint", &self.endpoint)
            .field("source", &self.source)
            .field("intent", &self.intent)
            .finish()
    }
}

enum PreparedPayloadOwner {
    Original(PathBuf),
    Owned(OwnedUploadTemp),
}

/// Prepared MP4 plus its create-upload metadata.
///
/// An owned payload is identity-fenced and removed on Drop. An original
/// payload borrows no handle here because the job retains `UploadSourceLease`.
pub struct PreparedUploadPayload {
    owner: PreparedPayloadOwner,
    request: CreateUploadRequest,
    description: Option<String>,
    client_clip_id: ClientClipId,
}

impl std::fmt::Debug for PreparedUploadPayload {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedUploadPayload")
            .field("path", &self.path())
            .field("request", &self.request)
            .field("description", &self.description)
            .field("client_clip_id", &self.client_clip_id)
            .finish_non_exhaustive()
    }
}

impl PreparedUploadPayload {
    pub fn original(
        source: &UploadSourceLease,
        request: CreateUploadRequest,
        description: Option<String>,
        client_clip_id: ClientClipId,
    ) -> Result<Self, UploadWorkError> {
        let payload = Self {
            owner: PreparedPayloadOwner::Original(source.canonical_path().to_path_buf()),
            request,
            description,
            client_clip_id,
        };
        payload.validate(source.token())?;
        Ok(payload)
    }

    pub fn owned(
        mut temp: OwnedUploadTemp,
        request: CreateUploadRequest,
        description: Option<String>,
        client_clip_id: ClientClipId,
        token: &DurableUploadToken,
    ) -> Result<Self, UploadWorkError> {
        temp.seal().map_err(UploadWorkError::ownership)?;
        let payload = Self {
            owner: PreparedPayloadOwner::Owned(temp),
            request,
            description,
            client_clip_id,
        };
        payload.validate(token)?;
        Ok(payload)
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        match &self.owner {
            PreparedPayloadOwner::Original(path) => path,
            PreparedPayloadOwner::Owned(temp) => temp.path(),
        }
    }

    #[must_use]
    pub const fn request(&self) -> &CreateUploadRequest {
        &self.request
    }

    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    #[must_use]
    pub const fn client_clip_id(&self) -> &ClientClipId {
        &self.client_clip_id
    }

    fn validate(&self, token: &DurableUploadToken) -> Result<(), UploadWorkError> {
        let expected =
            client_clip_id_for_payload(&token.local_clip_id, &self.request.checksum_sha256)
                .map_err(UploadWorkError::ownership)?;
        if expected != self.client_clip_id
            || self.request.client_clip_id.as_deref() != Some(self.client_clip_id.as_str())
        {
            return Err(UploadWorkError::failed(
                "prepared payload client ID does not match its source and checksum",
            ));
        }
        if self.request.file_size_bytes == 0 {
            return Err(UploadWorkError::failed("prepared upload payload is empty"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UploadWorkError {
    #[error("upload was canceled")]
    Canceled,
    #[error("{0}")]
    Failed(String),
}

impl UploadWorkError {
    #[must_use]
    pub fn failed(message: impl Into<String>) -> Self {
        Self::Failed(bounded_message(message.into()))
    }

    fn ownership(error: UploadOwnershipError) -> Self {
        Self::failed(error.to_string())
    }

    #[must_use]
    pub const fn is_canceled(&self) -> bool {
        matches!(self, Self::Canceled)
    }
}

impl From<CloudProtocolError> for UploadWorkError {
    fn from(error: CloudProtocolError) -> Self {
        if matches!(error, CloudProtocolError::Canceled) {
            Self::Canceled
        } else {
            Self::failed(error.to_string())
        }
    }
}

pub trait UploadAccountFence: Send + Sync {
    fn is_current(&self, owner: &UploadAccountOwner) -> bool;
}

pub trait UploadPreparationPort: Send + Sync {
    fn prepare<'a>(
        &'a self,
        source: &'a UploadSourceLease,
        intent: &'a UploadIntent,
        cancellation: &'a UploadCancellation,
    ) -> UploadFuture<'a, Result<PreparedUploadPayload, UploadWorkError>>;
}

pub trait UploadTransportPort: Send + Sync {
    fn upload<'a>(
        &'a self,
        endpoint: &'a UploadEndpoint,
        payload: &'a PreparedUploadPayload,
        cancellation: &'a UploadCancellation,
        on_progress: &'a mut (dyn FnMut(&UploadProgressResponse) + Send),
    ) -> UploadFuture<'a, Result<UploadProgressResponse, UploadWorkError>>;
}

impl UploadTransportPort for crate::ReqwestUploadTransport {
    fn upload<'a>(
        &'a self,
        endpoint: &'a UploadEndpoint,
        payload: &'a PreparedUploadPayload,
        cancellation: &'a UploadCancellation,
        on_progress: &'a mut (dyn FnMut(&UploadProgressResponse) + Send),
    ) -> UploadFuture<'a, Result<UploadProgressResponse, UploadWorkError>> {
        Box::pin(async move {
            self.upload_file_with_progress(
                endpoint.api(),
                endpoint.credential(),
                payload.request(),
                payload.description(),
                payload.path(),
                cancellation,
                on_progress,
            )
            .await
            .map_err(Into::into)
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyUpload {
    pub remote_clip_id: String,
    pub visibility: String,
    pub remote_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadRemoteOutcome {
    Ready(ReadyUpload),
    ProcessingTimedOut,
    ProcessingFailed(String),
}

pub trait UploadRemotePort: Send + Sync {
    fn wait_until_ready<'a>(
        &'a self,
        endpoint: &'a UploadEndpoint,
        remote_clip_id: &'a str,
        visibility: &'a str,
        cancellation: &'a UploadCancellation,
    ) -> UploadFuture<'a, Result<UploadRemoteOutcome, UploadWorkError>>;

    fn probe_media<'a>(
        &'a self,
        endpoint: &'a UploadEndpoint,
        remote_clip_id: &'a str,
        cancellation: &'a UploadCancellation,
    ) -> UploadFuture<'a, Result<(), UploadWorkError>>;
}

/// Repository-owned exact cleanup of the primary clip and all sidecars.
///
/// The service acquires the exclusive source permit only after Cloud reports
/// Ready and the media probe succeeds. The permit retains the exact validated
/// clip, making source/permit mismatch unrepresentable. Implementations
/// preflight every sidecar and then delete through repository identity fences;
/// they must never fall back to raw path deletion.
pub trait UploadDeletionPort: Send + Sync {
    fn delete_local(&self, permit: &UploadDeletePermit) -> Result<(), UploadWorkError>;
}

impl UploadDeletionPort for LocalLibraryRepository {
    fn delete_local(&self, permit: &UploadDeletePermit) -> Result<(), UploadWorkError> {
        permit
            .delete_clip_and_sidecars_if_current(self)
            .map_err(UploadWorkError::ownership)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadPhase {
    Queued,
    Preparing,
    Uploading,
    Processing,
    Verifying,
    DeletingLocal,
    Completed,
    Canceled,
    Failed,
    Abandoned,
}

impl UploadPhase {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Canceled | Self::Failed | Self::Abandoned
        )
    }

    const fn can_transition_to(self, next: Self) -> bool {
        if self as u8 == next as u8 {
            return true;
        }
        match self {
            Self::Queued => matches!(
                next,
                Self::Preparing | Self::Canceled | Self::Failed | Self::Abandoned
            ),
            Self::Preparing => matches!(
                next,
                Self::Uploading | Self::Canceled | Self::Failed | Self::Abandoned
            ),
            Self::Uploading => matches!(
                next,
                Self::Processing | Self::Canceled | Self::Failed | Self::Abandoned
            ),
            Self::Processing => matches!(next, Self::Verifying | Self::Failed | Self::Abandoned),
            Self::Verifying => matches!(
                next,
                Self::DeletingLocal | Self::Completed | Self::Failed | Self::Abandoned
            ),
            Self::DeletingLocal => matches!(next, Self::Completed | Self::Failed | Self::Abandoned),
            Self::Completed | Self::Canceled | Self::Failed | Self::Abandoned => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadRecord {
    pub token: DurableUploadToken,
    pub client_clip_id: Option<ClientClipId>,
    pub path: String,
    pub visibility: String,
    pub phase: UploadPhase,
    pub upload_status: String,
    pub received_size_bytes: u64,
    pub file_size_bytes: u64,
    pub remote_clip_id: Option<String>,
    pub remote_url: Option<String>,
    pub error: Option<String>,
    pub local_deleted: bool,
    pub updated_at_unix: u64,
}

impl UploadRecord {
    fn queued(token: DurableUploadToken, path: String, visibility: String) -> Self {
        Self {
            token,
            client_clip_id: None,
            path,
            visibility,
            phase: UploadPhase::Queued,
            upload_status: "queued".into(),
            received_size_bytes: 0,
            file_size_bytes: 0,
            remote_clip_id: None,
            remote_url: None,
            error: None,
            local_deleted: false,
            updated_at_unix: unix_now(),
        }
    }

    fn transition(&self, phase: UploadPhase, status: &str) -> Result<Self, UploadWorkError> {
        if !self.phase.can_transition_to(phase) {
            return Err(UploadWorkError::failed(format!(
                "invalid upload phase transition {:?} -> {:?}",
                self.phase, phase
            )));
        }
        let mut next = self.clone();
        next.phase = phase;
        next.upload_status = status.into();
        next.updated_at_unix = unix_now();
        Ok(next)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadRecordCursor {
    pub revision: u64,
    pub record: UploadRecord,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadRecordErrorKind {
    AccountChanged,
    Superseded,
    Contended,
    Persistence,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct UploadRecordError {
    kind: UploadRecordErrorKind,
    message: String,
}

impl UploadRecordError {
    #[must_use]
    pub fn new(kind: UploadRecordErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: bounded_message(message.into()),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> UploadRecordErrorKind {
        self.kind
    }
}

pub trait UploadRecordPort: Send + Sync {
    /// Derive a generation newer than every durable generation previously used
    /// by this exact account/local-clip slot or a path-equivalent legacy alias.
    /// `admit` atomically commits that candidate against the durable prior
    /// record; concurrent candidates may race, but only one exact generation
    /// can be admitted.
    fn allocate_generation(
        &self,
        owner: &UploadAccountOwner,
        local_clip_id: &LocalClipId,
        source: &ValidatedClipPath,
    ) -> Result<UploadGeneration, UploadRecordError>;

    fn load(
        &self,
        owner: &UploadAccountOwner,
        local_clip_id: &LocalClipId,
    ) -> Result<Option<UploadRecordCursor>, UploadRecordError>;

    /// Atomically admits this generation against the exact account and current
    /// durable record slot.
    fn admit(&self, record: UploadRecord) -> Result<UploadRecordCursor, UploadRecordError>;

    /// Whole-settings compare-and-swap. Implementations must validate the
    /// token's account generation and exact expected record inside the commit.
    fn compare_exchange(
        &self,
        expected: &UploadRecordCursor,
        replacement: UploadRecord,
    ) -> Result<UploadRecordCursor, UploadRecordError>;

    /// Atomically removes the exact expected record. Implementations must
    /// validate the token's account generation and the complete expected
    /// cursor inside the same whole-settings commit. A missing or changed
    /// record is `Superseded`, never a successful no-op.
    fn remove_exact(&self, expected: &UploadRecordCursor) -> Result<(), UploadRecordError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadEventKind {
    Bytes,
    State,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadNotice {
    pub id: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadServiceEvent {
    pub kind: UploadEventKind,
    pub record: UploadRecord,
    pub notice: Option<UploadNotice>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct UploadEventPortError(pub String);

pub trait UploadEventPort: Send + Sync {
    /// Must be a bounded non-blocking publication operation. Durable state is
    /// already committed when `kind == State`, so queue pressure cannot undo it.
    fn try_publish(&self, event: UploadServiceEvent) -> Result<(), UploadEventPortError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadJobOutcome {
    Completed,
    Canceled,
    Failed,
    AccountChanged,
    Superseded,
    Abandoned,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadJobCompletion {
    pub outcome: UploadJobOutcome,
    pub record: Option<UploadRecord>,
}

pub struct UploadJobHandle {
    token: DurableUploadToken,
    completion: tokio::sync::watch::Receiver<Option<UploadJobCompletion>>,
}

impl std::fmt::Debug for UploadJobHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UploadJobHandle")
            .field("token", &self.token)
            .finish_non_exhaustive()
    }
}

impl UploadJobHandle {
    #[must_use]
    pub const fn token(&self) -> &DurableUploadToken {
        &self.token
    }

    /// Waiting is optional; dropping this handle never cancels the job.
    pub async fn wait(mut self) -> UploadJobCompletion {
        loop {
            if let Some(outcome) = self.completion.borrow().clone() {
                return outcome;
            }
            if self.completion.changed().await.is_err() {
                return UploadJobCompletion {
                    outcome: UploadJobOutcome::Abandoned,
                    record: None,
                };
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UploadStartError {
    #[error(transparent)]
    InvalidRequest(#[from] UploadRequestError),
    #[error("cloud account changed before upload admission")]
    AccountChanged,
    #[error("an upload for this account and local clip is already active")]
    AlreadyActive(DurableUploadToken),
    #[error("the bounded active-upload capacity of {MAX_ACTIVE_UPLOAD_JOBS} is full")]
    AtCapacity,
    #[error("upload generation is exhausted")]
    GenerationExhausted,
    #[error("upload service is shutting down")]
    ShuttingDown,
    #[error("upload service requires an active Tokio runtime")]
    RuntimeUnavailable,
    #[error(transparent)]
    Ownership(#[from] UploadOwnershipError),
    #[error(transparent)]
    Record(#[from] UploadRecordError),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct JobKey {
    owner: UploadAccountOwner,
    local_clip_id: LocalClipId,
}

impl JobKey {
    fn from_token(token: &DurableUploadToken) -> Self {
        Self {
            owner: UploadAccountOwner::new(token.account_key.clone(), token.account_generation),
            local_clip_id: token.local_clip_id.clone(),
        }
    }
}

#[derive(Debug)]
struct ActiveJob {
    token: DurableUploadToken,
    cancellation: UploadCancellation,
}

#[derive(Debug, Default)]
struct ActiveJobs {
    accepting: bool,
    terminated: bool,
    jobs: HashMap<JobKey, ActiveJob>,
}

struct UploadServiceInner {
    registry: ActiveFileRegistry,
    accounts: Arc<dyn UploadAccountFence>,
    preparation: Arc<dyn UploadPreparationPort>,
    transport: Arc<dyn UploadTransportPort>,
    remote: Arc<dyn UploadRemotePort>,
    deletion: Arc<dyn UploadDeletionPort>,
    records: Arc<dyn UploadRecordPort>,
    events: Arc<dyn UploadEventPort>,
    active: Mutex<ActiveJobs>,
    idle: tokio::sync::Notify,
}

#[derive(Clone)]
pub struct UploadService {
    inner: Arc<UploadServiceInner>,
}

impl std::fmt::Debug for UploadService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UploadService")
            .field("active_count", &self.active_count())
            .finish_non_exhaustive()
    }
}

impl UploadService {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        registry: ActiveFileRegistry,
        accounts: Arc<dyn UploadAccountFence>,
        preparation: Arc<dyn UploadPreparationPort>,
        transport: Arc<dyn UploadTransportPort>,
        remote: Arc<dyn UploadRemotePort>,
        deletion: Arc<dyn UploadDeletionPort>,
        records: Arc<dyn UploadRecordPort>,
        events: Arc<dyn UploadEventPort>,
    ) -> Self {
        Self {
            inner: Arc::new(UploadServiceInner {
                registry,
                accounts,
                preparation,
                transport,
                remote,
                deletion,
                records,
                events,
                active: Mutex::new(ActiveJobs {
                    accepting: true,
                    ..ActiveJobs::default()
                }),
                idle: tokio::sync::Notify::new(),
            }),
        }
    }

    pub fn start(&self, request: UploadStartRequest) -> Result<UploadJobHandle, UploadStartError> {
        request
            .intent
            .validate_for_path(request.source.display_path())?;
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| UploadStartError::RuntimeUnavailable)?;
        if !self.inner.accounts.is_current(request.endpoint.owner()) {
            return Err(UploadStartError::AccountChanged);
        }
        let local_clip_id = local_clip_id_for_source(request.source.file_identity());
        let key = JobKey {
            owner: request.endpoint.owner().clone(),
            local_clip_id: local_clip_id.clone(),
        };
        let (token, cancellation) = {
            let mut active = self
                .inner
                .active
                .lock()
                .map_err(|_| UploadStartError::ShuttingDown)?;
            if !active.accepting {
                return Err(UploadStartError::ShuttingDown);
            }
            if let Some(existing) = active.jobs.get(&key) {
                return Err(UploadStartError::AlreadyActive(existing.token.clone()));
            }
            if active.jobs.len() >= MAX_ACTIVE_UPLOAD_JOBS {
                return Err(UploadStartError::AtCapacity);
            }
            let prior = self.inner.records.load(&key.owner, &key.local_clip_id)?;
            if let Some(prior) = &prior {
                validate_upload_record(&prior.record)?;
            }
            let upload_generation = self.inner.records.allocate_generation(
                &key.owner,
                &key.local_clip_id,
                &request.source,
            )?;
            if prior
                .as_ref()
                .is_some_and(|cursor| cursor.record.token.upload_generation >= upload_generation)
            {
                return Err(UploadRecordError::new(
                    UploadRecordErrorKind::Persistence,
                    "durable upload generation did not advance",
                )
                .into());
            }
            let token = DurableUploadToken {
                account_key: key.owner.account_key.clone(),
                account_generation: key.owner.account_generation,
                upload_generation,
                local_clip_id,
                source_path: request.source.comparison_identity().clone(),
            };
            let cancellation = UploadCancellation::default();
            active.jobs.insert(
                key.clone(),
                ActiveJob {
                    token: token.clone(),
                    cancellation: cancellation.clone(),
                },
            );
            (token, cancellation)
        };

        let lease = match self
            .inner
            .registry
            .acquire_upload(&request.source, token.clone())
        {
            Ok(lease) => lease,
            Err(error) => {
                self.inner.finish_job(&token);
                return Err(error.into());
            }
        };
        if !self.inner.accounts.is_current(request.endpoint.owner()) {
            drop(lease);
            self.inner.finish_job(&token);
            return Err(UploadStartError::AccountChanged);
        }

        let queued = UploadRecord::queued(
            token.clone(),
            request.source.display_path().to_owned(),
            request.intent.visibility.clone(),
        );
        if let Err(error) = validate_upload_record(&queued) {
            drop(lease);
            self.inner.finish_job(&token);
            return Err(error.into());
        }
        let cursor = match self.inner.records.admit(queued.clone()) {
            Ok(cursor) => cursor,
            Err(error) => {
                drop(lease);
                self.inner.finish_job(&token);
                return Err(error.into());
            }
        };
        if cursor.record != queued {
            drop(lease);
            self.inner.finish_job(&token);
            return Err(UploadRecordError::new(
                UploadRecordErrorKind::Persistence,
                "upload record admission returned a different record",
            )
            .into());
        }
        if let Err(error) = validate_upload_record(&cursor.record) {
            drop(lease);
            self.inner.finish_job(&token);
            return Err(error.into());
        }
        if self.inner.accounts.is_current(request.endpoint.owner()) {
            self.inner.publish_state(&cursor.record, None);
        } else {
            cancellation.cancel();
        }

        let (completion_tx, completion) = tokio::sync::watch::channel(None);
        let inner = self.inner.clone();
        let task_token = token.clone();
        let completion_token = token.clone();
        // Construct before spawning: if the runtime drops this future without
        // polling it, the captured guard still releases the admission slot.
        let active_guard = ActiveJobGuard {
            inner: inner.clone(),
            token: task_token,
        };
        runtime.spawn(async move {
            let _guard = active_guard;
            let outcome = inner.run(request, lease, cursor, cancellation).await;
            let record = if matches!(
                outcome,
                UploadJobOutcome::AccountChanged | UploadJobOutcome::Superseded
            ) {
                None
            } else {
                inner
                    .records
                    .load(
                        &UploadAccountOwner::new(
                            completion_token.account_key.clone(),
                            completion_token.account_generation,
                        ),
                        &completion_token.local_clip_id,
                    )
                    .ok()
                    .flatten()
                    .filter(|cursor| {
                        cursor.record.token == completion_token
                            && validate_upload_record(&cursor.record).is_ok()
                    })
                    .map(|cursor| cursor.record)
            };
            let _ = completion_tx.send(Some(UploadJobCompletion { outcome, record }));
        });
        Ok(UploadJobHandle { token, completion })
    }

    /// Cancel only the exact active generation. A stale handle cannot affect a
    /// newer retry that reused the same durable owner key.
    pub fn cancel(&self, token: &DurableUploadToken) -> bool {
        let Ok(active) = self.inner.active.lock() else {
            return false;
        };
        let key = JobKey::from_token(token);
        let Some(job) = active.jobs.get(&key) else {
            return false;
        };
        if &job.token != token {
            return false;
        }
        job.cancellation.cancel();
        true
    }

    /// Cancel jobs whose durable account owner is no longer current. This does
    /// not depend on, or invalidate work for, a window attachment.
    pub fn account_changed(&self, current: Option<&UploadAccountOwner>) {
        let Ok(active) = self.inner.active.lock() else {
            return;
        };
        for job in active.jobs.values() {
            let owner = UploadAccountOwner::new(
                job.token.account_key.clone(),
                job.token.account_generation,
            );
            if current != Some(&owner) {
                job.cancellation.cancel();
            }
        }
    }

    /// Temporarily stop admission and cancel every active generation.
    ///
    /// This is reversible so a failed updater or aborted shell shutdown can
    /// return the still-running process to normal operation.
    pub fn quiesce(&self) {
        let Ok(mut active) = self.inner.active.lock() else {
            return;
        };
        active.accepting = false;
        for job in active.jobs.values() {
            job.cancellation.cancel();
        }
    }

    /// Resume admission after a reversible quiesce. Returns `false` after
    /// irreversible shutdown or when the active-job lock is poisoned.
    pub fn resume(&self) -> bool {
        let Ok(mut active) = self.inner.active.lock() else {
            return false;
        };
        if active.terminated {
            return false;
        }
        active.accepting = true;
        true
    }

    /// Permanently stop admission and cancel every active generation.
    pub fn shutdown(&self) {
        let Ok(mut active) = self.inner.active.lock() else {
            return;
        };
        active.terminated = true;
        active.accepting = false;
        for job in active.jobs.values() {
            job.cancellation.cancel();
        }
    }

    pub async fn wait_idle(&self) {
        loop {
            let notified = self.inner.idle.notified();
            if self.active_count() == 0 {
                return;
            }
            notified.await;
        }
    }

    #[must_use]
    pub fn active_count(&self) -> usize {
        self.inner
            .active
            .lock()
            .map_or(MAX_ACTIVE_UPLOAD_JOBS, |active| active.jobs.len())
    }

    /// Commit a delayed status-sync result against its exact prior cursor.
    /// A newer upload or newer sync makes this return `Superseded` without an
    /// event or mutation.
    pub fn commit_status_sync(
        &self,
        expected: &UploadRecordCursor,
        replacement: UploadRecord,
    ) -> Result<UploadRecordCursor, UploadRecordError> {
        validate_upload_record(&expected.record)?;
        validate_upload_record(&replacement)?;
        if expected.record.token != replacement.token {
            return Err(UploadRecordError::new(
                UploadRecordErrorKind::Superseded,
                "status sync changed upload ownership",
            ));
        }
        let owner = UploadAccountOwner::new(
            expected.record.token.account_key.clone(),
            expected.record.token.account_generation,
        );
        if !self.inner.accounts.is_current(&owner) {
            return Err(UploadRecordError::new(
                UploadRecordErrorKind::AccountChanged,
                "cloud account changed while status sync was in flight",
            ));
        }
        let next = self.inner.records.compare_exchange(expected, replacement)?;
        validate_upload_record(&next.record)?;
        if self.inner.accounts.is_current(&owner) {
            self.inner.publish_state(&next.record, None);
        }
        Ok(next)
    }

    /// Remove a status record against the exact cursor that produced the
    /// remote result. This deliberately emits no progress event: callers use
    /// it for the second confirmed-not-found observation, where removing the
    /// durable marker is the complete state transition.
    pub fn remove_status_sync(
        &self,
        expected: &UploadRecordCursor,
    ) -> Result<(), UploadRecordError> {
        validate_upload_record(&expected.record)?;
        let owner = UploadAccountOwner::new(
            expected.record.token.account_key.clone(),
            expected.record.token.account_generation,
        );
        if !self.inner.accounts.is_current(&owner) {
            return Err(UploadRecordError::new(
                UploadRecordErrorKind::AccountChanged,
                "cloud account changed while status removal was in flight",
            ));
        }
        self.inner.records.remove_exact(expected)?;
        if !self.inner.accounts.is_current(&owner) {
            return Err(UploadRecordError::new(
                UploadRecordErrorKind::AccountChanged,
                "cloud account changed while status removal was committing",
            ));
        }
        Ok(())
    }

    pub fn status_cursor(
        &self,
        owner: &UploadAccountOwner,
        local_clip_id: &LocalClipId,
    ) -> Result<Option<UploadRecordCursor>, UploadRecordError> {
        if !self.inner.accounts.is_current(owner) {
            return Err(UploadRecordError::new(
                UploadRecordErrorKind::AccountChanged,
                "cloud account changed before status sync",
            ));
        }
        let cursor = self.inner.records.load(owner, local_clip_id)?;
        if let Some(cursor) = &cursor {
            validate_upload_record(&cursor.record)?;
        }
        if !self.inner.accounts.is_current(owner) {
            return Err(UploadRecordError::new(
                UploadRecordErrorKind::AccountChanged,
                "cloud account changed while status record was loading",
            ));
        }
        Ok(cursor)
    }
}

struct ActiveJobGuard {
    inner: Arc<UploadServiceInner>,
    token: DurableUploadToken,
}

impl Drop for ActiveJobGuard {
    fn drop(&mut self) {
        self.inner.finish_job(&self.token);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunAbort {
    AccountChanged,
    Superseded,
    Persistence,
}

impl UploadServiceInner {
    async fn run(
        self: &Arc<Self>,
        request: UploadStartRequest,
        lease: UploadSourceLease,
        mut cursor: UploadRecordCursor,
        cancellation: UploadCancellation,
    ) -> UploadJobOutcome {
        let mut lease = Some(lease);
        let preparing = match cursor.record.transition(UploadPhase::Preparing, "queued") {
            Ok(record) => record,
            Err(error) => return self.fail(&mut cursor, error.to_string()),
        };
        if let Err(abort) = self.advance(&mut cursor, preparing, None) {
            return abort.outcome();
        }
        if cancellation.is_canceled() {
            return self.cancel_before_completion(&mut cursor);
        }

        let prepared = self
            .preparation
            .prepare(
                lease
                    .as_ref()
                    .expect("the source lease exists before delete"),
                &request.intent,
                &cancellation,
            )
            .await;
        if let Err(abort) = self.ensure_exact(&cursor.record.token) {
            return abort.outcome();
        }
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) if error.is_canceled() => return self.cancel_before_completion(&mut cursor),
            Err(error) => return self.fail(&mut cursor, error.to_string()),
        };
        if let Err(error) = prepared.validate(&cursor.record.token) {
            return self.fail(&mut cursor, error.to_string());
        }
        if cancellation.is_canceled() {
            return self.cancel_before_completion(&mut cursor);
        }

        let mut uploading = match cursor
            .record
            .transition(UploadPhase::Uploading, "uploading")
        {
            Ok(record) => record,
            Err(error) => return self.fail(&mut cursor, error.to_string()),
        };
        uploading.client_clip_id = Some(prepared.client_clip_id().clone());
        uploading.file_size_bytes = prepared.request().file_size_bytes;
        if let Err(abort) = self.advance(&mut cursor, uploading, None) {
            return abort.outcome();
        }

        let callback_abort = Arc::new(Mutex::new(None));
        let callback_abort_sink = callback_abort.clone();
        let callback_cancellation = cancellation.clone();
        let mut on_progress = |progress: &UploadProgressResponse| {
            if callback_abort_sink
                .lock()
                .is_ok_and(|abort| abort.is_some())
            {
                return;
            }
            if let Err(abort) = self.ensure_exact(&cursor.record.token) {
                if let Ok(mut slot) = callback_abort_sink.lock() {
                    *slot = Some(abort);
                }
                callback_cancellation.cancel();
                return;
            }
            let state_changed =
                cursor.record.remote_clip_id.as_deref() != Some(progress.clip_id.as_str());
            if state_changed {
                let mut next = cursor.record.clone();
                next.remote_clip_id = Some(progress.clip_id.clone());
                next.received_size_bytes = progress.received_size_bytes;
                next.file_size_bytes = progress.file_size_bytes;
                next.updated_at_unix = unix_now();
                if let Err(abort) = self.advance(&mut cursor, next, None) {
                    if let Ok(mut slot) = callback_abort_sink.lock() {
                        *slot = Some(abort);
                    }
                    callback_cancellation.cancel();
                }
            } else {
                let mut bytes = cursor.record.clone();
                bytes.received_size_bytes = progress.received_size_bytes;
                bytes.file_size_bytes = progress.file_size_bytes;
                if let Err(abort) = self.publish_bytes(bytes) {
                    if let Ok(mut slot) = callback_abort_sink.lock() {
                        *slot = Some(abort);
                    }
                    callback_cancellation.cancel();
                }
            }
        };

        let transport_result = self
            .transport
            .upload(
                &request.endpoint,
                &prepared,
                &cancellation,
                &mut on_progress,
            )
            .await;
        if let Some(abort) = callback_abort.lock().ok().and_then(|mut slot| slot.take()) {
            return abort.outcome();
        }
        if let Err(abort) = self.ensure_exact(&cursor.record.token) {
            return abort.outcome();
        }
        let progress = match transport_result {
            Ok(progress) => progress,
            Err(error) if error.is_canceled() => return self.cancel_before_completion(&mut cursor),
            Err(error) => return self.fail(&mut cursor, error.to_string()),
        };

        // Server completion is durable even if cancellation raced this await.
        // The local source remains leased and is preserved unless all later
        // ready/probe/delete gates complete under the exact token.
        let mut processing = match cursor
            .record
            .transition(UploadPhase::Processing, "processing")
        {
            Ok(record) => record,
            Err(error) => return self.fail(&mut cursor, error.to_string()),
        };
        processing.remote_clip_id = Some(progress.clip_id.clone());
        processing.received_size_bytes = progress.received_size_bytes;
        processing.file_size_bytes = progress.file_size_bytes;
        if let Err(abort) = self.advance(&mut cursor, processing, None) {
            return abort.outcome();
        }
        if cancellation.is_canceled() {
            return self.abandon_after_server(
                &mut cursor,
                "upload completed on the server, but local follow-up was canceled; the local clip was preserved",
            );
        }

        let remote = self
            .remote
            .wait_until_ready(
                &request.endpoint,
                &progress.clip_id,
                &request.intent.visibility,
                &cancellation,
            )
            .await;
        if let Err(abort) = self.ensure_exact(&cursor.record.token) {
            return abort.outcome();
        }
        let ready = match remote {
            Ok(UploadRemoteOutcome::Ready(ready)) => ready,
            Ok(UploadRemoteOutcome::ProcessingTimedOut) => {
                return self.abandon_after_server(
                    &mut cursor,
                    "cloud processing is still pending; the local clip was preserved",
                );
            }
            Ok(UploadRemoteOutcome::ProcessingFailed(message)) => {
                return self.fail(&mut cursor, message)
            }
            Err(error) if error.is_canceled() => {
                return self.abandon_after_server(
                    &mut cursor,
                    "upload completed on the server, but local follow-up was canceled; the local clip was preserved",
                )
            }
            Err(error) => {
                return self.abandon_after_server(
                    &mut cursor,
                    format!(
                        "checking cloud processing failed: {error}; the local clip was preserved"
                    ),
                );
            }
        };

        let mut verifying = match cursor
            .record
            .transition(UploadPhase::Verifying, "uploaded_processing")
        {
            Ok(record) => record,
            Err(error) => return self.fail(&mut cursor, error.to_string()),
        };
        verifying.remote_clip_id = Some(ready.remote_clip_id.clone());
        verifying.visibility.clone_from(&ready.visibility);
        verifying.remote_url.clone_from(&ready.remote_url);
        if let Err(abort) = self.advance(&mut cursor, verifying, None) {
            return abort.outcome();
        }

        if !request.intent.delete_local_after_upload {
            return self.complete(&mut cursor, &ready, false, None);
        }
        if cancellation.is_canceled() {
            return self.complete(
                &mut cursor,
                &ready,
                false,
                Some("local deletion was canceled; the local clip was preserved".into()),
            );
        }

        let probe = self
            .remote
            .probe_media(&request.endpoint, &ready.remote_clip_id, &cancellation)
            .await;
        if let Err(abort) = self.ensure_exact(&cursor.record.token) {
            return abort.outcome();
        }
        if let Err(error) = probe {
            return self.complete(
                &mut cursor,
                &ready,
                false,
                Some(if error.is_canceled() {
                    "local deletion was canceled; the local clip was preserved".into()
                } else {
                    bounded_message(format!(
                        "cloud media could not be verified: {error}; the local clip was preserved"
                    ))
                }),
            );
        }
        if cancellation.is_canceled() {
            return self.complete(
                &mut cursor,
                &ready,
                false,
                Some("local deletion was canceled; the local clip was preserved".into()),
            );
        }

        let deleting = match cursor
            .record
            .transition(UploadPhase::DeletingLocal, "uploaded_processing")
        {
            Ok(record) => record,
            Err(error) => return self.fail(&mut cursor, error.to_string()),
        };
        if let Err(abort) = self.advance(&mut cursor, deleting, None) {
            return abort.outcome();
        }
        if let Err(abort) = self.ensure_exact(&cursor.record.token) {
            return abort.outcome();
        }
        if cancellation.is_canceled() {
            return self.complete(
                &mut cursor,
                &ready,
                false,
                Some("local deletion was canceled; the local clip was preserved".into()),
            );
        }
        let permit = match lease
            .take()
            .expect("the source lease is consumed exactly once")
            .into_delete_permit()
        {
            Ok(permit) => permit,
            Err(error) => {
                return self.complete_after_source_release(
                    &mut cursor,
                    &ready,
                    bounded_message(format!(
                        "local cleanup could not acquire ownership: {error}; the local clip was preserved"
                    )),
                )
            }
        };
        let delete = self.deletion.delete_local(&permit);
        // Retain the exact exclusive permit through the final record CAS. That
        // closes the ownership gap between filesystem cleanup and durable state.
        let outcome = match delete {
            Ok(()) => self.complete(&mut cursor, &ready, true, None),
            Err(error) => self.complete(
                &mut cursor,
                &ready,
                false,
                Some(bounded_message(format!(
                    "local cleanup failed: {error}; the local clip was preserved"
                ))),
            ),
        };
        drop(permit);
        outcome
    }

    fn ensure_exact(&self, token: &DurableUploadToken) -> Result<(), RunAbort> {
        let owner = UploadAccountOwner::new(token.account_key.clone(), token.account_generation);
        if !self.accounts.is_current(&owner) {
            return Err(RunAbort::AccountChanged);
        }
        if !self.registry.is_current(token) {
            return Err(RunAbort::Superseded);
        }
        Ok(())
    }

    fn advance(
        &self,
        cursor: &mut UploadRecordCursor,
        replacement: UploadRecord,
        notice: Option<UploadNotice>,
    ) -> Result<(), RunAbort> {
        self.ensure_exact(&cursor.record.token)?;
        validate_upload_record(&cursor.record).map_err(RunAbort::from_record)?;
        validate_upload_record(&replacement).map_err(RunAbort::from_record)?;
        if replacement.token != cursor.record.token {
            return Err(RunAbort::Superseded);
        }
        let expected_replacement = replacement.clone();
        let next = self
            .records
            .compare_exchange(cursor, replacement)
            .map_err(RunAbort::from_record)?;
        validate_upload_record(&next.record).map_err(RunAbort::from_record)?;
        if next.record != expected_replacement {
            return Err(RunAbort::Persistence);
        }
        *cursor = next;
        let owner = UploadAccountOwner::new(
            cursor.record.token.account_key.clone(),
            cursor.record.token.account_generation,
        );
        if self.accounts.is_current(&owner) {
            self.publish_state(&cursor.record, notice);
        }
        Ok(())
    }

    fn publish_state(&self, record: &UploadRecord, notice: Option<UploadNotice>) {
        if validate_upload_record(record).is_err()
            || notice.as_ref().is_some_and(|notice| {
                notice.id.is_empty()
                    || notice.id.len() > MAX_CATALOG_STRING_BYTES
                    || notice.message.len() > MAX_CATALOG_STRING_BYTES
            })
        {
            return;
        }
        let _ = self.events.try_publish(UploadServiceEvent {
            kind: UploadEventKind::State,
            record: record.clone(),
            notice,
        });
    }

    fn publish_bytes(&self, record: UploadRecord) -> Result<(), RunAbort> {
        validate_upload_record(&record).map_err(RunAbort::from_record)?;
        let owner = UploadAccountOwner::new(
            record.token.account_key.clone(),
            record.token.account_generation,
        );
        if self.accounts.is_current(&owner) {
            let _ = self.events.try_publish(UploadServiceEvent {
                kind: UploadEventKind::Bytes,
                record,
                notice: None,
            });
        }
        Ok(())
    }

    fn cancel_before_completion(&self, cursor: &mut UploadRecordCursor) -> UploadJobOutcome {
        let canceled = match cursor.record.transition(UploadPhase::Canceled, "canceled") {
            Ok(record) => record,
            Err(_) => return UploadJobOutcome::Canceled,
        };
        match self.advance(
            cursor,
            canceled,
            Some(terminal_notice(
                &cursor.record.token,
                "canceled",
                "Upload canceled",
            )),
        ) {
            Ok(()) => UploadJobOutcome::Canceled,
            Err(abort) => abort.outcome(),
        }
    }

    fn fail(
        &self,
        cursor: &mut UploadRecordCursor,
        message: impl Into<String>,
    ) -> UploadJobOutcome {
        let failed = match cursor.record.transition(UploadPhase::Failed, "failed") {
            Ok(mut record) => {
                record.error = Some(bounded_message(message.into()));
                record
            }
            Err(_) => return UploadJobOutcome::Failed,
        };
        match self.advance(
            cursor,
            failed,
            Some(terminal_notice(
                &cursor.record.token,
                "failed",
                "Upload failed",
            )),
        ) {
            Ok(()) => UploadJobOutcome::Failed,
            Err(abort) => abort.outcome(),
        }
    }

    fn abandon_after_server(
        &self,
        cursor: &mut UploadRecordCursor,
        message: impl Into<String>,
    ) -> UploadJobOutcome {
        let abandoned = match cursor
            .record
            .transition(UploadPhase::Abandoned, "uploaded_processing")
        {
            Ok(mut record) => {
                record.error = Some(bounded_message(message.into()));
                record
            }
            Err(_) => return UploadJobOutcome::Abandoned,
        };
        match self.advance(
            cursor,
            abandoned,
            Some(terminal_notice(
                &cursor.record.token,
                "follow-up-pending",
                "Upload completed; cloud follow-up is pending",
            )),
        ) {
            Ok(()) => UploadJobOutcome::Abandoned,
            Err(abort) => abort.outcome(),
        }
    }

    fn complete(
        &self,
        cursor: &mut UploadRecordCursor,
        ready: &ReadyUpload,
        local_deleted: bool,
        error: Option<String>,
    ) -> UploadJobOutcome {
        let status = if ready.visibility == "private" {
            "uploaded_private"
        } else {
            "uploaded_public"
        };
        let completed = match cursor.record.transition(UploadPhase::Completed, status) {
            Ok(mut record) => {
                record.remote_clip_id = Some(ready.remote_clip_id.clone());
                record.visibility.clone_from(&ready.visibility);
                record.remote_url.clone_from(&ready.remote_url);
                record.received_size_bytes = record.file_size_bytes;
                record.local_deleted = local_deleted;
                record.error = error;
                record
            }
            Err(_) => return UploadJobOutcome::Failed,
        };
        match self.advance(
            cursor,
            completed,
            Some(terminal_notice(
                &cursor.record.token,
                "completed",
                "Upload complete",
            )),
        ) {
            Ok(()) => UploadJobOutcome::Completed,
            Err(abort) => abort.outcome(),
        }
    }

    fn complete_after_source_release(
        &self,
        cursor: &mut UploadRecordCursor,
        ready: &ReadyUpload,
        error: String,
    ) -> UploadJobOutcome {
        let status = if ready.visibility == "private" {
            "uploaded_private"
        } else {
            "uploaded_public"
        };
        let mut completed = match cursor.record.transition(UploadPhase::Completed, status) {
            Ok(record) => record,
            Err(_) => return UploadJobOutcome::Failed,
        };
        completed.remote_clip_id = Some(ready.remote_clip_id.clone());
        completed.visibility.clone_from(&ready.visibility);
        completed.remote_url.clone_from(&ready.remote_url);
        completed.received_size_bytes = completed.file_size_bytes;
        completed.error = Some(error);

        let owner = UploadAccountOwner::new(
            cursor.record.token.account_key.clone(),
            cursor.record.token.account_generation,
        );
        if !self.accounts.is_current(&owner) {
            return UploadJobOutcome::AccountChanged;
        }
        match self.records.compare_exchange(cursor, completed) {
            Ok(next) => {
                *cursor = next;
                if self.accounts.is_current(&owner) {
                    self.publish_state(
                        &cursor.record,
                        Some(terminal_notice(
                            &cursor.record.token,
                            "completed",
                            "Upload complete",
                        )),
                    );
                }
                UploadJobOutcome::Completed
            }
            Err(error) => RunAbort::from_record(error).outcome(),
        }
    }

    fn finish_job(&self, token: &DurableUploadToken) {
        let Ok(mut active) = self.active.lock() else {
            return;
        };
        let key = JobKey::from_token(token);
        if active.jobs.get(&key).is_some_and(|job| &job.token == token) {
            active.jobs.remove(&key);
        }
        let idle = active.jobs.is_empty();
        drop(active);
        if idle {
            self.idle.notify_waiters();
        }
    }
}

impl RunAbort {
    fn from_record(error: UploadRecordError) -> Self {
        match error.kind() {
            UploadRecordErrorKind::AccountChanged => Self::AccountChanged,
            UploadRecordErrorKind::Superseded | UploadRecordErrorKind::Contended => {
                Self::Superseded
            }
            UploadRecordErrorKind::Persistence => Self::Persistence,
        }
    }

    const fn outcome(self) -> UploadJobOutcome {
        match self {
            Self::AccountChanged => UploadJobOutcome::AccountChanged,
            Self::Superseded => UploadJobOutcome::Superseded,
            Self::Persistence => UploadJobOutcome::Abandoned,
        }
    }
}

fn terminal_notice(token: &DurableUploadToken, suffix: &str, message: &str) -> UploadNotice {
    UploadNotice {
        id: format!(
            "cloud-upload-{}-{}-{suffix}",
            token.upload_generation,
            token.local_clip_id.as_str()
        ),
        message: message.into(),
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn validate_upload_record(record: &UploadRecord) -> Result<(), UploadRecordError> {
    let invalid =
        |message: String| UploadRecordError::new(UploadRecordErrorKind::Persistence, message);
    if record.path.is_empty() || record.path.len() > MAX_CLOUD_UPLOAD_PATH_BYTES {
        return Err(invalid(format!(
            "upload record path is {} bytes; expected 1..={MAX_CLOUD_UPLOAD_PATH_BYTES}",
            record.path.len()
        )));
    }
    if !matches!(
        record.visibility.as_str(),
        "private" | "public" | "unlisted"
    ) {
        return Err(invalid("upload record visibility is invalid".into()));
    }
    if !matches!(
        record.upload_status.as_str(),
        "queued"
            | "uploading"
            | "processing"
            | "uploaded_private"
            | "uploaded_public"
            | "uploaded_processing"
            | "failed"
            | "retrying"
            | "canceled"
    ) {
        return Err(invalid("upload record status is invalid".into()));
    }
    let status_matches_phase = match record.phase {
        UploadPhase::Queued | UploadPhase::Preparing => {
            matches!(record.upload_status.as_str(), "queued" | "retrying")
        }
        UploadPhase::Uploading => record.upload_status == "uploading",
        UploadPhase::Processing => record.upload_status == "processing",
        UploadPhase::Verifying | UploadPhase::DeletingLocal | UploadPhase::Abandoned => {
            record.upload_status == "uploaded_processing"
        }
        UploadPhase::Completed => {
            matches!(
                record.upload_status.as_str(),
                "uploaded_private" | "uploaded_public"
            )
        }
        UploadPhase::Canceled => record.upload_status == "canceled",
        UploadPhase::Failed => record.upload_status == "failed",
    };
    if !status_matches_phase {
        return Err(invalid(
            "upload record phase and durable status disagree".into(),
        ));
    }
    validate_optional_record_field(
        "remote clip id",
        record.remote_clip_id.as_deref(),
        MAX_CLOUD_UPLOAD_ID_BYTES,
    )?;
    validate_optional_record_field(
        "remote URL",
        record.remote_url.as_deref(),
        MAX_CLOUD_UPLOAD_URL_BYTES,
    )?;
    validate_optional_record_field(
        "error",
        record.error.as_deref(),
        MAX_CLOUD_UPLOAD_ERROR_BYTES,
    )?;
    if record.visibility == "private" && record.remote_url.is_some() {
        return Err(invalid(
            "private upload record must not retain a remote URL".into(),
        ));
    }
    if record.received_size_bytes > record.file_size_bytes {
        return Err(invalid(
            "upload record received bytes exceed its payload size".into(),
        ));
    }
    if record.phase == UploadPhase::Completed && record.remote_clip_id.is_none() {
        return Err(invalid(
            "completed upload record is missing its remote clip id".into(),
        ));
    }
    Ok(())
}

fn validate_optional_record_field(
    field: &'static str,
    value: Option<&str>,
    maximum: usize,
) -> Result<(), UploadRecordError> {
    if let Some(value) = value {
        if value.is_empty() || value.len() > maximum {
            return Err(UploadRecordError::new(
                UploadRecordErrorKind::Persistence,
                format!(
                    "upload record {field} is {} bytes; expected 1..={maximum}",
                    value.len()
                ),
            ));
        }
    }
    Ok(())
}

fn bounded_message(mut message: String) -> String {
    if message.len() <= MAX_CATALOG_STRING_BYTES {
        return message;
    }
    let mut boundary = MAX_CATALOG_STRING_BYTES;
    while boundary > 0 && !message.is_char_boundary(boundary) {
        boundary -= 1;
    }
    message.truncate(boundary);
    message
}
