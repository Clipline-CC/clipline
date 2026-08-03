//! Slint-facing adapter for the framework-neutral catalog reducer.
//!
//! Slint callbacks carry a catalog revision and bounded indices. The adapter
//! resolves those indices against the exact immutable projection accepted by
//! the reducer, so UI code never constructs paths or cloud identities.

use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};
use std::thread::JoinHandle;

use clipline_library::{
    load_clip_detail, ActiveFileRegistry, CatalogAction, CatalogController, CatalogControllerError,
    CatalogDialogKind, CatalogDialogTextField, CatalogEffect, CatalogItemIdentity,
    CatalogLoadState, CatalogOperationOwner, CatalogProjection, CatalogResult, CatalogResultSender,
    CatalogSource, CatalogUploadVisibility, CatalogWarning, CompactClipProjection,
    ExpectedResultOwner, ForegroundGeneration, KnownGameIdentityResolver, LegacyAudioTrackProbe,
    LocalClipFilter, LocalClipGrouping, LocalClipSort, LocalDay, LocalDayResolver,
    LocalIndexCompletion, LocalLibraryRepository, LocalLibraryScanner, Mp4LegacyAudioTrackProbe,
    PlatformEffect, PlaybackSourceLease, PosterPageItem, PresentationPoster, ResolvedLocalClip,
    StandardRepositoryFileSystem, ValidatedClipPath, WindowAttachmentGeneration, WindowWorkToken,
    MAX_CATALOG_STRING_BYTES, MAX_FOREGROUND_MESSAGE_BYTES,
};
use thiserror::Error;

use crate::{
    CatalogAudioTrack, CatalogDialogKind as SlintDialogKind, CatalogDialogModel, CatalogFilter,
    CatalogGroup, CatalogSort, CatalogSource as SlintCatalogSource,
    CatalogUploadVisibility as SlintUploadVisibility, CliplineSpike, LibraryItem,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogUiIntent {
    Refresh,
    SetSource(CatalogSource),
    SetQuery(String),
    SetLocalFilter(LocalClipFilter),
    SetLocalSort(LocalClipSort),
    SetLocalGrouping(LocalClipGrouping),
    PreviousPage,
    NextPage,
    EnterSelection,
    ExitSelection,
    ToggleSelection {
        row: usize,
    },
    SelectVisiblePage,
    ClearSelection,
    OpenRow {
        row: usize,
    },
    CloseActive,
    OpenContext {
        row: usize,
    },
    CloseContext,
    RenameTitleFromMenu,
    RenameFileFromMenu,
    DeleteFromMenu,
    DeleteSelection,
    UploadFromMenu,
    CancelUploadFromMenu,
    RevealFromMenu,
    OpenInBrowserFromMenu,
    CopyPublicLinkFromMenu,
    SetDialogText {
        field: CatalogDialogTextField,
        value: String,
    },
    SetUploadVisibility(CatalogUploadVisibility),
    SetUploadAudioTrack {
        row: usize,
        selected: bool,
    },
    ConfirmDialog,
    CancelDialog,
    Escape,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CatalogUiError {
    #[error("catalog callback revision {actual} is stale; current revision is {current}")]
    StaleRevision { actual: u64, current: u64 },
    #[error("catalog row index {index} is outside the active page of {len} rows")]
    RowOutOfBounds { index: usize, len: usize },
    #[error("catalog menu action has no current menu target")]
    NoMenuTarget,
    #[error("catalog menu target has no cancelable upload")]
    NoCancelableUpload,
    #[error("catalog audio-track index {index} is outside the active dialog's {len} tracks")]
    AudioTrackOutOfBounds { index: usize, len: usize },
    #[error("catalog controller rejected the action: {0}")]
    Controller(#[from] CatalogControllerError),
}

pub const CATALOG_EFFECT_QUEUE_CAPACITY: usize = 128;
pub const CATALOG_EFFECT_WORKERS: usize = 2;

pub struct OwnedCatalogResult {
    pub result: CatalogResult,
    pub expected: ExpectedResultOwner,
}

pub trait CatalogEffectHandler: Send + Sync + 'static {
    fn execute(&self, effect: CatalogEffect) -> Result<Option<OwnedCatalogResult>, String>;
}

pub trait CatalogResultWake: Send + Sync + 'static {
    fn wake(&self);
}

/// Fixed-worker, fixed-queue executor. Slint callbacks only call `try_submit`;
/// filesystem/network/process work happens on one of the two owned workers.
pub struct CatalogEffectExecutor {
    sender: Option<mpsc::SyncSender<CatalogEffect>>,
    workers: Vec<JoinHandle<()>>,
    stopping: Arc<AtomicBool>,
}

impl CatalogEffectExecutor {
    pub fn start(
        handler: Arc<dyn CatalogEffectHandler>,
        results: CatalogResultSender,
        wake: Arc<dyn CatalogResultWake>,
    ) -> Result<Self, String> {
        let (sender, receiver) = mpsc::sync_channel(CATALOG_EFFECT_QUEUE_CAPACITY);
        let receiver = Arc::new(Mutex::new(receiver));
        let stopping = Arc::new(AtomicBool::new(false));
        let mut executor = Self {
            sender: Some(sender),
            workers: Vec::new(),
            stopping,
        };
        executor
            .workers
            .try_reserve_exact(CATALOG_EFFECT_WORKERS)
            .map_err(|_| "reserve catalog effect workers".to_owned())?;
        for index in 0..CATALOG_EFFECT_WORKERS {
            let handler = Arc::clone(&handler);
            let results = results.clone();
            let wake = Arc::clone(&wake);
            let receiver = Arc::clone(&receiver);
            let stopping = Arc::clone(&executor.stopping);
            let worker = match std::thread::Builder::new()
                .name(format!("clipline-catalog-{index}"))
                .spawn(move || worker_loop(receiver, handler, results, wake, stopping))
            {
                Ok(worker) => worker,
                Err(error) => {
                    let start_error = format!("start catalog effect worker: {error}");
                    return match executor.shutdown_inner() {
                        Ok(()) => Err(start_error),
                        Err(cleanup) => Err(format!("{start_error}; cleanup failed: {cleanup}")),
                    };
                }
            };
            executor.workers.push(worker);
        }
        Ok(executor)
    }

    /// Non-blocking callback boundary. A rejected effect is returned intact so
    /// the UI-thread controller can accept its exact `OperationFailed` owner.
    /// Review lifecycle effects (`CloseReview`, `CancelCloudReviewMedia`,
    /// `OpenPreparedCloudReview`, and `ReleaseCloudReviewMedia`) have no
    /// operation owner and must be executed through the shell's guaranteed
    /// inline path; callers must never enqueue or discard them when this queue
    /// is full.
    pub fn try_submit(&self, effect: CatalogEffect) -> Result<(), Box<CatalogEffect>> {
        let Some(sender) = self.sender.as_ref() else {
            return Err(Box::new(effect));
        };
        sender.try_send(effect).map_err(|error| match error {
            mpsc::TrySendError::Full(effect) | mpsc::TrySendError::Disconnected(effect) => {
                Box::new(effect)
            }
        })
    }

    pub fn shutdown(mut self) -> Result<(), String> {
        self.shutdown_inner()
    }

    fn shutdown_inner(&mut self) -> Result<(), String> {
        self.stopping.store(true, Ordering::Release);
        self.sender.take();
        let mut first_error = None;
        for worker in self.workers.drain(..) {
            if worker.join().is_err() && first_error.is_none() {
                first_error = Some("catalog effect worker panicked".to_owned());
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

impl Drop for CatalogEffectExecutor {
    fn drop(&mut self) {
        let _ = self.shutdown_inner();
    }
}

fn worker_loop(
    receiver: Arc<Mutex<mpsc::Receiver<CatalogEffect>>>,
    handler: Arc<dyn CatalogEffectHandler>,
    results: CatalogResultSender,
    wake: Arc<dyn CatalogResultWake>,
    stopping: Arc<AtomicBool>,
) {
    loop {
        let effect = match receiver.lock() {
            Ok(receiver) => receiver.recv(),
            Err(_) => return,
        };
        let Ok(effect) = effect else {
            return;
        };
        let fallback_owner = effect.operation_owner().ok().flatten();
        let failure_effect = effect.clone();
        let completion = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handler.execute(effect)
        })) {
            Ok(Ok(completion)) => completion,
            Ok(Err(message)) => rejected_effect_result(&failure_effect, message),
            Err(_) => rejected_effect_result(&failure_effect, "catalog effect handler panicked"),
        };
        if let Some(completion) = completion {
            publish_completion(
                &results,
                completion,
                fallback_owner,
                wake.as_ref(),
                &stopping,
            );
        }
    }
}

fn publish_completion(
    results: &CatalogResultSender,
    mut completion: OwnedCatalogResult,
    fallback_owner: Option<CatalogOperationOwner>,
    wake: &dyn CatalogResultWake,
    stopping: &AtomicBool,
) {
    loop {
        let was_operation_failure =
            matches!(&completion.result, CatalogResult::OperationFailed { .. });
        match results.try_send_recoverable(completion.result, completion.expected) {
            Ok(_) => {
                wake.wake();
                return;
            }
            Err(rejected)
                if matches!(
                    rejected.error,
                    clipline_library::ResultPortError::Full { .. }
                        | clipline_library::ResultPortError::ByteCapacity { .. }
                ) =>
            {
                if stopping.load(Ordering::Acquire) {
                    return;
                }
                completion = OwnedCatalogResult {
                    result: rejected.result,
                    expected: rejected.expected,
                };
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            Err(rejected) => {
                let Some(owner) = fallback_owner.clone() else {
                    return;
                };
                if was_operation_failure {
                    return;
                }
                completion = operation_failure(
                    owner,
                    format!(
                        "catalog effect produced an invalid completion: {}",
                        rejected.error
                    ),
                );
            }
        }
    }
}

/// Converts an executor admission failure into the exact reducer-owned
/// terminal result. Effects without an operation owner are lifecycle or
/// foreground actions and must be handled by their dedicated inline path.
#[must_use]
pub fn rejected_effect_result(
    effect: &CatalogEffect,
    message: impl Into<String>,
) -> Option<OwnedCatalogResult> {
    let message = bounded_utf8(message.into(), MAX_FOREGROUND_MESSAGE_BYTES);
    if let CatalogEffect::LoadCloudThumbnail { request } = effect {
        let message = bounded_utf8(message, MAX_CATALOG_STRING_BYTES);
        let owner = request.owner.clone();
        return Some(OwnedCatalogResult {
            result: CatalogResult::CloudThumbnail {
                owner: owner.clone(),
                status: clipline_library::PosterStatus::Failed { message },
            },
            expected: ExpectedResultOwner::CloudThumbnail(owner),
        });
    }
    effect
        .operation_owner()
        .ok()
        .flatten()
        .map(|owner| operation_failure(owner, message))
}

fn operation_failure(owner: CatalogOperationOwner, message: String) -> OwnedCatalogResult {
    let message = bounded_utf8(message, MAX_FOREGROUND_MESSAGE_BYTES);
    OwnedCatalogResult {
        result: CatalogResult::OperationFailed {
            owner: owner.clone(),
            message,
        },
        expected: ExpectedResultOwner::Operation(owner),
    }
}

fn bounded_utf8(mut value: String, maximum: usize) -> String {
    if value.len() <= maximum {
        return value;
    }
    let mut end = maximum;
    while end != 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
}

/// Platform boundary for a validated Library reveal request.
///
/// Keeping this behind a port prevents tests and non-Windows builds from
/// launching a shell while the repository remains the sole path authority.
pub trait CatalogRevealPort: Send + Sync + 'static {
    fn reveal(&self, target: &Path) -> Result<(), String>;
}

/// Window-owned handoff for a validated local review source. The worker owns
/// the filesystem validation and playback lease before invoking this port;
/// dropping either argument is the fail-closed release path.
pub trait CatalogReviewPort: Send + Sync + 'static {
    fn open(
        &self,
        token: WindowWorkToken,
        source: ValidatedClipPath,
        lease: PlaybackSourceLease,
    ) -> Result<(), String>;
}

#[derive(Debug, Clone, Copy, Default)]
struct UnavailableCatalogReview;

impl CatalogReviewPort for UnavailableCatalogReview {
    fn open(
        &self,
        _token: WindowWorkToken,
        _source: ValidatedClipPath,
        _lease: PlaybackSourceLease,
    ) -> Result<(), String> {
        Err("native review session is not attached".into())
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemCatalogReveal;

impl CatalogRevealPort for SystemCatalogReveal {
    fn reveal(&self, target: &Path) -> Result<(), String> {
        #[cfg(windows)]
        {
            clipline_shell::windows::shell_execute::reveal_in_explorer(
                target,
                "reveal Library clip in Explorer",
            )
            .map_err(|error| error.to_string())
        }
        #[cfg(not(windows))]
        {
            let _ = target;
            Err("revealing a Library clip is supported only on Windows".into())
        }
    }
}

/// Production local-filesystem implementation of the catalog effect seam.
///
/// The scanner, detail probe, repository, and upload/mutation registry are
/// shared by both fixed executor workers. Every filesystem target is
/// revalidated by `LocalLibraryRepository` immediately before mutation or
/// reveal; reducer-supplied display paths are never treated as authority.
pub struct LocalCatalogEffectHandler {
    scanner: LocalLibraryScanner,
    repository: LocalLibraryRepository,
    active_files: ActiveFileRegistry,
    probe: Arc<dyn LegacyAudioTrackProbe + Send + Sync>,
    reveal: Arc<dyn CatalogRevealPort>,
    review: Arc<dyn CatalogReviewPort>,
}

impl LocalCatalogEffectHandler {
    pub fn open(root: impl AsRef<Path>, active_files: ActiveFileRegistry) -> Result<Self, String> {
        Self::open_with_ports(
            root,
            active_files,
            Arc::new(Mp4LegacyAudioTrackProbe),
            Arc::new(SystemCatalogReveal),
        )
    }

    pub fn open_with_ports(
        root: impl AsRef<Path>,
        active_files: ActiveFileRegistry,
        probe: Arc<dyn LegacyAudioTrackProbe + Send + Sync>,
        reveal: Arc<dyn CatalogRevealPort>,
    ) -> Result<Self, String> {
        Self::open_with_review_port(
            root,
            active_files,
            probe,
            reveal,
            Arc::new(UnavailableCatalogReview),
        )
    }

    pub fn open_with_review_port(
        root: impl AsRef<Path>,
        active_files: ActiveFileRegistry,
        probe: Arc<dyn LegacyAudioTrackProbe + Send + Sync>,
        reveal: Arc<dyn CatalogRevealPort>,
        review: Arc<dyn CatalogReviewPort>,
    ) -> Result<Self, String> {
        let scanner = LocalLibraryScanner::open(root.as_ref())?;
        let repository = LocalLibraryRepository::with_seams(
            root,
            Arc::new(StandardRepositoryFileSystem),
            Arc::new(active_files.clone()),
        )
        .map_err(|error| error.to_string())?;
        Ok(Self {
            scanner,
            repository,
            active_files,
            probe,
            reveal,
            review,
        })
    }

    fn refresh_local(
        &self,
        token: clipline_library::WindowWorkToken,
        revision: clipline_library::CatalogRevision,
    ) -> Result<OwnedCatalogResult, String> {
        let games = KnownGameIdentityResolver;
        let projection = CompactClipProjection::new(self.probe.as_ref(), &games);
        let scan = self.scanner.scan(&projection)?;
        let warnings = scan
            .warnings
            .into_iter()
            .map(|message| CatalogWarning {
                code: "local_scan".into(),
                message: bounded_utf8(message, MAX_CATALOG_STRING_BYTES),
                path: None,
            })
            .collect();
        let completion =
            LocalIndexCompletion::new(token, revision, scan.truncated, scan.clips, warnings)
                .map_err(|error| error.to_string())?;
        Ok(OwnedCatalogResult {
            result: CatalogResult::LocalIndex(completion),
            expected: ExpectedResultOwner::Window(token),
        })
    }

    fn validate_target(
        &self,
        target: &ResolvedLocalClip,
    ) -> Result<clipline_library::ValidatedClipPath, String> {
        let validated = self
            .repository
            .validate_clip_path(&target.path)
            .map_err(|error| error.to_string())?;
        if validated.comparison_identity() != &target.identity {
            return Err("catalog target identity changed before execution".into());
        }
        if target
            .expected_file_identity
            .is_some_and(|expected| expected != validated.file_identity())
        {
            return Err("catalog target file was replaced before execution".into());
        }
        Ok(validated)
    }

    fn reveal_local(
        &self,
        token: clipline_library::WindowWorkToken,
        target: &ResolvedLocalClip,
    ) -> Result<Option<OwnedCatalogResult>, String> {
        let validated = self.validate_target(target)?;
        let effect = self
            .repository
            .reveal_effect(&validated)
            .map_err(|error| error.to_string())?;
        let PlatformEffect::RevealClip(path) = effect else {
            return Err("Library returned an invalid reveal effect".into());
        };
        match self.reveal.reveal(&path) {
            Ok(()) => Ok(None),
            Err(message) => Ok(Some(OwnedCatalogResult {
                result: CatalogResult::ForegroundFeedback {
                    token,
                    message: bounded_utf8(message, MAX_FOREGROUND_MESSAGE_BYTES),
                },
                expected: ExpectedResultOwner::Window(token),
            })),
        }
    }
}

impl CatalogEffectHandler for LocalCatalogEffectHandler {
    fn execute(&self, effect: CatalogEffect) -> Result<Option<OwnedCatalogResult>, String> {
        effect
            .validate_bounds()
            .map_err(|error| error.to_string())?;
        match effect {
            CatalogEffect::RefreshLocal { token, revision } => {
                self.refresh_local(token, revision).map(Some)
            }
            CatalogEffect::LoadClipDetail {
                token: _,
                request,
                target,
                title,
                description,
            } => {
                let validated = self.validate_target(&target)?;
                let owner = request.owner().clone();
                let result = load_clip_detail(
                    &request,
                    validated.canonical_path(),
                    &title,
                    &description,
                    self.probe.as_ref(),
                )
                .map_err(|error| error.to_string())?;
                Ok(Some(OwnedCatalogResult {
                    result: CatalogResult::ClipDetail(result),
                    expected: ExpectedResultOwner::Detail(owner),
                }))
            }
            CatalogEffect::OpenLocalReview { token, target } => {
                let validated = self.validate_target(&target)?;
                let lease = self
                    .active_files
                    .acquire_playback(&validated, token)
                    .map_err(|error| error.to_string())?;
                self.review.open(token, validated, lease)?;
                Ok(None)
            }
            CatalogEffect::RenameTitle {
                token,
                target,
                title,
            } => {
                let validated = self.validate_target(&target)?;
                let result = self
                    .repository
                    .rename_title(&validated, &title)
                    .map_err(|error| error.to_string())?;
                Ok(Some(OwnedCatalogResult {
                    result: CatalogResult::RenameCompleted { token, result },
                    expected: ExpectedResultOwner::Window(token),
                }))
            }
            CatalogEffect::RenameFile {
                token,
                target,
                file_name,
            } => {
                let validated = self.validate_target(&target)?;
                let result = self
                    .repository
                    .rename_file(&validated, &file_name)
                    .map_err(|error| error.to_string())?;
                Ok(Some(OwnedCatalogResult {
                    result: CatalogResult::RenameCompleted { token, result },
                    expected: ExpectedResultOwner::Window(token),
                }))
            }
            CatalogEffect::Delete { token, targets } => {
                let mut report = clipline_library::DeletedClipsReport::default();
                for target in targets {
                    let path = target.path.clone();
                    match self.validate_target(&target).and_then(|validated| {
                        self.repository
                            .delete(&validated)
                            .map_err(|error| error.to_string())
                    }) {
                        Ok(()) => report.deleted.push(path),
                        Err(error) => report.failed.push((path, error)),
                    }
                }
                Ok(Some(OwnedCatalogResult {
                    result: CatalogResult::DeleteCompleted { token, report },
                    expected: ExpectedResultOwner::Window(token),
                }))
            }
            CatalogEffect::Reveal { token, target } => self.reveal_local(token, &target),
            other => Err(format!(
                "catalog effect is not handled by the local executor: {other:?}"
            )),
        }
    }
}

/// Long-lived owner of catalog state for the Slint shell.
///
/// The window receives only an `Arc<CatalogProjection>`. The controller and
/// accepted local/cloud metadata survive window destruction in this adapter.
pub struct SlintCatalogController {
    controller: CatalogController,
}

/// Exact local page used by the window-scoped poster owner. The stamp includes
/// the scanner-captured file identity so a same-path replacement invalidates a
/// retained image without making the Slint callback touch the filesystem.
pub struct CatalogPosterPage {
    pub items: Vec<PosterPageItem>,
    pub stamp: Vec<(
        clipline_library::ClipPathIdentity,
        Option<clipline_shell::FileIdentity>,
    )>,
}

impl SlintCatalogController {
    pub fn new(days: Arc<dyn LocalDayResolver>) -> Result<Self, CatalogUiError> {
        Ok(Self {
            controller: CatalogController::new(days)?,
        })
    }

    #[must_use]
    pub fn projection(&self) -> Arc<CatalogProjection> {
        Arc::clone(&self.controller.state().projection)
    }

    pub fn poster_page(&self) -> Result<CatalogPosterPage, CatalogUiError> {
        let state = self.controller.state();
        if state.source != CatalogSource::Local {
            return Ok(CatalogPosterPage {
                items: Vec::new(),
                stamp: Vec::new(),
            });
        }
        let mut items = Vec::new();
        let mut stamp = Vec::new();
        items
            .try_reserve_exact(state.projection.rows.len())
            .map_err(|_| {
                CatalogUiError::Controller(CatalogControllerError::Invalid {
                    field: "slint.poster_page.items",
                })
            })?;
        stamp
            .try_reserve_exact(state.projection.rows.len())
            .map_err(|_| {
                CatalogUiError::Controller(CatalogControllerError::Invalid {
                    field: "slint.poster_page.stamp",
                })
            })?;
        for row in &state.projection.rows {
            let CatalogItemIdentity::Local { path } = &row.identity else {
                return Err(CatalogUiError::Controller(
                    CatalogControllerError::Invalid {
                        field: "slint.poster_page.source",
                    },
                ));
            };
            let local = state
                .local_items
                .iter()
                .find(|item| item.path_identity().as_ref() == Some(path))
                .ok_or(CatalogUiError::Controller(
                    CatalogControllerError::Invalid {
                        field: "slint.poster_page.identity",
                    },
                ))?;
            let duration =
                local.duration_s.or((local.marker_summary.duration_s > 0.0)
                    .then_some(local.marker_summary.duration_s));
            let seek_seconds = duration
                .filter(|duration| duration.is_finite() && *duration > 0.0)
                .map_or(1.0, |duration| (duration * 0.15).min(5.0));
            items.push(
                PosterPageItem::new_with_file_identity(
                    PathBuf::from(&local.path),
                    local.file_identity,
                    seek_seconds,
                )
                .map_err(|_| {
                    CatalogUiError::Controller(CatalogControllerError::Invalid {
                        field: "slint.poster_page.item",
                    })
                })?,
            );
            stamp.push((path.clone(), local.file_identity));
        }
        Ok(CatalogPosterPage { items, stamp })
    }

    /// Exact revision retained in Rust. Slint's `int` is only 32-bit, so the
    /// callback fence must never round-trip through a component property.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.controller.state().projection.revision.get()
    }

    pub fn attach(
        &mut self,
        attachment: WindowAttachmentGeneration,
        foreground: ForegroundGeneration,
    ) -> Result<Vec<CatalogEffect>, CatalogUiError> {
        Ok(self.controller.attach(attachment, foreground)?)
    }

    pub fn detach(&mut self) -> Result<Vec<CatalogEffect>, CatalogUiError> {
        Ok(self.controller.detach()?)
    }

    pub fn set_cloud_context(
        &mut self,
        owner: Option<clipline_library::CloudCatalogOwner>,
        preferences: clipline_library::CatalogCloudPreferences,
    ) -> Result<Vec<CatalogEffect>, CatalogUiError> {
        Ok(self.controller.set_cloud_context(owner, preferences)?)
    }

    pub fn dispatch(
        &mut self,
        revision: u64,
        intent: CatalogUiIntent,
    ) -> Result<Vec<CatalogEffect>, CatalogUiError> {
        let action = route_ui_intent(
            self.controller.state().projection.as_ref(),
            revision,
            intent,
        )?;
        Ok(self.controller.dispatch(action)?)
    }

    pub fn accept(&mut self, result: CatalogResult) -> Result<Vec<CatalogEffect>, CatalogUiError> {
        Ok(self.controller.accept(result)?)
    }
}

pub fn route_ui_intent(
    projection: &CatalogProjection,
    revision: u64,
    intent: CatalogUiIntent,
) -> Result<CatalogAction, CatalogUiError> {
    let current = projection.revision.get();
    if revision != current {
        return Err(CatalogUiError::StaleRevision {
            actual: revision,
            current,
        });
    }

    Ok(match intent {
        CatalogUiIntent::Refresh => CatalogAction::Refresh,
        CatalogUiIntent::SetSource(source) => CatalogAction::SetSource { source },
        CatalogUiIntent::SetQuery(query) => CatalogAction::SetQuery { query },
        CatalogUiIntent::SetLocalFilter(filter) => CatalogAction::SetLocalFilter { filter },
        CatalogUiIntent::SetLocalSort(sort) => CatalogAction::SetLocalSort { sort },
        CatalogUiIntent::SetLocalGrouping(grouping) => CatalogAction::SetLocalGrouping { grouping },
        CatalogUiIntent::PreviousPage => CatalogAction::PreviousPage,
        CatalogUiIntent::NextPage => CatalogAction::NextPage,
        CatalogUiIntent::EnterSelection => CatalogAction::EnterSelection,
        CatalogUiIntent::ExitSelection => CatalogAction::ExitSelection,
        CatalogUiIntent::ToggleSelection { row } => CatalogAction::ToggleSelection {
            item: row_identity(projection, row)?,
        },
        CatalogUiIntent::SelectVisiblePage => CatalogAction::SelectVisiblePage,
        CatalogUiIntent::ClearSelection => CatalogAction::ClearSelection,
        CatalogUiIntent::OpenRow { row } => CatalogAction::OpenItem {
            item: row_identity(projection, row)?,
        },
        CatalogUiIntent::CloseActive => CatalogAction::CloseActive,
        CatalogUiIntent::OpenContext { row } => CatalogAction::OpenContext {
            item: row_identity(projection, row)?,
        },
        CatalogUiIntent::CloseContext => CatalogAction::CloseContext,
        CatalogUiIntent::RenameTitleFromMenu => CatalogAction::OpenRenameTitle {
            item: menu_identity(projection)?,
        },
        CatalogUiIntent::RenameFileFromMenu => CatalogAction::OpenRenameFile {
            item: menu_identity(projection)?,
        },
        CatalogUiIntent::DeleteFromMenu => CatalogAction::OpenDelete {
            item: menu_identity(projection)?,
        },
        CatalogUiIntent::DeleteSelection => CatalogAction::OpenDeleteSelection,
        CatalogUiIntent::UploadFromMenu => CatalogAction::OpenUpload {
            item: menu_identity(projection)?,
        },
        CatalogUiIntent::CancelUploadFromMenu => CatalogAction::OpenCancelUpload {
            token: menu_upload(projection)?.token.clone(),
        },
        CatalogUiIntent::RevealFromMenu => CatalogAction::Reveal {
            item: menu_identity(projection)?,
        },
        CatalogUiIntent::OpenInBrowserFromMenu => CatalogAction::OpenInBrowser {
            item: menu_identity(projection)?,
        },
        CatalogUiIntent::CopyPublicLinkFromMenu => CatalogAction::CopyPublicLink {
            item: menu_identity(projection)?,
        },
        CatalogUiIntent::SetDialogText { field, value } => {
            CatalogAction::SetDialogText { field, value }
        }
        CatalogUiIntent::SetUploadVisibility(visibility) => {
            CatalogAction::SetUploadVisibility { visibility }
        }
        CatalogUiIntent::SetUploadAudioTrack { row, selected } => {
            let tracks = projection
                .dialog
                .as_ref()
                .map_or(&[][..], |dialog| dialog.audio_tracks.as_slice());
            let track = tracks
                .get(row)
                .ok_or(CatalogUiError::AudioTrackOutOfBounds {
                    index: row,
                    len: tracks.len(),
                })?;
            CatalogAction::SetUploadAudioTrack {
                track_id: track.id.clone(),
                selected,
            }
        }
        CatalogUiIntent::ConfirmDialog => CatalogAction::ConfirmDialog,
        CatalogUiIntent::CancelDialog => CatalogAction::CancelDialog,
        CatalogUiIntent::Escape => CatalogAction::Escape,
    })
}

/// Stage and publish one complete bounded controller projection. The optional
/// image callback is window-owned; retained paths and controller data survive
/// window recreation, while decoded Slint handles do not.
pub fn publish_projection(
    window: &CliplineSpike,
    projection: &CatalogProjection,
    mut image_for: impl FnMut(&CatalogItemIdentity) -> Option<slint::Image>,
) -> Result<(), CatalogUiError> {
    let mut rows = Vec::new();
    rows.try_reserve_exact(projection.rows.len()).map_err(|_| {
        CatalogUiError::Controller(CatalogControllerError::Invalid {
            field: "slint.rows.allocation",
        })
    })?;
    for (index, row) in projection.rows.iter().enumerate() {
        let seed = u16::try_from(index % 255).unwrap_or(0);
        rows.push(LibraryItem {
            title: row.title.clone().into(),
            subtitle: row.subtitle.clone().into(),
            duration: row.duration.clone().into(),
            kind: row.kind.to_uppercase().into(),
            kind_color: kind_color(&row.kind),
            poster_image: image_for(&row.identity).unwrap_or_default(),
            poster_a: crate::color(
                (34 + seed * 7 % 72) as u8,
                (23 + seed * 11 % 58) as u8,
                (16 + seed * 13 % 46) as u8,
            ),
            poster_b: crate::color(
                (92 + seed * 5 % 92) as u8,
                (54 + seed * 3 % 74) as u8,
                (28 + seed * 9 % 64) as u8,
            ),
            game_badge: row.game_badge.clone().unwrap_or_default().into(),
            marker_badge: row.marker_badge.clone().unwrap_or_default().into(),
            outcome_badge: row.outcome_badge.clone().unwrap_or_default().into(),
            upload_badge: row.upload_badge.clone().unwrap_or_default().into(),
            warning: row
                .warning
                .clone()
                .unwrap_or_else(|| poster_warning(&row.poster))
                .into(),
            selected: row.selected,
            active: row.active,
        });
    }

    let (load_state, error_text) = match &projection.load_state {
        CatalogLoadState::Empty => ("empty", ""),
        CatalogLoadState::Loading => ("loading", ""),
        CatalogLoadState::Ready => ("ready", ""),
        CatalogLoadState::Disconnected => ("disconnected", ""),
        CatalogLoadState::Error { message } => ("error", message.as_str()),
    };
    let menu = projection.menu.as_ref();
    let context_row = menu
        .and_then(|menu| {
            projection
                .rows
                .iter()
                .position(|row| row.identity == menu.target)
        })
        .and_then(|index| i32::try_from(index).ok())
        .unwrap_or(-1);
    let (dialog, audio_tracks) = dialog_models(projection);

    window.set_library_items(slint::ModelRc::new(slint::VecModel::from(rows)));
    window.set_catalog_source(match projection.source {
        CatalogSource::Local => SlintCatalogSource::Local,
        CatalogSource::Cloud => SlintCatalogSource::Cloud,
    });
    window.set_catalog_load_state(load_state.into());
    window.set_catalog_error_text(error_text.into());
    window.set_catalog_query(projection.controls.query.clone().into());
    window.set_catalog_filter(map_filter(projection.controls.local_filter));
    window.set_catalog_sort(map_sort(projection.controls.local_sort));
    window.set_catalog_group(map_group(projection.controls.local_grouping));
    window.set_catalog_range_text(projection.page.range_text.clone().into());
    window.set_catalog_page(i32::try_from(projection.page.page).unwrap_or(i32::MAX));
    window.set_catalog_has_previous(projection.page.has_previous);
    window.set_catalog_has_next(projection.page.has_next);
    window.set_catalog_selected_count(i32::try_from(projection.selected_count).unwrap_or(i32::MAX));
    window.set_catalog_selection_mode(projection.controls.selection_mode);
    window.set_catalog_context_row(context_row);
    window.set_catalog_can_review(menu.is_some_and(|menu| menu.can_review));
    window.set_catalog_can_rename(menu.is_some_and(|menu| menu.can_rename));
    window.set_catalog_can_delete(menu.is_some_and(|menu| menu.can_delete));
    window.set_catalog_can_upload(menu.is_some_and(|menu| menu.can_upload));
    window.set_catalog_can_reveal(menu.is_some_and(|menu| menu.can_reveal));
    window.set_catalog_can_copy_link(menu.is_some_and(|menu| menu.can_copy_link));
    window.set_catalog_can_open_link(menu.is_some_and(|menu| menu.can_open_browser));
    window.set_catalog_can_cancel_upload(menu_upload(projection).is_ok());
    window.set_catalog_dialog(dialog);
    window
        .set_catalog_dialog_audio_tracks(slint::ModelRc::new(slint::VecModel::from(audio_tracks)));
    Ok(())
}

fn dialog_models(projection: &CatalogProjection) -> (CatalogDialogModel, Vec<CatalogAudioTrack>) {
    let Some(dialog) = projection.dialog.as_ref() else {
        return (CatalogDialogModel::default(), Vec::new());
    };
    let kind = match dialog.kind {
        CatalogDialogKind::RenameTitle => SlintDialogKind::RenameTitle,
        CatalogDialogKind::RenameFile => SlintDialogKind::RenameFile,
        CatalogDialogKind::Delete => SlintDialogKind::Delete,
        CatalogDialogKind::Upload => SlintDialogKind::Upload,
        CatalogDialogKind::CancelUpload => SlintDialogKind::CancelUpload,
        CatalogDialogKind::PartialDelete => SlintDialogKind::PartialDelete,
    };
    let visibility = match dialog
        .visibility
        .unwrap_or(CatalogUploadVisibility::Private)
    {
        CatalogUploadVisibility::Private => SlintUploadVisibility::Private,
        CatalogUploadVisibility::Public => SlintUploadVisibility::Public,
        CatalogUploadVisibility::Unlisted => SlintUploadVisibility::Unlisted,
    };
    let audio_tracks = dialog
        .audio_tracks
        .iter()
        .map(|track| CatalogAudioTrack {
            label: track.label.clone().into(),
            selected: track.selected,
        })
        .collect();
    (
        CatalogDialogModel {
            open: true,
            kind,
            title: dialog.title.clone().into(),
            message: dialog.message.clone().into(),
            confirm_label: dialog.confirm_label.clone().into(),
            text_value: dialog.text_value.clone().unwrap_or_default().into(),
            description: dialog.description.clone().unwrap_or_default().into(),
            visibility,
            destructive: dialog.destructive,
            delete_local_after_upload: dialog.delete_local_after_upload,
            progress: dialog.progress.clone().unwrap_or_default().into(),
        },
        audio_tracks,
    )
}

fn map_filter(value: LocalClipFilter) -> CatalogFilter {
    match value {
        LocalClipFilter::All => CatalogFilter::All,
        LocalClipFilter::Replay => CatalogFilter::Replay,
        LocalClipFilter::Session => CatalogFilter::Session,
        LocalClipFilter::Trim => CatalogFilter::Trim,
        LocalClipFilter::Marked => CatalogFilter::Marked,
    }
}

fn map_sort(value: LocalClipSort) -> CatalogSort {
    match value {
        LocalClipSort::Newest => CatalogSort::Newest,
        LocalClipSort::Oldest => CatalogSort::Oldest,
        LocalClipSort::Largest => CatalogSort::Largest,
        LocalClipSort::Marks => CatalogSort::Marks,
    }
}

fn map_group(value: LocalClipGrouping) -> CatalogGroup {
    match value {
        LocalClipGrouping::Smart => CatalogGroup::Smart,
        LocalClipGrouping::Day => CatalogGroup::Day,
        LocalClipGrouping::Game => CatalogGroup::Game,
        LocalClipGrouping::Session => CatalogGroup::Session,
        LocalClipGrouping::None => CatalogGroup::Ungrouped,
    }
}

fn poster_warning(poster: &PresentationPoster) -> String {
    match poster {
        PresentationPoster::Failed { message } => message.clone(),
        PresentationPoster::Queued
        | PresentationPoster::Ready { .. }
        | PresentationPoster::Missing => String::new(),
    }
}

fn kind_color(kind: &str) -> slint::Color {
    match kind.trim().to_ascii_lowercase().as_str() {
        "session" => crate::color(100, 164, 214),
        "trim" => crate::color(116, 185, 126),
        "replay" => crate::color(217, 150, 42),
        _ => crate::color(132, 117, 102),
    }
}

fn row_identity(
    projection: &CatalogProjection,
    index: usize,
) -> Result<CatalogItemIdentity, CatalogUiError> {
    projection
        .rows
        .get(index)
        .map(|row| row.identity.clone())
        .ok_or(CatalogUiError::RowOutOfBounds {
            index,
            len: projection.rows.len(),
        })
}

fn menu_identity(projection: &CatalogProjection) -> Result<CatalogItemIdentity, CatalogUiError> {
    projection
        .menu
        .as_ref()
        .map(|menu| menu.target.clone())
        .ok_or(CatalogUiError::NoMenuTarget)
}

fn menu_upload(
    projection: &CatalogProjection,
) -> Result<&clipline_library::CatalogUploadProjection, CatalogUiError> {
    let target = projection
        .menu
        .as_ref()
        .ok_or(CatalogUiError::NoMenuTarget)?
        .target
        .local_path()
        .ok_or(CatalogUiError::NoCancelableUpload)?;
    projection
        .uploads
        .iter()
        .find(|upload| {
            &upload.token.source_path == target
                && matches!(
                    upload.summary.upload_status.as_str(),
                    "queued" | "retrying" | "preparing" | "uploading"
                )
        })
        .ok_or(CatalogUiError::NoCancelableUpload)
}

/// Minimal system resolver used by the candidate shell. Group keys are UTC
/// civil-day buckets; the retained browser adapter remains the locale-parity
/// oracle until the native localization milestone.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemDayResolver;

impl LocalDayResolver for SystemDayResolver {
    fn today_start_unix(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs() / 86_400 * 86_400)
    }

    fn resolve_day(&self, timestamp: u64) -> LocalDay {
        let day = timestamp / 86_400;
        LocalDay {
            key: format!("utc-day-{day}"),
            label: format!("Day {day}"),
        }
    }
}
