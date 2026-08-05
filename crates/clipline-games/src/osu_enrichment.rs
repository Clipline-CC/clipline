//! Bounded osu! pending-enrichment discovery, mapping, and publication.

use std::collections::{HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::future::Future;
use std::io::{Read, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use clipline_events::{ClipMarkers, ClipPlay, GameId};
use clipline_library::{
    clip_sidecar_paths, parse_marker_sidecar_preserving_all, MutationLease, OsuEnrichmentStatus,
    OsuPendingEnrichment, OsuTitleEvent, MAX_CLIP_DETAIL_SIDECAR_BYTES, MAX_CLIP_SIDECAR_PLAYS,
    MAX_PENDING_OSU_BYTES,
};
use clipline_shell::{
    metadata_is_link_or_reparse_point, open_regular_file_nofollow, opened_file_identity,
    DirectoryAuthority, FileIdentity,
};

use crate::identity::OSU_ID;
use crate::osu_http::{
    OsuHttpClient, OsuHttpConfig, OsuHttpError, OsuHttpErrorKind, OsuHttpOwner, OsuProxyScore,
    OsuRecentFetch, OsuRequestFence,
};

pub const OSU_PENDING_SCHEMA_VERSION: u32 = 1;
pub const MAX_OSU_PENDING_BYTES: usize = MAX_PENDING_OSU_BYTES as usize;
pub const MAX_OSU_PENDING_JOBS: usize = 10_000;
pub const MAX_OSU_PENDING_SCAN_ENTRIES: usize = 50_000;
pub const MAX_OSU_TITLE_EVENTS: usize = 512;
pub const MAX_OSU_TITLE_BYTES: usize = 4 * 1024;
pub const MAX_OSU_TITLE_TOTAL_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_OSU_PENDING_MESSAGE_BYTES: usize = 64 * 1024;
pub const MAX_OSU_PENDING_PATH_BYTES: usize = 64 * 1024;
pub const MAX_ACTIVE_OSU_ENRICHMENT_ROOTS: usize = 8;

const SESSION_META_FILE: &str = "clipline-session.json";
const MAX_SESSION_META_BYTES: usize = 64 * 1024;
const UTC_SKEW_TOLERANCE_S: f64 = 15.0;
const PASSED_RESULTS_SCREEN_PADDING_S: f64 = 1.0;
const TITLE_EVENT_FALLBACK_LOOKBACK_S: i64 = 15 * 60;
const TITLE_EVENT_LENGTH_SLACK_S: i64 = 60;
const PENDING_RETRY_BASE_S: u64 = 60;
const PENDING_RETRY_CAP_S: u64 = 6 * 60 * 60;
const FAILED_RETRY_BASE_S: u64 = 6 * 60 * 60;
const FAILED_RETRY_CAP_S: u64 = 24 * 60 * 60;
const MAX_CLIP_DURATION_S: f64 = 7.0 * 24.0 * 60.0 * 60.0;

static OSU_SIDECAR_COUNTER: AtomicU64 = AtomicU64::new(0);
static ACTIVE_ROOTS: OnceLock<Mutex<HashSet<FileIdentity>>> = OnceLock::new();

const _: () = assert!(MAX_OSU_TITLE_EVENTS >= MAX_CLIP_SIDECAR_PLAYS);
const _: () = assert!(MAX_OSU_PENDING_BYTES <= MAX_CLIP_DETAIL_SIDECAR_BYTES);

#[derive(Debug, Clone)]
pub struct OsuSavedClip {
    pub path: PathBuf,
    pub seconds: f64,
    pub full_session: bool,
    pub recording_start_unix: Option<i64>,
    pub recording_end_unix: Option<i64>,
    pub title_events: Vec<OsuTitleEvent>,
}

/// A pending record and the exact filesystem objects that authorized it.
#[derive(Debug)]
pub struct DiscoveredPendingEnrichment {
    record: OsuPendingEnrichment,
    clip_path: PathBuf,
    clip_identity: FileIdentity,
    sidecar_path: PathBuf,
    sidecar_identity: FileIdentity,
    parent_authority: DirectoryAuthority,
    clip_name: OsString,
    sidecar_name: OsString,
}

#[derive(Debug)]
struct InvalidPendingEnrichment {
    parent_authority: DirectoryAuthority,
    sidecar_name: OsString,
    sidecar_identity: FileIdentity,
}

impl InvalidPendingEnrichment {
    fn quarantine(self) -> Result<PathBuf, OsuEnrichmentError> {
        for _ in 0..64 {
            let counter = OSU_SIDECAR_COUNTER.fetch_add(1, Ordering::Relaxed);
            let mut quarantine_name = self.sidecar_name.clone();
            quarantine_name.push(format!(".invalid.{}.{counter}", std::process::id()));
            match self.parent_authority.rename_file_noreplace_if_identity(
                &self.sidecar_name,
                &quarantine_name,
                self.sidecar_identity,
            ) {
                Ok(()) => {
                    return Ok(self.parent_authority.display_path().join(quarantine_name));
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::AlreadyExists
                        && !error.may_have_moved() =>
                {
                    continue;
                }
                Err(_) => {
                    return Err(OsuEnrichmentError::new(
                        OsuEnrichmentErrorKind::StaleFile,
                        "quarantine exact invalid osu! enrichment sidecar",
                    ));
                }
            }
        }
        Err(OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::TooLarge,
            "allocate invalid osu! enrichment quarantine name",
        ))
    }
}

struct PendingDiscovery {
    jobs: Vec<DiscoveredPendingEnrichment>,
    invalid: Vec<InvalidPendingEnrichment>,
}

impl DiscoveredPendingEnrichment {
    #[must_use]
    pub const fn record(&self) -> &OsuPendingEnrichment {
        &self.record
    }

    #[must_use]
    pub fn clip_path(&self) -> &Path {
        &self.clip_path
    }

    #[must_use]
    pub fn sidecar_path(&self) -> &Path {
        &self.sidecar_path
    }

    #[must_use]
    pub fn retry_due(&self, now_unix: u64) -> bool {
        let modified_unix = std::fs::metadata(&self.sidecar_path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_secs());
        retry_is_due(
            self.record.status.clone(),
            self.record.attempts,
            modified_unix,
            now_unix,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsuEnrichmentErrorKind {
    InvalidInput,
    TooLarge,
    Allocation,
    UnsafePath,
    StaleFile,
    Io,
    Malformed,
    AlreadyRunning,
    AccountChanged,
    Canceled,
    Http,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{context}")]
pub struct OsuEnrichmentError {
    kind: OsuEnrichmentErrorKind,
    context: &'static str,
    #[source]
    http: Option<OsuHttpError>,
}

impl OsuEnrichmentError {
    const fn new(kind: OsuEnrichmentErrorKind, context: &'static str) -> Self {
        Self {
            kind,
            context,
            http: None,
        }
    }

    fn http(error: OsuHttpError) -> Self {
        let kind = match error.kind() {
            OsuHttpErrorKind::AccountChanged => OsuEnrichmentErrorKind::AccountChanged,
            OsuHttpErrorKind::Canceled => OsuEnrichmentErrorKind::Canceled,
            _ => OsuEnrichmentErrorKind::Http,
        };
        Self {
            kind,
            context: "fetch osu! enrichment scores",
            http: Some(error),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> OsuEnrichmentErrorKind {
        self.kind
    }

    #[must_use]
    pub const fn account_changed() -> Self {
        Self::new(
            OsuEnrichmentErrorKind::AccountChanged,
            "fence osu! enrichment account",
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OsuMappedPlays {
    pub plays: Vec<ClipPlay>,
    pub pagination_ceiling_reached: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsuEnrichmentSummary {
    pub owner: OsuHttpOwner,
    pub discovered: usize,
    pub attempted: usize,
    pub updated: usize,
    pub retry_scheduled: usize,
    pub failed: usize,
    pub pagination_ceiling_reached: bool,
}

pub type OsuScoreFetchFuture<'a> =
    Pin<Box<dyn Future<Output = Result<OsuRecentFetch, OsuHttpError>> + Send + 'a>>;

pub trait OsuScoreFetchPort: Send + Sync {
    fn owner(&self) -> OsuHttpOwner;

    fn fetch<'a>(
        &'a self,
        stop_before_unix: Option<i64>,
        fence: &'a dyn OsuRequestFence,
    ) -> OsuScoreFetchFuture<'a>;
}

pub trait OsuEnrichmentFence: OsuRequestFence {
    /// Serialize an account-current check with one short durable publication.
    /// Implementations use the same gate as save/test/disconnect; the closure
    /// must never perform network I/O or wait for another thread.
    fn publish_if_current(
        &self,
        owner: OsuHttpOwner,
        publish: &mut dyn FnMut() -> Result<(), OsuEnrichmentError>,
    ) -> Result<(), OsuEnrichmentError>;
}

/// Durable account fence shared by Tauri and Slint enrichment executors.
/// Cleanup-only reconciliation remains current; save/test/disconnect and ABA
/// replacement invalidate both HTTP work and sidecar publication.
pub struct SettingsOsuEnrichmentFence {
    store: clipline_settings::SettingsStore,
    expected: clipline_settings::OsuApiSettings,
    publication: Mutex<()>,
    canceled: AtomicBool,
    canceled_notify: tokio::sync::Notify,
}

impl SettingsOsuEnrichmentFence {
    #[must_use]
    pub fn new(
        store: clipline_settings::SettingsStore,
        expected: clipline_settings::OsuApiSettings,
    ) -> Self {
        Self {
            store,
            expected,
            publication: Mutex::new(()),
            canceled: AtomicBool::new(false),
            canceled_notify: tokio::sync::Notify::new(),
        }
    }

    #[must_use]
    pub const fn owner(&self) -> OsuHttpOwner {
        OsuHttpOwner::new(self.expected.account_generation)
    }

    pub fn cancel(&self) {
        let _publication = self
            .publication
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.canceled.store(true, Ordering::Release);
        self.canceled_notify.notify_waiters();
        // Preserve one permit for a waiter that races the check-before-await
        // boundary after `notify_waiters` has returned.
        self.canceled_notify.notify_one();
    }
}

impl OsuRequestFence for SettingsOsuEnrichmentFence {
    fn is_current(&self, owner: OsuHttpOwner) -> bool {
        if self.canceled.load(Ordering::Acquire) || owner != self.owner() {
            return false;
        }
        self.store
            .current_osu_profile()
            .is_ok_and(|current| same_profile_owner(&current, &self.expected))
    }

    fn cancelled<'a>(&'a self, owner: OsuHttpOwner) -> crate::osu_http::OsuCancellationFuture<'a> {
        Box::pin(async move {
            if self.canceled.load(Ordering::Acquire) || owner != self.owner() {
                return;
            }
            let notified = self.canceled_notify.notified();
            if self.canceled.load(Ordering::Acquire) || owner != self.owner() {
                return;
            }
            notified.await;
        })
    }
}

impl OsuEnrichmentFence for SettingsOsuEnrichmentFence {
    fn publish_if_current(
        &self,
        owner: OsuHttpOwner,
        publish: &mut dyn FnMut() -> Result<(), OsuEnrichmentError>,
    ) -> Result<(), OsuEnrichmentError> {
        let _publication = self
            .publication
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.canceled.load(Ordering::Acquire) || owner != self.owner() {
            return Err(OsuEnrichmentError::account_changed());
        }
        self.store
            .publish_if_osu_profile_current(&self.expected, publish)
            .map_err(|_| OsuEnrichmentError::account_changed())?
    }
}

fn same_profile_owner(
    current: &clipline_settings::OsuApiSettings,
    expected: &clipline_settings::OsuApiSettings,
) -> bool {
    current.account_generation == expected.account_generation
        && current.client_id == expected.client_id
        && current.user == expected.user
        && current.credential_target == expected.credential_target
}

pub struct ConfiguredOsuScoreFetcher {
    client: OsuHttpClient,
    config: OsuHttpConfig,
}

impl ConfiguredOsuScoreFetcher {
    #[must_use]
    pub const fn new(client: OsuHttpClient, config: OsuHttpConfig) -> Self {
        Self { client, config }
    }
}

impl OsuScoreFetchPort for ConfiguredOsuScoreFetcher {
    fn owner(&self) -> OsuHttpOwner {
        self.config.owner()
    }

    fn fetch<'a>(
        &'a self,
        stop_before_unix: Option<i64>,
        fence: &'a dyn OsuRequestFence,
    ) -> OsuScoreFetchFuture<'a> {
        Box::pin(
            self.client
                .fetch_recent_scores(&self.config, stop_before_unix, fence),
        )
    }
}

pub struct OsuEnrichmentService<F> {
    fetcher: F,
    mutation_lease: Arc<dyn MutationLease>,
}

impl<F> OsuEnrichmentService<F>
where
    F: OsuScoreFetchPort,
{
    #[must_use]
    pub fn new(fetcher: F, mutation_lease: Arc<dyn MutationLease>) -> Self {
        Self {
            fetcher,
            mutation_lease,
        }
    }

    pub async fn run<T: OsuEnrichmentFence>(
        &self,
        media_root: &Path,
        now_unix: u64,
        fence: &T,
    ) -> Result<OsuEnrichmentSummary, OsuEnrichmentError> {
        self.run_exact(media_root, None, now_unix, fence).await
    }

    async fn run_exact(
        &self,
        media_root: &Path,
        expected_root: Option<FileIdentity>,
        now_unix: u64,
        fence: &dyn OsuEnrichmentFence,
    ) -> Result<OsuEnrichmentSummary, OsuEnrichmentError> {
        let owner = self.fetcher.owner();
        checkpoint(fence, owner)?;
        let Some(_lease) = EnrichmentPassLease::try_acquire(media_root, expected_root)? else {
            return Err(OsuEnrichmentError::new(
                OsuEnrichmentErrorKind::AlreadyRunning,
                "run one osu! enrichment pass per media root",
            ));
        };
        checkpoint(fence, owner)?;
        let PendingDiscovery { jobs, invalid } = discover_pending_exact(media_root)?;
        for invalid in invalid {
            checkpoint(fence, owner)?;
            let mut invalid = Some(invalid);
            let mut quarantine = || {
                invalid
                    .take()
                    .expect("quarantine publication closure runs at most once")
                    .quarantine()
                    .map(|_| ())
            };
            match fence.publish_if_current(owner, &mut quarantine) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        OsuEnrichmentErrorKind::AccountChanged | OsuEnrichmentErrorKind::Canceled
                    ) =>
                {
                    return Err(error);
                }
                Err(error) => tracing::warn!(
                    event = "invalid_osu_enrichment_quarantine_failed",
                    kind = ?error.kind()
                ),
            }
        }
        let mut due = Vec::new();
        due.try_reserve_exact(jobs.len()).map_err(|_| {
            OsuEnrichmentError::new(
                OsuEnrichmentErrorKind::Allocation,
                "reserve due osu! enrichment jobs",
            )
        })?;
        due.extend(jobs.into_iter().filter(|job| job.retry_due(now_unix)));
        let mut summary = OsuEnrichmentSummary {
            owner,
            discovered: due.len(),
            attempted: 0,
            updated: 0,
            retry_scheduled: 0,
            failed: 0,
            pagination_ceiling_reached: false,
        };
        if due.is_empty() {
            return Ok(summary);
        }
        let earliest = due.iter().map(|job| job.record.recording_start_unix).min();
        let fetch = match self.fetcher.fetch(earliest, fence).await {
            Ok(fetch) => fetch,
            Err(error) => {
                if matches!(
                    error.kind(),
                    OsuHttpErrorKind::Canceled | OsuHttpErrorKind::AccountChanged
                ) {
                    return Err(OsuEnrichmentError::http(error));
                }
                for job in &due {
                    checkpoint(fence, owner)?;
                    let _permit = match self
                        .mutation_lease
                        .acquire(&job.clip_path, job.clip_identity)
                    {
                        Ok(permit) => permit,
                        Err(_) => continue,
                    };
                    let mut publish =
                        || mark_pending_retry(job, "osu! API fetch failed; retrying later");
                    let _ = fence.publish_if_current(owner, &mut publish);
                }
                return Err(OsuEnrichmentError::http(error));
            }
        };
        if fetch.owner != owner {
            return Err(OsuEnrichmentError::new(
                OsuEnrichmentErrorKind::AccountChanged,
                "reject stale osu! enrichment fetch",
            ));
        }
        checkpoint(fence, owner)?;
        summary.pagination_ceiling_reached = fetch.pagination_ceiling_reached;
        for job in &due {
            checkpoint(fence, owner)?;
            summary.attempted += 1;
            let _permit = match self
                .mutation_lease
                .acquire(&job.clip_path, job.clip_identity)
            {
                Ok(permit) => permit,
                Err(_) => {
                    summary.retry_scheduled += 1;
                    continue;
                }
            };
            let mapped = map_proxy_scores_to_clip_plays(
                &job.record,
                &fetch.scores,
                fetch.pagination_ceiling_reached,
            );
            let mapped = match mapped {
                Ok(mapped) => mapped,
                Err(error) => {
                    let mut publish = || mark_pending_failed(job, "osu! score mapping failed");
                    if fence.publish_if_current(owner, &mut publish).is_ok() {
                        summary.failed += 1;
                    }
                    tracing::warn!(event = "osu_enrichment_mapping_failed", kind = ?error.kind());
                    continue;
                }
            };
            if mapped.plays.is_empty() {
                let mut publish = || {
                    mark_pending_retry(
                        job,
                        "No osu! API plays matched this recording yet; keeping fallback plays and retrying later.",
                    )
                };
                fence.publish_if_current(owner, &mut publish)?;
                summary.retry_scheduled += 1;
                continue;
            }
            let mut publish_markers = Some(mapped.plays);
            let mut publish = || {
                write_plays_sidecar(
                    job,
                    publish_markers
                        .take()
                        .expect("publication closure runs at most once"),
                )
                .map(|_| ())
            };
            fence.publish_if_current(owner, &mut publish)?;
            summary.updated += 1;
            let mut remove = || remove_pending(job);
            if fence.publish_if_current(owner, &mut remove).is_err() {
                // Marker publication is the durable completion point. Leaving
                // the exact pending sidecar schedules idempotent cleanup/retry.
                summary.retry_scheduled += 1;
            }
        }
        Ok(summary)
    }
}

pub type OsuEnrichmentRunFuture<'a> =
    Pin<Box<dyn Future<Output = Result<OsuEnrichmentSummary, OsuEnrichmentError>> + Send + 'a>>;

/// One owned enrichment pass suitable for the joined process coordinator.
pub trait OsuEnrichmentPass: Send + Sync + 'static {
    fn run<'a>(
        &'a self,
        media_root: &'a Path,
        expected_root: FileIdentity,
        now_unix: u64,
        fence: &'a dyn OsuEnrichmentFence,
    ) -> OsuEnrichmentRunFuture<'a>;
}

impl<F> OsuEnrichmentPass for OsuEnrichmentService<F>
where
    F: OsuScoreFetchPort + 'static,
{
    fn run<'a>(
        &'a self,
        media_root: &'a Path,
        expected_root: FileIdentity,
        now_unix: u64,
        fence: &'a dyn OsuEnrichmentFence,
    ) -> OsuEnrichmentRunFuture<'a> {
        Box::pin(self.run_exact(media_root, Some(expected_root), now_unix, fence))
    }
}

#[derive(Debug)]
pub enum JoinedOsuEnrichmentOutcome {
    Completed(OsuEnrichmentSummary),
    Failed(OsuEnrichmentError),
    Panicked,
    Superseded,
    ShutDown,
}

pub struct JoinedOsuEnrichmentHandle {
    receiver: mpsc::Receiver<JoinedOsuEnrichmentOutcome>,
}

impl JoinedOsuEnrichmentHandle {
    pub fn recv(self) -> Result<JoinedOsuEnrichmentOutcome, mpsc::RecvError> {
        self.receiver.recv()
    }

    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<JoinedOsuEnrichmentOutcome, mpsc::RecvTimeoutError> {
        self.receiver.recv_timeout(timeout)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum JoinedOsuEnrichmentSubmitError {
    #[error("osu! enrichment coordinator is shut down")]
    ShutDown,
    #[error("osu! enrichment root is unsafe")]
    UnsafeRoot,
    #[error("too many osu! enrichment roots are active")]
    Full,
    #[error("osu! enrichment root path is too large")]
    TooLarge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum JoinedOsuEnrichmentServiceError {
    #[error("could not start the osu! enrichment coordinator")]
    Spawn,
    #[error("the osu! enrichment coordinator worker panicked")]
    WorkerPanicked,
}

struct JoinedRequest {
    root: PathBuf,
    root_identity: FileIdentity,
    _authority: DirectoryAuthority,
    now_unix: u64,
    pass: Arc<dyn OsuEnrichmentPass>,
    fence: Arc<dyn OsuEnrichmentFence>,
    result: mpsc::SyncSender<JoinedOsuEnrichmentOutcome>,
}

struct JoinedRootSlot {
    identity: FileIdentity,
    pending: Option<JoinedRequest>,
}

#[derive(Default)]
struct JoinedState {
    shut_down: bool,
    running: Option<FileIdentity>,
    slots: Vec<JoinedRootSlot>,
    ready: VecDeque<JoinedRequest>,
}

struct JoinedShared {
    state: Mutex<JoinedState>,
    changed: Condvar,
}

struct JoinedCore {
    shared: Arc<JoinedShared>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl JoinedCore {
    fn stop(&self) -> Result<(), JoinedOsuEnrichmentServiceError> {
        stop_joined_state(&self.shared);
        self.worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .map_or(Ok(()), |worker| {
                worker
                    .join()
                    .map_err(|_| JoinedOsuEnrichmentServiceError::WorkerPanicked)
            })
    }
}

impl Drop for JoinedCore {
    fn drop(&mut self) {
        stop_joined_state(&self.shared);
        if let Some(worker) = self
            .worker
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = worker.join();
        }
    }
}

/// One joined coordinator worker with at most one queued latest request per
/// media-root identity. Replacement drops no trigger silently: the displaced
/// caller receives `Superseded`, and the latest request runs after the current
/// pass completes.
#[derive(Clone)]
pub struct JoinedOsuEnrichmentService {
    core: Arc<JoinedCore>,
}

impl JoinedOsuEnrichmentService {
    pub fn start() -> Result<Self, JoinedOsuEnrichmentServiceError> {
        let shared = Arc::new(JoinedShared {
            state: Mutex::new(JoinedState::default()),
            changed: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name("clipline-osu-enrichment".into())
            .spawn(move || joined_worker(worker_shared))
            .map_err(|_| JoinedOsuEnrichmentServiceError::Spawn)?;
        Ok(Self {
            core: Arc::new(JoinedCore {
                shared,
                worker: Mutex::new(Some(worker)),
            }),
        })
    }

    pub fn submit(
        &self,
        media_root: &Path,
        now_unix: u64,
        pass: Arc<dyn OsuEnrichmentPass>,
        fence: Arc<dyn OsuEnrichmentFence>,
    ) -> Result<JoinedOsuEnrichmentHandle, JoinedOsuEnrichmentSubmitError> {
        let root_bytes = media_root.as_os_str().to_string_lossy().len();
        if root_bytes == 0 || root_bytes > MAX_OSU_PENDING_PATH_BYTES {
            return Err(JoinedOsuEnrichmentSubmitError::TooLarge);
        }
        let authority = DirectoryAuthority::open(media_root)
            .map_err(|_| JoinedOsuEnrichmentSubmitError::UnsafeRoot)?;
        let identity = authority.identity();
        let (result, receiver) = mpsc::sync_channel(1);
        let request = JoinedRequest {
            root: media_root.to_path_buf(),
            root_identity: identity,
            _authority: authority,
            now_unix,
            pass,
            fence,
            result,
        };
        let mut state = self
            .core
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.shut_down {
            return Err(JoinedOsuEnrichmentSubmitError::ShutDown);
        }
        if let Some(slot) = state
            .slots
            .iter_mut()
            .find(|slot| slot.identity == identity)
        {
            if let Some(displaced) = slot.pending.replace(request) {
                let _ = displaced
                    .result
                    .try_send(JoinedOsuEnrichmentOutcome::Superseded);
            }
        } else {
            if state.slots.len() >= MAX_ACTIVE_OSU_ENRICHMENT_ROOTS {
                return Err(JoinedOsuEnrichmentSubmitError::Full);
            }
            state.slots.push(JoinedRootSlot {
                identity,
                pending: None,
            });
            state.ready.push_back(request);
            self.core.shared.changed.notify_one();
        }
        Ok(JoinedOsuEnrichmentHandle { receiver })
    }

    pub fn shutdown(&self) -> Result<(), JoinedOsuEnrichmentServiceError> {
        self.core.stop()
    }
}

fn joined_worker(shared: Arc<JoinedShared>) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            stop_joined_state(&shared);
            return;
        }
    };
    loop {
        let request = {
            let mut state = shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while state.ready.is_empty() && !state.shut_down {
                state = shared
                    .changed
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            let Some(request) = state.ready.pop_front() else {
                break;
            };
            state.running = Some(request.root_identity);
            request
        };
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            runtime.block_on(request.pass.run(
                &request.root,
                request.root_identity,
                request.now_unix,
                request.fence.as_ref(),
            ))
        }));
        let _ = request.result.try_send(match outcome {
            Ok(Ok(summary)) => JoinedOsuEnrichmentOutcome::Completed(summary),
            Ok(Err(error)) => JoinedOsuEnrichmentOutcome::Failed(error),
            Err(_) => JoinedOsuEnrichmentOutcome::Panicked,
        });

        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.running = None;
        let position = state
            .slots
            .iter()
            .position(|slot| slot.identity == request.root_identity);
        if let Some(position) = position {
            if state.shut_down {
                if let Some(pending) = state.slots[position].pending.take() {
                    let _ = pending
                        .result
                        .try_send(JoinedOsuEnrichmentOutcome::ShutDown);
                }
                state.slots.remove(position);
            } else if let Some(pending) = state.slots[position].pending.take() {
                state.ready.push_back(pending);
                shared.changed.notify_one();
            } else {
                state.slots.remove(position);
            }
        }
        if state.shut_down && state.ready.is_empty() {
            break;
        }
    }
}

fn stop_joined_state(shared: &JoinedShared) {
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if state.shut_down {
        shared.changed.notify_all();
        return;
    }
    state.shut_down = true;
    for request in state.ready.drain(..) {
        let _ = request
            .result
            .try_send(JoinedOsuEnrichmentOutcome::ShutDown);
    }
    for slot in &mut state.slots {
        if let Some(request) = slot.pending.take() {
            let _ = request
                .result
                .try_send(JoinedOsuEnrichmentOutcome::ShutDown);
        }
    }
    if state.running.is_none() {
        state.slots.clear();
    }
    shared.changed.notify_all();
}

struct EnrichmentPassLease {
    identity: FileIdentity,
    _authority: DirectoryAuthority,
}

impl EnrichmentPassLease {
    fn try_acquire(
        root: &Path,
        expected: Option<FileIdentity>,
    ) -> Result<Option<Self>, OsuEnrichmentError> {
        let authority = DirectoryAuthority::open(root).map_err(|_| {
            OsuEnrichmentError::new(
                OsuEnrichmentErrorKind::UnsafePath,
                "open osu! enrichment media root",
            )
        })?;
        let identity = authority.identity();
        if expected.is_some_and(|expected| expected != identity) {
            return Err(OsuEnrichmentError::new(
                OsuEnrichmentErrorKind::StaleFile,
                "fence osu! enrichment media root identity",
            ));
        }
        let mut active = ACTIVE_ROOTS
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active.contains(&identity) {
            return Ok(None);
        }
        if active.len() >= MAX_ACTIVE_OSU_ENRICHMENT_ROOTS {
            return Err(OsuEnrichmentError::new(
                OsuEnrichmentErrorKind::TooLarge,
                "admit osu! enrichment media root",
            ));
        }
        active.insert(identity);
        Ok(Some(Self {
            identity,
            _authority: authority,
        }))
    }
}

impl Drop for EnrichmentPassLease {
    fn drop(&mut self) {
        ACTIVE_ROOTS
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.identity);
    }
}

#[must_use]
pub fn pending_path(path: &Path) -> PathBuf {
    clip_sidecar_paths(path).pending_osu
}

pub fn write_pending_for_saved_clip(
    saved: &OsuSavedClip,
    mutation_lease: &dyn MutationLease,
) -> Result<Option<PathBuf>, OsuEnrichmentError> {
    if !saved.full_session || !clip_session_is_osu(&saved.path) {
        return Ok(None);
    }
    if !saved.seconds.is_finite() || !(0.0..=MAX_CLIP_DURATION_S).contains(&saved.seconds) {
        return Err(OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::InvalidInput,
            "validate saved osu! clip duration",
        ));
    }
    if saved.path.as_os_str().to_string_lossy().len() > MAX_OSU_PENDING_PATH_BYTES {
        return Err(OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::TooLarge,
            "validate saved osu! clip path",
        ));
    }
    validate_title_events(&saved.title_events)?;
    let parent = saved.path.parent().ok_or_else(|| {
        OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::UnsafePath,
            "validate saved osu! clip parent",
        )
    })?;
    let clip_name = saved.path.file_name().ok_or_else(|| {
        OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::UnsafePath,
            "validate saved osu! clip name",
        )
    })?;
    let path = pending_path(&saved.path);
    let sidecar_name = path.file_name().ok_or_else(|| {
        OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::UnsafePath,
            "validate saved osu! sidecar name",
        )
    })?;
    let parent_authority = DirectoryAuthority::open(parent).map_err(|_| {
        OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::UnsafePath,
            "retain saved osu! clip parent",
        )
    })?;
    let clip_identity = parent_authority
        .regular_file_identity(clip_name)
        .map_err(|_| {
            OsuEnrichmentError::new(
                OsuEnrichmentErrorKind::UnsafePath,
                "identify saved osu! clip",
            )
        })?
        .ok_or_else(|| {
            OsuEnrichmentError::new(OsuEnrichmentErrorKind::UnsafePath, "open saved osu! clip")
        })?;
    let _permit = mutation_lease
        .acquire(&saved.path, clip_identity)
        .map_err(|_| {
            OsuEnrichmentError::new(
                OsuEnrichmentErrorKind::StaleFile,
                "lease saved osu! clip mutation",
            )
        })?;
    let end = saved.recording_end_unix.unwrap_or_else(unix_now_i64);
    let derived_start = end.saturating_sub(saved.seconds.max(0.0).round() as i64);
    let start = saved.recording_start_unix.unwrap_or(derived_start);
    let record = OsuPendingEnrichment {
        schema_version: OSU_PENDING_SCHEMA_VERSION,
        clip_path: saved.path.display().to_string(),
        recording_start_unix: start,
        recording_end_unix: end.max(start),
        clip_duration_s: saved.seconds,
        status: OsuEnrichmentStatus::Pending,
        attempts: 0,
        pagination_ceiling_reached: false,
        title_events: saved.title_events.clone(),
        message: None,
    };
    validate_pending(&record)?;
    ensure_authority_identity(&parent_authority, clip_name, clip_identity)?;
    let expected = regular_identity(&parent_authority, sidecar_name)?;
    publish_json(
        &parent_authority,
        sidecar_name,
        expected,
        &record,
        MAX_OSU_PENDING_BYTES,
    )?;
    let sidecar_identity = regular_identity(&parent_authority, sidecar_name)?.ok_or_else(|| {
        OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::StaleFile,
            "identify published osu! enrichment sidecar",
        )
    })?;
    let discovered = DiscoveredPendingEnrichment {
        record,
        clip_path: saved.path.clone(),
        clip_identity,
        sidecar_path: path.clone(),
        sidecar_identity,
        parent_authority,
        clip_name: clip_name.to_os_string(),
        sidecar_name: sidecar_name.to_os_string(),
    };
    let title_plays = map_title_events_to_clip_plays(&discovered.record)?;
    if !title_plays.is_empty() {
        let _ = write_plays_sidecar(&discovered, title_plays)?;
    }
    Ok(Some(path))
}

pub fn discover_pending(
    media_root: &Path,
) -> Result<Vec<DiscoveredPendingEnrichment>, OsuEnrichmentError> {
    let PendingDiscovery { jobs, invalid } = discover_pending_exact(media_root)?;
    for invalid in invalid {
        match invalid.quarantine() {
            Ok(path) => tracing::warn!(
                event = "invalid_osu_enrichment_quarantined",
                path = %path.display()
            ),
            Err(error) => tracing::warn!(
                event = "invalid_osu_enrichment_quarantine_failed",
                kind = ?error.kind()
            ),
        }
    }
    Ok(jobs)
}

fn discover_pending_exact(media_root: &Path) -> Result<PendingDiscovery, OsuEnrichmentError> {
    let root_authority = DirectoryAuthority::open(media_root).map_err(|_| {
        OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::UnsafePath,
            "open osu! enrichment media root",
        )
    })?;
    let media_root = media_root.canonicalize().map_err(|_| {
        OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::UnsafePath,
            "canonicalize osu! enrichment media root",
        )
    })?;
    if DirectoryAuthority::open(&media_root)
        .map(|current| current.identity())
        .ok()
        != Some(root_authority.identity())
    {
        return Err(OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::StaleFile,
            "fence osu! enrichment media root",
        ));
    }
    let mut out = Vec::new();
    out.try_reserve_exact(MAX_OSU_PENDING_JOBS).map_err(|_| {
        OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::Allocation,
            "reserve pending osu! enrichment jobs",
        )
    })?;
    let mut invalid = Vec::new();
    invalid
        .try_reserve_exact(MAX_OSU_PENDING_JOBS)
        .map_err(|_| {
            OsuEnrichmentError::new(
                OsuEnrichmentErrorKind::Allocation,
                "reserve invalid pending osu! enrichment jobs",
            )
        })?;
    let mut scanned = 0usize;
    discover_pending_in_dir(
        &media_root,
        &media_root,
        &mut out,
        &mut invalid,
        &mut scanned,
    )?;
    let entries = std::fs::read_dir(&media_root).map_err(|_| {
        OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::Io,
            "read osu! enrichment media root",
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|_| {
            OsuEnrichmentError::new(
                OsuEnrichmentErrorKind::Io,
                "read osu! enrichment root entry",
            )
        })?;
        bump_scanned(&mut scanned)?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path).map_err(|_| {
            OsuEnrichmentError::new(
                OsuEnrichmentErrorKind::Io,
                "inspect osu! enrichment root entry",
            )
        })?;
        if metadata.is_dir() && !metadata_is_link_or_reparse_point(&metadata) {
            discover_pending_in_dir(&media_root, &path, &mut out, &mut invalid, &mut scanned)?;
        }
    }
    out.sort_by(|a, b| {
        a.record
            .recording_start_unix
            .cmp(&b.record.recording_start_unix)
            .then_with(|| a.clip_path.cmp(&b.clip_path))
    });
    Ok(PendingDiscovery { jobs: out, invalid })
}

pub fn apply_scores_to_pending(
    pending: &DiscoveredPendingEnrichment,
    scores: &[OsuProxyScore],
    pagination_ceiling_reached: bool,
    mutation_lease: &dyn MutationLease,
) -> Result<OsuMappedPlays, OsuEnrichmentError> {
    let _permit = mutation_lease
        .acquire(&pending.clip_path, pending.clip_identity)
        .map_err(|_| {
            OsuEnrichmentError::new(
                OsuEnrichmentErrorKind::StaleFile,
                "lease pending osu! clip mutation",
            )
        })?;
    let mut mapped =
        map_proxy_scores_to_clip_plays(&pending.record, scores, pagination_ceiling_reached)?;
    if mapped.plays.is_empty() {
        mark_pending_retry(
            pending,
            "No osu! API plays matched this recording yet; keeping fallback plays and retrying later.",
        )?;
        return Ok(mapped);
    }
    mapped.plays = write_plays_sidecar(pending, mapped.plays)?;
    remove_pending(pending)?;
    Ok(mapped)
}

pub fn mark_pending_retry(
    pending: &DiscoveredPendingEnrichment,
    message: &str,
) -> Result<(), OsuEnrichmentError> {
    if message.len() > MAX_OSU_PENDING_MESSAGE_BYTES {
        return Err(OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::TooLarge,
            "validate osu! enrichment retry message",
        ));
    }
    ensure_pending_clip(pending)?;
    ensure_pending_sidecar(pending)?;
    let mut next = pending.record.clone();
    next.status = OsuEnrichmentStatus::Pending;
    next.attempts = next.attempts.saturating_add(1);
    next.message = Some(message.to_owned());
    validate_pending(&next)?;
    publish_json(
        &pending.parent_authority,
        &pending.sidecar_name,
        Some(pending.sidecar_identity),
        &next,
        MAX_OSU_PENDING_BYTES,
    )
}

pub fn mark_pending_failed(
    pending: &DiscoveredPendingEnrichment,
    message: &str,
) -> Result<(), OsuEnrichmentError> {
    if message.len() > MAX_OSU_PENDING_MESSAGE_BYTES {
        return Err(OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::TooLarge,
            "validate osu! enrichment failure message",
        ));
    }
    ensure_pending_clip(pending)?;
    ensure_pending_sidecar(pending)?;
    let mut next = pending.record.clone();
    next.status = OsuEnrichmentStatus::Failed;
    next.attempts = next.attempts.saturating_add(1);
    next.message = Some(message.to_owned());
    validate_pending(&next)?;
    publish_json(
        &pending.parent_authority,
        &pending.sidecar_name,
        Some(pending.sidecar_identity),
        &next,
        MAX_OSU_PENDING_BYTES,
    )
}

pub fn map_proxy_scores_to_clip_plays(
    pending: &OsuPendingEnrichment,
    scores: &[OsuProxyScore],
    pagination_ceiling_reached: bool,
) -> Result<OsuMappedPlays, OsuEnrichmentError> {
    validate_pending(pending)?;
    if scores.len() > crate::osu_http::OSU_RECENT_SCORE_CEILING {
        return Err(OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::TooLarge,
            "validate osu! score input",
        ));
    }
    let mut sorted = Vec::new();
    sorted.try_reserve_exact(scores.len()).map_err(|_| {
        OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::Allocation,
            "reserve sorted osu! scores",
        )
    })?;
    for score in scores {
        score.validate_bounds().map_err(OsuEnrichmentError::http)?;
        sorted.push(score);
    }
    sorted.sort_by_key(|score| score.ended_at_unix);
    let mut seen = HashSet::new();
    seen.try_reserve(scores.len()).map_err(|_| {
        OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::Allocation,
            "reserve osu! score identities",
        )
    })?;
    let mut plays = Vec::new();
    plays
        .try_reserve_exact(scores.len().min(MAX_CLIP_SIDECAR_PLAYS))
        .map_err(|_| {
            OsuEnrichmentError::new(
                OsuEnrichmentErrorKind::Allocation,
                "reserve osu! mapped plays",
            )
        })?;
    let mut last_end_s = 0.0_f64;
    for score in sorted {
        if !seen.insert(score.id.as_str()) {
            continue;
        }
        let Some((start_unix, derived_start, point_marker)) = score_start_unix(score, pending)
        else {
            continue;
        };
        let score_start = start_unix as f64;
        let score_end = score.ended_at_unix as f64;
        if score_end < pending.recording_start_unix as f64 - UTC_SKEW_TOLERANCE_S
            || score_start > pending.recording_end_unix as f64 + UTC_SKEW_TOLERANCE_S
        {
            continue;
        }
        if plays.len() >= MAX_CLIP_SIDECAR_PLAYS {
            return Err(OsuEnrichmentError::new(
                OsuEnrichmentErrorKind::TooLarge,
                "retain osu! mapped plays",
            ));
        }
        let end_padding_s = if score.passed && !point_marker {
            PASSED_RESULTS_SCREEN_PADDING_S
        } else {
            0.0
        };
        let clip_end_s = clamp_clip_time(
            score_end - pending.recording_start_unix as f64 + end_padding_s,
            pending,
        );
        let mut clip_start_s =
            clamp_clip_time(score_start - pending.recording_start_unix as f64, pending);
        if derived_start && !point_marker && clip_start_s < last_end_s {
            clip_start_s = last_end_s;
        }
        let t_end_s = (!point_marker).then(|| clip_end_s.max(clip_start_s));
        last_end_s = last_end_s.max(t_end_s.unwrap_or(clip_start_s));
        plays.push(ClipPlay {
            game_id: GameId::Osu,
            source: "osu_api".into(),
            external_id: score.id.clone(),
            url: score.url.clone(),
            beatmap_id: score.beatmap_id,
            beatmapset_id: score.beatmapset_id,
            cover_url: score.cover_url.clone(),
            title: score.title.clone(),
            artist: score.artist.clone(),
            difficulty: score.difficulty.clone(),
            mapper: score.mapper.clone(),
            star_rating: score.star_rating,
            mods: score.mods.clone(),
            rank: score.rank.clone(),
            passed: score.passed,
            accuracy: score.accuracy,
            max_combo: score.max_combo,
            total_score: score.total_score,
            pp: score.pp,
            started_at: score.started_at_unix.map(unix_to_rfc3339).transpose()?,
            ended_at: unix_to_rfc3339(score.ended_at_unix)?,
            derived_start,
            t_start_s: clip_start_s,
            t_end_s,
        });
    }
    Ok(OsuMappedPlays {
        plays,
        pagination_ceiling_reached,
    })
}

fn map_title_events_to_clip_plays(
    pending: &OsuPendingEnrichment,
) -> Result<Vec<ClipPlay>, OsuEnrichmentError> {
    validate_pending(pending)?;
    let mut plays = Vec::new();
    plays
        .try_reserve_exact(pending.title_events.len().min(MAX_CLIP_SIDECAR_PLAYS))
        .map_err(|_| {
            OsuEnrichmentError::new(
                OsuEnrichmentErrorKind::Allocation,
                "reserve osu! title plays",
            )
        })?;
    for (index, event) in pending.title_events.iter().enumerate() {
        let Some(info) = parse_osu_title_play(&event.title) else {
            continue;
        };
        if plays.len() >= MAX_CLIP_SIDECAR_PLAYS {
            return Err(OsuEnrichmentError::new(
                OsuEnrichmentErrorKind::TooLarge,
                "retain osu! title plays",
            ));
        }
        let next_unix = pending
            .title_events
            .iter()
            .skip(index + 1)
            .map(|next| next.unix_s)
            .find(|next| *next > event.unix_s)
            .unwrap_or(pending.recording_end_unix);
        if next_unix <= pending.recording_start_unix || event.unix_s >= pending.recording_end_unix {
            continue;
        }
        let start_unix = event.unix_s.max(pending.recording_start_unix);
        let end_unix = next_unix.min(pending.recording_end_unix).max(start_unix);
        let clip_start_s = clamp_clip_time(
            start_unix as f64 - pending.recording_start_unix as f64,
            pending,
        );
        let clip_end_s = clamp_clip_time(
            end_unix as f64 - pending.recording_start_unix as f64,
            pending,
        )
        .max(clip_start_s);
        if clip_end_s <= clip_start_s {
            continue;
        }
        plays.push(ClipPlay {
            game_id: GameId::Osu,
            source: "osu_title".into(),
            external_id: format!("osu-title:{}", event.unix_s),
            url: None,
            beatmap_id: None,
            beatmapset_id: None,
            cover_url: None,
            title: info.title,
            artist: info.artist,
            difficulty: info.difficulty,
            mapper: None,
            star_rating: None,
            mods: Vec::new(),
            rank: None,
            passed: true,
            accuracy: None,
            max_combo: None,
            total_score: None,
            pp: None,
            started_at: Some(unix_to_rfc3339(start_unix)?),
            ended_at: unix_to_rfc3339(end_unix)?,
            derived_start: true,
            t_start_s: clip_start_s,
            t_end_s: Some(clip_end_s),
        });
    }
    Ok(plays)
}

fn write_plays_sidecar(
    pending: &DiscoveredPendingEnrichment,
    plays: Vec<ClipPlay>,
) -> Result<Vec<ClipPlay>, OsuEnrichmentError> {
    if plays.len() > MAX_CLIP_SIDECAR_PLAYS {
        return Err(OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::TooLarge,
            "validate osu! marker plays",
        ));
    }
    ensure_pending_clip(pending)?;
    ensure_pending_sidecar(pending)?;
    let marker_path = clip_sidecar_paths(&pending.clip_path).markers;
    let marker_name = marker_path.file_name().ok_or_else(|| {
        OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::UnsafePath,
            "validate osu! marker sidecar name",
        )
    })?;
    let existing = read_bounded_regular_under(
        &pending.parent_authority,
        marker_name,
        MAX_CLIP_DETAIL_SIDECAR_BYTES,
    )?;
    let (mut markers, marker_identity) = match existing {
        Some((bytes, identity)) => (
            parse_marker_sidecar_preserving_all(&bytes).map_err(|_| {
                OsuEnrichmentError::new(
                    OsuEnrichmentErrorKind::Malformed,
                    "parse existing marker sidecar",
                )
            })?,
            Some(identity),
        ),
        None => (
            ClipMarkers {
                recording_start_s: 0.0,
                duration_s: pending.record.clip_duration_s,
                player_summary: None,
                audio_tracks: Vec::new(),
                plays: Vec::new(),
                markers: Vec::new(),
            },
            None,
        ),
    };
    if markers.duration_s <= 0.0 || !markers.duration_s.is_finite() {
        markers.duration_s = pending.record.clip_duration_s;
    }
    markers.plays = plays;
    let bytes = serialize_json(&markers, MAX_CLIP_DETAIL_SIDECAR_BYTES)?;
    parse_marker_sidecar_preserving_all(&bytes).map_err(|_| {
        OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::Malformed,
            "validate replacement marker sidecar",
        )
    })?;
    let plays = std::mem::take(&mut markers.plays);
    ensure_pending_clip(pending)?;
    ensure_pending_sidecar(pending)?;
    publish_bytes(
        &pending.parent_authority,
        marker_name,
        marker_identity,
        &bytes,
    )?;
    Ok(plays)
}

fn remove_pending(pending: &DiscoveredPendingEnrichment) -> Result<(), OsuEnrichmentError> {
    ensure_pending_clip(pending)?;
    pending
        .parent_authority
        .remove_file_if_identity(&pending.sidecar_name, pending.sidecar_identity)
        .map_err(|_| {
            OsuEnrichmentError::new(
                OsuEnrichmentErrorKind::StaleFile,
                "remove exact pending osu! sidecar",
            )
        })
}

fn ensure_pending_clip(pending: &DiscoveredPendingEnrichment) -> Result<(), OsuEnrichmentError> {
    if pending
        .parent_authority
        .regular_file_identity(&pending.clip_name)
        .ok()
        == Some(Some(pending.clip_identity))
    {
        Ok(())
    } else {
        Err(OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::StaleFile,
            "fence discovered osu! clip",
        ))
    }
}

fn ensure_pending_sidecar(pending: &DiscoveredPendingEnrichment) -> Result<(), OsuEnrichmentError> {
    if pending
        .parent_authority
        .regular_file_identity(&pending.sidecar_name)
        .ok()
        == Some(Some(pending.sidecar_identity))
    {
        Ok(())
    } else {
        Err(OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::StaleFile,
            "fence discovered osu! sidecar",
        ))
    }
}

fn discover_pending_in_dir(
    media_root: &Path,
    dir: &Path,
    out: &mut Vec<DiscoveredPendingEnrichment>,
    invalid: &mut Vec<InvalidPendingEnrichment>,
    scanned: &mut usize,
) -> Result<(), OsuEnrichmentError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => {
            return Err(OsuEnrichmentError::new(
                OsuEnrichmentErrorKind::Io,
                "read pending osu! enrichment directory",
            ))
        }
    };
    for entry in entries {
        let entry = entry.map_err(|_| {
            OsuEnrichmentError::new(
                OsuEnrichmentErrorKind::Io,
                "read pending osu! enrichment entry",
            )
        })?;
        bump_scanned(scanned)?;
        let path = entry.path();
        let Some(stem) = path
            .file_name()
            .and_then(OsStr::to_str)
            .and_then(|name| name.strip_suffix(".osu-enrichment.json"))
            .filter(|stem| !stem.is_empty())
        else {
            continue;
        };
        if out.len().saturating_add(invalid.len()) >= MAX_OSU_PENDING_JOBS {
            return Err(OsuEnrichmentError::new(
                OsuEnrichmentErrorKind::TooLarge,
                "retain pending osu! enrichment jobs",
            ));
        }
        // Capture the candidate identity before parsing. If the directory
        // entry changes anywhere during validation, quarantine can only move
        // this exact pre-parse file and therefore preserves the replacement.
        let invalid_candidate = discover_invalid_pending(&path);
        match discover_pending_file(media_root, &path, stem) {
            Ok(job) => out.push(job),
            Err(error) => match invalid_candidate {
                Ok(candidate) => invalid.push(candidate),
                Err(quarantine_error) => tracing::warn!(
                    event = "invalid_osu_enrichment_skipped",
                    kind = ?error.kind(),
                    quarantine_kind = ?quarantine_error.kind()
                ),
            },
        }
    }
    Ok(())
}

fn discover_invalid_pending(path: &Path) -> Result<InvalidPendingEnrichment, OsuEnrichmentError> {
    let parent = path.parent().ok_or_else(|| {
        OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::UnsafePath,
            "validate invalid osu! sidecar parent",
        )
    })?;
    let sidecar_name = path.file_name().ok_or_else(|| {
        OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::UnsafePath,
            "validate invalid osu! sidecar name",
        )
    })?;
    let parent_authority = DirectoryAuthority::open(parent).map_err(|_| {
        OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::UnsafePath,
            "retain invalid osu! sidecar parent",
        )
    })?;
    let sidecar = parent_authority
        .open_regular_file(sidecar_name)
        .map_err(|_| {
            OsuEnrichmentError::new(
                OsuEnrichmentErrorKind::UnsafePath,
                "open invalid osu! enrichment sidecar",
            )
        })?;
    let sidecar_identity = opened_file_identity(&sidecar).map_err(|_| {
        OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::UnsafePath,
            "identify invalid osu! enrichment sidecar",
        )
    })?;
    drop(sidecar);
    Ok(InvalidPendingEnrichment {
        parent_authority,
        sidecar_name: sidecar_name.to_os_string(),
        sidecar_identity,
    })
}

fn discover_pending_file(
    media_root: &Path,
    path: &Path,
    stem: &str,
) -> Result<DiscoveredPendingEnrichment, OsuEnrichmentError> {
    let parent = path.parent().ok_or_else(|| {
        OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::UnsafePath,
            "validate discovered osu! sidecar parent",
        )
    })?;
    let sidecar_name = path.file_name().ok_or_else(|| {
        OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::UnsafePath,
            "validate discovered osu! sidecar name",
        )
    })?;
    let clip_name = OsString::from(format!("{stem}.mp4"));
    let parent_authority = DirectoryAuthority::open(parent).map_err(|_| {
        OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::UnsafePath,
            "retain discovered osu! clip parent",
        )
    })?;
    let (bytes, sidecar_identity) =
        read_bounded_regular_under(&parent_authority, sidecar_name, MAX_OSU_PENDING_BYTES)?
            .ok_or_else(|| {
                OsuEnrichmentError::new(
                    OsuEnrichmentErrorKind::UnsafePath,
                    "open pending osu! enrichment sidecar",
                )
            })?;
    let sidecar_path = path.canonicalize().map_err(|_| {
        OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::UnsafePath,
            "canonicalize pending osu! enrichment sidecar",
        )
    })?;
    if !sidecar_path.starts_with(media_root) {
        return Err(OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::UnsafePath,
            "contain pending osu! enrichment sidecar",
        ));
    }
    let clip_candidate = path.with_file_name(&clip_name);
    let clip_file = parent_authority
        .open_regular_file(&clip_name)
        .map_err(|_| {
            OsuEnrichmentError::new(
                OsuEnrichmentErrorKind::UnsafePath,
                "open expected osu! enrichment clip",
            )
        })?;
    let clip_identity = opened_file_identity(&clip_file).map_err(|_| {
        OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::UnsafePath,
            "identify expected osu! enrichment clip",
        )
    })?;
    let clip_path = clip_candidate.canonicalize().map_err(|_| {
        OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::UnsafePath,
            "canonicalize expected osu! enrichment clip",
        )
    })?;
    let parent_ok = clip_path.parent() == Some(media_root)
        || clip_path.parent().and_then(Path::parent) == Some(media_root);
    if !parent_ok
        || !clip_path.starts_with(media_root)
        || clip_path.extension().and_then(OsStr::to_str) != Some("mp4")
    {
        return Err(OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::UnsafePath,
            "contain expected osu! enrichment clip",
        ));
    }
    let record: OsuPendingEnrichment = serde_json::from_slice(&bytes).map_err(|_| {
        OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::Malformed,
            "parse pending osu! enrichment sidecar",
        )
    })?;
    validate_pending(&record)?;
    let serialized_clip = Path::new(&record.clip_path).canonicalize().map_err(|_| {
        OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::UnsafePath,
            "canonicalize serialized osu! enrichment clip",
        )
    })?;
    if serialized_clip != clip_path {
        return Err(OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::StaleFile,
            "match serialized osu! enrichment clip",
        ));
    }
    ensure_authority_identity(&parent_authority, sidecar_name, sidecar_identity)?;
    ensure_authority_identity(&parent_authority, &clip_name, clip_identity)?;
    if sidecar_path.parent() != Some(parent) {
        return Err(OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::UnsafePath,
            "match discovered osu! sidecar parent",
        ));
    }
    Ok(DiscoveredPendingEnrichment {
        record,
        clip_path,
        clip_identity,
        sidecar_path,
        sidecar_identity,
        parent_authority,
        clip_name,
        sidecar_name: sidecar_name.to_os_string(),
    })
}

fn validate_pending(record: &OsuPendingEnrichment) -> Result<(), OsuEnrichmentError> {
    if record.schema_version != OSU_PENDING_SCHEMA_VERSION
        || record.recording_end_unix < record.recording_start_unix
        || !record.clip_duration_s.is_finite()
        || !(0.0..=MAX_CLIP_DURATION_S).contains(&record.clip_duration_s)
    {
        return Err(OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::InvalidInput,
            "validate pending osu! enrichment record",
        ));
    }
    if record.clip_path.is_empty() || record.clip_path.len() > MAX_OSU_PENDING_PATH_BYTES {
        return Err(OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::TooLarge,
            "validate pending osu! enrichment path",
        ));
    }
    validate_title_events(&record.title_events)?;
    if record
        .message
        .as_ref()
        .is_some_and(|message| message.len() > MAX_OSU_PENDING_MESSAGE_BYTES)
    {
        return Err(OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::TooLarge,
            "validate pending osu! enrichment text",
        ));
    }
    Ok(())
}

fn validate_title_events(events: &[OsuTitleEvent]) -> Result<(), OsuEnrichmentError> {
    if events.len() > MAX_OSU_TITLE_EVENTS {
        return Err(OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::TooLarge,
            "validate pending osu! title events",
        ));
    }
    let title_bytes = events.iter().try_fold(0usize, |total, event| {
        if event.title.len() > MAX_OSU_TITLE_BYTES {
            return Err(OsuEnrichmentError::new(
                OsuEnrichmentErrorKind::TooLarge,
                "validate pending osu! title",
            ));
        }
        total.checked_add(event.title.len()).ok_or_else(|| {
            OsuEnrichmentError::new(
                OsuEnrichmentErrorKind::TooLarge,
                "sum pending osu! title bytes",
            )
        })
    })?;
    if title_bytes > MAX_OSU_TITLE_TOTAL_BYTES {
        return Err(OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::TooLarge,
            "validate pending osu! title text",
        ));
    }
    Ok(())
}

fn read_bounded_regular(
    path: &Path,
    maximum: usize,
) -> Result<Option<(Vec<u8>, FileIdentity)>, OsuEnrichmentError> {
    let file = match open_regular_file_nofollow(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(OsuEnrichmentError::new(
                OsuEnrichmentErrorKind::UnsafePath,
                "open bounded osu! sidecar",
            ))
        }
    };
    let identity = opened_file_identity(&file).map_err(|_| {
        OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::UnsafePath,
            "identify bounded osu! sidecar",
        )
    })?;
    let declared = file.metadata().map_err(|_| {
        OsuEnrichmentError::new(OsuEnrichmentErrorKind::Io, "inspect bounded osu! sidecar")
    })?;
    if declared.len() > maximum as u64 {
        return Err(OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::TooLarge,
            "validate bounded osu! sidecar length",
        ));
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(usize::try_from(declared.len()).unwrap_or(maximum))
        .map_err(|_| {
            OsuEnrichmentError::new(
                OsuEnrichmentErrorKind::Allocation,
                "reserve bounded osu! sidecar",
            )
        })?;
    let mut file = file;
    Read::by_ref(&mut file)
        .take(maximum as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            OsuEnrichmentError::new(OsuEnrichmentErrorKind::Io, "read bounded osu! sidecar")
        })?;
    if bytes.len() > maximum {
        return Err(OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::TooLarge,
            "validate bounded osu! sidecar bytes",
        ));
    }
    ensure_file_identity(path, identity)?;
    Ok(Some((bytes, identity)))
}

fn read_bounded_regular_under(
    authority: &DirectoryAuthority,
    name: &OsStr,
    maximum: usize,
) -> Result<Option<(Vec<u8>, FileIdentity)>, OsuEnrichmentError> {
    let file = match authority.open_regular_file(name) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(OsuEnrichmentError::new(
                OsuEnrichmentErrorKind::UnsafePath,
                "open retained bounded osu! sidecar",
            ))
        }
    };
    let identity = opened_file_identity(&file).map_err(|_| {
        OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::UnsafePath,
            "identify retained bounded osu! sidecar",
        )
    })?;
    let declared = file.metadata().map_err(|_| {
        OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::Io,
            "inspect retained bounded osu! sidecar",
        )
    })?;
    if declared.len() > maximum as u64 {
        return Err(OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::TooLarge,
            "validate retained bounded osu! sidecar length",
        ));
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(usize::try_from(declared.len()).unwrap_or(maximum))
        .map_err(|_| {
            OsuEnrichmentError::new(
                OsuEnrichmentErrorKind::Allocation,
                "reserve retained bounded osu! sidecar",
            )
        })?;
    let mut file = file;
    Read::by_ref(&mut file)
        .take(maximum as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            OsuEnrichmentError::new(
                OsuEnrichmentErrorKind::Io,
                "read retained bounded osu! sidecar",
            )
        })?;
    if bytes.len() > maximum {
        return Err(OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::TooLarge,
            "validate retained bounded osu! sidecar bytes",
        ));
    }
    ensure_authority_identity(authority, name, identity)?;
    Ok(Some((bytes, identity)))
}

fn existing_regular_identity(path: &Path) -> Result<Option<FileIdentity>, OsuEnrichmentError> {
    let parent = path.parent().ok_or_else(|| {
        OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::UnsafePath,
            "validate osu! sidecar parent",
        )
    })?;
    let name = path.file_name().ok_or_else(|| {
        OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::UnsafePath,
            "validate osu! sidecar name",
        )
    })?;
    DirectoryAuthority::open(parent)
        .map_err(|_| {
            OsuEnrichmentError::new(
                OsuEnrichmentErrorKind::UnsafePath,
                "open osu! sidecar parent",
            )
        })?
        .regular_file_identity(name)
        .map_err(|_| {
            OsuEnrichmentError::new(OsuEnrichmentErrorKind::UnsafePath, "identify osu! sidecar")
        })
}

fn ensure_file_identity(path: &Path, expected: FileIdentity) -> Result<(), OsuEnrichmentError> {
    if existing_regular_identity(path)? == Some(expected) {
        Ok(())
    } else {
        Err(OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::StaleFile,
            "fence exact osu! file",
        ))
    }
}

fn regular_identity(
    authority: &DirectoryAuthority,
    name: &OsStr,
) -> Result<Option<FileIdentity>, OsuEnrichmentError> {
    authority.regular_file_identity(name).map_err(|_| {
        OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::UnsafePath,
            "identify retained osu! sidecar child",
        )
    })
}

fn ensure_authority_identity(
    authority: &DirectoryAuthority,
    name: &OsStr,
    expected: FileIdentity,
) -> Result<(), OsuEnrichmentError> {
    if regular_identity(authority, name)? == Some(expected) {
        Ok(())
    } else {
        Err(OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::StaleFile,
            "fence retained osu! file child",
        ))
    }
}

struct OwnedSidecarTemp<'a> {
    authority: &'a DirectoryAuthority,
    name: OsString,
    file: Option<std::fs::File>,
    identity: FileIdentity,
    armed: bool,
}

impl<'a> OwnedSidecarTemp<'a> {
    fn create(
        authority: &'a DirectoryAuthority,
        target_name: &OsStr,
    ) -> Result<Self, OsuEnrichmentError> {
        for _ in 0..64 {
            let counter = OSU_SIDECAR_COUNTER.fetch_add(1, Ordering::Relaxed);
            let mut name = OsString::from(target_name);
            name.push(format!(
                ".clipline-osu-tmp.{}.{counter}",
                std::process::id()
            ));
            match authority.create_new_regular_file(&name) {
                Ok(file) => {
                    let identity = opened_file_identity(&file).map_err(|_| {
                        OsuEnrichmentError::new(
                            OsuEnrichmentErrorKind::UnsafePath,
                            "identify temporary osu! sidecar",
                        )
                    })?;
                    return Ok(Self {
                        authority,
                        name,
                        file: Some(file),
                        identity,
                        armed: true,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => {
                    return Err(OsuEnrichmentError::new(
                        OsuEnrichmentErrorKind::Io,
                        "create temporary osu! sidecar",
                    ))
                }
            }
        }
        Err(OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::TooLarge,
            "allocate temporary osu! sidecar name",
        ))
    }

    fn write_and_publish(
        mut self,
        target: &OsStr,
        expected_target: Option<FileIdentity>,
        bytes: &[u8],
    ) -> Result<(), OsuEnrichmentError> {
        let file = self.file.as_mut().expect("new osu! temp owns its file");
        file.write_all(bytes).map_err(|_| {
            OsuEnrichmentError::new(OsuEnrichmentErrorKind::Io, "write temporary osu! sidecar")
        })?;
        file.sync_all().map_err(|_| {
            OsuEnrichmentError::new(OsuEnrichmentErrorKind::Io, "sync temporary osu! sidecar")
        })?;
        self.file.take();
        match expected_target {
            Some(target_identity) => self
                .authority
                .replace_file_if_identities(&self.name, self.identity, target, target_identity)
                .map_err(|_| {
                    OsuEnrichmentError::new(
                        OsuEnrichmentErrorKind::StaleFile,
                        "replace exact osu! sidecar",
                    )
                })?,
            None => self
                .authority
                .rename_file_noreplace_if_identity(&self.name, target, self.identity)
                .map_err(|_| {
                    OsuEnrichmentError::new(
                        OsuEnrichmentErrorKind::StaleFile,
                        "publish new osu! sidecar",
                    )
                })?,
        }
        self.armed = false;
        Ok(())
    }
}

impl Drop for OwnedSidecarTemp<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.file.take();
            let _ = self
                .authority
                .remove_file_if_identity(&self.name, self.identity);
        }
    }
}

fn serialize_json<T: serde::Serialize>(
    value: &T,
    maximum: usize,
) -> Result<Vec<u8>, OsuEnrichmentError> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|_| {
        OsuEnrichmentError::new(OsuEnrichmentErrorKind::Malformed, "serialize osu! sidecar")
    })?;
    if bytes.len() > maximum {
        return Err(OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::TooLarge,
            "validate serialized osu! sidecar",
        ));
    }
    Ok(bytes)
}

fn publish_json<T: serde::Serialize>(
    authority: &DirectoryAuthority,
    target_name: &OsStr,
    expected_target: Option<FileIdentity>,
    value: &T,
    maximum: usize,
) -> Result<(), OsuEnrichmentError> {
    let bytes = serialize_json(value, maximum)?;
    publish_bytes(authority, target_name, expected_target, &bytes)
}

fn publish_bytes(
    authority: &DirectoryAuthority,
    target_name: &OsStr,
    expected_target: Option<FileIdentity>,
    bytes: &[u8],
) -> Result<(), OsuEnrichmentError> {
    OwnedSidecarTemp::create(authority, target_name)?.write_and_publish(
        target_name,
        expected_target,
        bytes,
    )
}

fn bump_scanned(scanned: &mut usize) -> Result<(), OsuEnrichmentError> {
    *scanned = scanned.checked_add(1).ok_or_else(|| {
        OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::TooLarge,
            "count osu! enrichment scan entries",
        )
    })?;
    if *scanned > MAX_OSU_PENDING_SCAN_ENTRIES {
        return Err(OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::TooLarge,
            "scan pending osu! enrichment entries",
        ));
    }
    Ok(())
}

fn retry_delay(status: OsuEnrichmentStatus, attempts: u32) -> Duration {
    let (base, cap) = match status {
        OsuEnrichmentStatus::Pending if attempts == 0 => return Duration::ZERO,
        OsuEnrichmentStatus::Pending => (PENDING_RETRY_BASE_S, PENDING_RETRY_CAP_S),
        OsuEnrichmentStatus::Failed => (FAILED_RETRY_BASE_S, FAILED_RETRY_CAP_S),
        OsuEnrichmentStatus::Complete => return Duration::MAX,
    };
    let shift = attempts.saturating_sub(1).min(31);
    Duration::from_secs(base.saturating_mul(1_u64 << shift).min(cap))
}

fn retry_is_due(
    status: OsuEnrichmentStatus,
    attempts: u32,
    modified_unix: u64,
    now_unix: u64,
) -> bool {
    let delay = retry_delay(status, attempts);
    delay != Duration::MAX && now_unix >= modified_unix.saturating_add(delay.as_secs())
}

fn checkpoint(fence: &dyn OsuRequestFence, owner: OsuHttpOwner) -> Result<(), OsuEnrichmentError> {
    if fence.is_current(owner) {
        Ok(())
    } else {
        Err(OsuEnrichmentError::new(
            OsuEnrichmentErrorKind::AccountChanged,
            "fence osu! enrichment account",
        ))
    }
}

fn clip_session_is_osu(path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    session_game_id(parent).as_deref() == Some(OSU_ID)
}

fn session_game_id(session_dir: &Path) -> Option<String> {
    let path = session_dir.join(SESSION_META_FILE);
    let (bytes, _) = read_bounded_regular(&path, MAX_SESSION_META_BYTES).ok()??;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value.get("id")?.as_str().map(str::to_owned)
}

struct TitlePlayInfo {
    artist: String,
    title: String,
    difficulty: String,
}

fn parse_osu_title_play(title: &str) -> Option<TitlePlayInfo> {
    let raw = title.trim();
    if !raw.to_ascii_lowercase().starts_with("osu!") {
        return None;
    }
    let rest = raw.get(4..)?.trim_start().strip_prefix('-')?.trim();
    if rest.is_empty() {
        return None;
    }
    let (song, difficulty) = if rest.ends_with(']') {
        rest.rfind('[').map_or((rest, ""), |open| {
            (
                rest[..open].trim_end(),
                rest[open + 1..rest.len().saturating_sub(1)].trim(),
            )
        })
    } else {
        (rest, "")
    };
    let (artist, title) = song
        .split_once(" - ")
        .map(|(artist, title)| (artist.trim(), title.trim()))
        .unwrap_or(("", song.trim()));
    Some(TitlePlayInfo {
        artist: artist.to_owned(),
        title: if title.is_empty() {
            rest.to_owned()
        } else {
            title.to_owned()
        },
        difficulty: difficulty.to_owned(),
    })
}

fn score_start_unix(
    score: &OsuProxyScore,
    pending: &OsuPendingEnrichment,
) -> Option<(i64, bool, bool)> {
    if let Some(started_at) = score.started_at_unix {
        return Some((started_at, false, false));
    }
    if let Some(title_start) = matching_title_event_start_unix(score, pending) {
        return Some((title_start, true, false));
    }
    if !score.passed {
        return Some((score.ended_at_unix, true, true));
    }
    let Some(length_s) = adjusted_total_length_s(score) else {
        return Some((score.ended_at_unix, true, true));
    };
    Some((
        score
            .ended_at_unix
            .saturating_sub(length_s.max(0.0).round() as i64),
        true,
        false,
    ))
}

fn matching_title_event_start_unix(
    score: &OsuProxyScore,
    pending: &OsuPendingEnrichment,
) -> Option<i64> {
    let lookback_s = adjusted_total_length_s(score)
        .map(|length_s| length_s.max(0.0).ceil() as i64 + TITLE_EVENT_LENGTH_SLACK_S)
        .unwrap_or(TITLE_EVENT_FALLBACK_LOOKBACK_S);
    let earliest = score.ended_at_unix.saturating_sub(lookback_s);
    let latest = score.ended_at_unix + UTC_SKEW_TOLERANCE_S.ceil() as i64;
    pending
        .title_events
        .iter()
        .filter(|event| event.unix_s >= earliest && event.unix_s <= latest)
        .filter(|event| title_event_matches_score(&event.title, score))
        .max_by_key(|event| event.unix_s)
        .map(|event| event.unix_s)
}

fn title_event_matches_score(title: &str, score: &OsuProxyScore) -> bool {
    let haystack = normalized_title_match_text(title);
    contains_normalized(&haystack, &score.title)
}

fn contains_normalized(haystack: &str, needle: &str) -> bool {
    let needle = normalized_title_match_text(needle);
    !needle.is_empty() && haystack.contains(&needle)
}

fn normalized_title_match_text(value: &str) -> String {
    let mut out = String::new();
    let mut last_was_space = true;
    for ch in value.chars().flat_map(char::to_lowercase) {
        if ch.is_alphanumeric() {
            out.push(ch);
            last_was_space = false;
        } else if !last_was_space {
            out.push(' ');
            last_was_space = true;
        }
    }
    out.trim().to_owned()
}

fn adjusted_total_length_s(score: &OsuProxyScore) -> Option<f64> {
    let mut length = score.beatmap_total_length_s?;
    if !length.is_finite() || length < 0.0 {
        return None;
    }
    if score
        .mods
        .iter()
        .any(|name| name.eq_ignore_ascii_case("DT") || name.eq_ignore_ascii_case("NC"))
    {
        length /= 1.5;
    } else if score
        .mods
        .iter()
        .any(|name| name.eq_ignore_ascii_case("HT") || name.eq_ignore_ascii_case("DC"))
    {
        length /= 0.75;
    }
    Some(length)
}

fn clamp_clip_time(value: f64, pending: &OsuPendingEnrichment) -> f64 {
    if !pending.clip_duration_s.is_finite() || pending.clip_duration_s <= 0.0 {
        return value.max(0.0);
    }
    value.max(0.0).min(pending.clip_duration_s)
}

fn unix_to_rfc3339(value: i64) -> Result<String, OsuEnrichmentError> {
    DateTime::<Utc>::from_timestamp(value, 0)
        .map(|timestamp| timestamp.to_rfc3339())
        .ok_or_else(|| {
            OsuEnrichmentError::new(
                OsuEnrichmentErrorKind::InvalidInput,
                "convert osu! score timestamp",
            )
        })
}

fn unix_now_i64() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod quarantine_tests {
    use super::*;
    use clipline_test_utils::TestDir;

    #[test]
    fn invalid_pending_quarantine_preserves_a_foreign_replacement() {
        let dir = TestDir::new("clipline-osu", "quarantine-foreign-replacement");
        let path = dir.path().join("clip.osu-enrichment.json");
        std::fs::write(&path, b"{ invalid").unwrap();
        let candidate = discover_invalid_pending(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"foreign").unwrap();

        let error = candidate.quarantine().unwrap_err();

        assert_eq!(error.kind(), OsuEnrichmentErrorKind::StaleFile);
        assert_eq!(std::fs::read(&path).unwrap(), b"foreign");
        assert!(!std::fs::read_dir(dir.path()).unwrap().any(|entry| {
            entry
                .ok()
                .and_then(|entry| entry.file_name().to_str().map(str::to_owned))
                .is_some_and(|name| name.contains(".invalid."))
        }));
    }
}
