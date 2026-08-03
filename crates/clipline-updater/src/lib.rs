//! Framework-neutral, fail-closed update policy for Clipline.

pub mod download;
pub mod manifest;

#[cfg(windows)]
pub mod windows;

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use minisign_verify::{PublicKey, Signature};
use sha2::{Digest, Sha256};
use thiserror::Error;

use download::DownloadTelemetry;

/// The same Minisign public key embedded in the shipping Tauri updater config.
///
/// Keep this decoded text in source so the framework-neutral verifier does not
/// depend on Tauri's double-base64 configuration representation.
pub const CLIPLINE_MINISIGN_PUBLIC_KEY: &str = "untrusted comment: minisign public key: 89E05097264BE6E6\nRWTm5ksml1DgiXheR48k0mC5ue9mQsnaK0Pa3S8G8virP7ar6HIOLunZ\n";

const VERIFY_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateOperationKind {
    Check,
    Install,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateOperationSnapshot {
    pub id: u64,
    pub kind: UpdateOperationKind,
    pub cancelled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum UpdateOperationError {
    #[error("an update {active:?} operation is already active; cannot start {requested:?}")]
    Busy {
        active: UpdateOperationKind,
        requested: UpdateOperationKind,
    },
    #[error("updates are quiescing for process shutdown; cannot start {requested:?}")]
    Quiescing { requested: UpdateOperationKind },
    #[error("the update operation identity is exhausted")]
    IdentityExhausted,
    #[error("the update operation gate is unavailable")]
    Unavailable,
    #[error("timed out waiting for update {active:?} operation {id} to stop")]
    WaitTimedOut {
        id: u64,
        active: UpdateOperationKind,
    },
}

struct ActiveUpdateOperation {
    id: u64,
    kind: UpdateOperationKind,
    cancelled: Arc<AtomicBool>,
}

#[derive(Default)]
struct UpdateOperationState {
    last_id: u64,
    active: Option<ActiveUpdateOperation>,
    quiescing: bool,
}

/// Process-local ownership for update checks and installs.
///
/// The lease is RAII so every early return releases the gate. Cancellation is
/// an explicit shared flag that a bounded downloader can observe without a UI
/// or framework dependency.
#[derive(Default)]
pub struct UpdateOperationGate {
    state: Mutex<UpdateOperationState>,
    idle: Condvar,
}

impl UpdateOperationGate {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: Mutex::new(UpdateOperationState {
                last_id: 0,
                active: None,
                quiescing: false,
            }),
            idle: Condvar::new(),
        }
    }

    pub fn begin(
        &self,
        requested: UpdateOperationKind,
    ) -> Result<UpdateOperationLease<'_>, UpdateOperationError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| UpdateOperationError::Unavailable)?;
        if state.quiescing {
            return Err(UpdateOperationError::Quiescing { requested });
        }
        if let Some(active) = &state.active {
            return Err(UpdateOperationError::Busy {
                active: active.kind,
                requested,
            });
        }
        let id = state
            .last_id
            .checked_add(1)
            .ok_or(UpdateOperationError::IdentityExhausted)?;
        let cancelled = Arc::new(AtomicBool::new(false));
        state.last_id = id;
        state.active = Some(ActiveUpdateOperation {
            id,
            kind: requested,
            cancelled: Arc::clone(&cancelled),
        });
        Ok(UpdateOperationLease {
            gate: self,
            id,
            cancelled,
            release_on_drop: true,
        })
    }

    #[must_use]
    pub fn cancel_active(&self) -> bool {
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(active) = &state.active else {
            return false;
        };
        active.cancelled.store(true, Ordering::Release);
        true
    }

    /// Prevent new update work, cancel the current operation, and wait for its
    /// RAII lease to release. The returned lease keeps the gate quiescent for
    /// the caller's shutdown transaction.
    pub fn quiesce_and_wait(
        &self,
        timeout: Duration,
    ) -> Result<UpdateQuiescenceLease<'_>, UpdateOperationError> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(UpdateOperationError::Unavailable)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| UpdateOperationError::Unavailable)?;
        if state.quiescing {
            return Err(UpdateOperationError::Unavailable);
        }
        state.quiescing = true;
        let tracked = state.active.as_ref().map(|active| {
            active.cancelled.store(true, Ordering::Release);
            UpdateOperationSnapshot {
                id: active.id,
                kind: active.kind,
                cancelled: true,
            }
        });

        while let Some(tracked) = tracked {
            if state
                .active
                .as_ref()
                .is_none_or(|active| active.id != tracked.id)
            {
                return Ok(UpdateQuiescenceLease {
                    gate: self,
                    completed: Some(tracked),
                    release_on_drop: true,
                });
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                state.quiescing = false;
                self.idle.notify_all();
                return Err(UpdateOperationError::WaitTimedOut {
                    id: tracked.id,
                    active: tracked.kind,
                });
            }
            let (next, wait) = self
                .idle
                .wait_timeout(state, remaining)
                .map_err(|_| UpdateOperationError::Unavailable)?;
            state = next;
            if wait.timed_out()
                && state
                    .active
                    .as_ref()
                    .is_some_and(|active| active.id == tracked.id)
            {
                state.quiescing = false;
                self.idle.notify_all();
                return Err(UpdateOperationError::WaitTimedOut {
                    id: tracked.id,
                    active: tracked.kind,
                });
            }
        }

        Ok(UpdateQuiescenceLease {
            gate: self,
            completed: None,
            release_on_drop: true,
        })
    }

    #[must_use]
    pub fn snapshot(&self) -> Option<UpdateOperationSnapshot> {
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.active.as_ref().map(|active| UpdateOperationSnapshot {
            id: active.id,
            kind: active.kind,
            cancelled: active.cancelled.load(Ordering::Acquire),
        })
    }
}

pub struct UpdateOperationLease<'a> {
    gate: &'a UpdateOperationGate,
    id: u64,
    cancelled: Arc<AtomicBool>,
    release_on_drop: bool,
}

impl UpdateOperationLease<'_> {
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }

    #[must_use]
    pub fn cancellation(&self) -> &AtomicBool {
        &self.cancelled
    }

    /// Keep the operation latched because application exit is committed.
    pub fn commit_exit(mut self) {
        self.release_on_drop = false;
    }
}

impl Drop for UpdateOperationLease<'_> {
    fn drop(&mut self) {
        if !self.release_on_drop {
            return;
        }
        let mut state = match self.gate.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        if state
            .active
            .as_ref()
            .is_some_and(|active| active.id == self.id)
        {
            state.active = None;
            self.gate.idle.notify_all();
        }
    }
}

pub struct UpdateQuiescenceLease<'a> {
    gate: &'a UpdateOperationGate,
    completed: Option<UpdateOperationSnapshot>,
    release_on_drop: bool,
}

impl UpdateQuiescenceLease<'_> {
    #[must_use]
    pub const fn completed(&self) -> Option<UpdateOperationSnapshot> {
        self.completed
    }

    /// Keep the gate permanently quiescent because application exit is now committed.
    pub fn commit_exit(mut self) {
        self.release_on_drop = false;
    }
}

impl Drop for UpdateQuiescenceLease<'_> {
    fn drop(&mut self) {
        if !self.release_on_drop {
            return;
        }
        let mut state = match self.gate.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.quiescing = false;
        self.gate.idle.notify_all();
    }
}

#[derive(Debug, Error)]
pub enum VerificationError {
    #[error("the installer filename is invalid")]
    InvalidFilename,
    #[error("the downloaded installer path has no filename")]
    MissingDownloadedFilename,
    #[error("the update signature is not valid base64")]
    SignatureBase64,
    #[error("the update signature is not valid Minisign data")]
    SignatureEncoding,
    #[error("the embedded update public key is invalid")]
    PublicKey,
    #[error("the signature trusted comment does not name the selected installer")]
    FilenameMismatch,
    #[error("open the downloaded installer for verification: {0}")]
    Open(#[source] std::io::Error),
    #[error("read the downloaded installer for verification: {0}")]
    Read(#[source] std::io::Error),
    #[error("the installer signature did not verify")]
    InvalidSignature,
    #[error("the verified installer bytes do not match the download telemetry")]
    TelemetryMismatch,
}

/// An installer whose exact bytes and selected release filename passed Minisign verification.
///
/// Fields are private so the Windows launcher cannot be reached with an unverified path.
#[derive(Debug)]
pub struct VerifiedInstaller {
    path: PathBuf,
    release_filename: String,
    telemetry: DownloadTelemetry,
    cleanup_on_drop: bool,
}

impl VerifiedInstaller {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn release_filename(&self) -> &str {
        &self.release_filename
    }

    #[must_use]
    pub const fn telemetry(&self) -> &DownloadTelemetry {
        &self.telemetry
    }

    #[cfg(windows)]
    fn transfer_cleanup(mut self) -> (PathBuf, DownloadTelemetry) {
        self.cleanup_on_drop = false;
        (self.path.clone(), self.telemetry.clone())
    }
}

impl Drop for VerifiedInstaller {
    fn drop(&mut self) {
        if self.cleanup_on_drop {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Verify a completed download with Clipline's embedded release key.
///
/// The manifest signature is the outer base64 string produced by Tauri's signer.
/// Failure removes the invocation-owned download before returning.
pub fn verify_download(
    telemetry: DownloadTelemetry,
    signature_base64: &str,
    release_filename: &str,
) -> Result<VerifiedInstaller, VerificationError> {
    verify_download_with_key(
        telemetry,
        signature_base64,
        release_filename,
        CLIPLINE_MINISIGN_PUBLIC_KEY,
    )
}

/// Test seam for exercising the exact production verification path with a disposable key.
#[doc(hidden)]
pub fn verify_download_with_key(
    telemetry: DownloadTelemetry,
    signature_base64: &str,
    release_filename: &str,
    public_key_text: &str,
) -> Result<VerifiedInstaller, VerificationError> {
    let result = verify_download_inner(
        telemetry.clone(),
        signature_base64,
        release_filename,
        public_key_text,
    );
    if result.is_err() {
        let _ = std::fs::remove_file(&telemetry.destination);
    }
    result
}

fn verify_download_inner(
    telemetry: DownloadTelemetry,
    signature_base64: &str,
    release_filename: &str,
    public_key_text: &str,
) -> Result<VerifiedInstaller, VerificationError> {
    validate_release_filename(release_filename)?;
    telemetry
        .destination
        .file_name()
        .ok_or(VerificationError::MissingDownloadedFilename)?;

    let signature_text = STANDARD
        .decode(signature_base64)
        .map_err(|_| VerificationError::SignatureBase64)?;
    let signature_text =
        std::str::from_utf8(&signature_text).map_err(|_| VerificationError::SignatureEncoding)?;
    let signature =
        Signature::decode(signature_text).map_err(|_| VerificationError::SignatureEncoding)?;
    let public_key =
        PublicKey::decode(public_key_text).map_err(|_| VerificationError::PublicKey)?;

    if !trusted_comment_names_file(signature.trusted_comment(), release_filename) {
        return Err(VerificationError::FilenameMismatch);
    }

    let mut verifier = public_key
        .verify_stream(&signature)
        .map_err(|_| VerificationError::InvalidSignature)?;
    let mut file = File::open(&telemetry.destination).map_err(VerificationError::Open)?;
    let mut buffer = [0_u8; VERIFY_BUFFER_BYTES];
    let mut sha256 = Sha256::new();
    let mut bytes_read = 0_u64;
    loop {
        let read = file.read(&mut buffer).map_err(VerificationError::Read)?;
        if read == 0 {
            break;
        }
        verifier.update(&buffer[..read]);
        sha256.update(&buffer[..read]);
        bytes_read = bytes_read
            .checked_add(u64::try_from(read).map_err(|_| VerificationError::TelemetryMismatch)?)
            .ok_or(VerificationError::TelemetryMismatch)?;
    }
    verifier
        .finalize()
        .map_err(|_| VerificationError::InvalidSignature)?;
    let verified_sha256: [u8; 32] = sha256.finalize().into();
    if bytes_read != telemetry.bytes_written || verified_sha256 != telemetry.sha256 {
        return Err(VerificationError::TelemetryMismatch);
    }

    Ok(VerifiedInstaller {
        path: telemetry.destination.clone(),
        release_filename: release_filename.to_owned(),
        telemetry,
        cleanup_on_drop: true,
    })
}

fn validate_release_filename(filename: &str) -> Result<(), VerificationError> {
    let path = Path::new(filename);
    if filename.is_empty()
        || filename.contains(['\r', '\n', '\t', '\0'])
        || path.file_name().and_then(|name| name.to_str()) != Some(filename)
    {
        return Err(VerificationError::InvalidFilename);
    }
    Ok(())
}

fn trusted_comment_names_file(comment: &str, filename: &str) -> bool {
    comment
        .split('\t')
        .any(|field| field.strip_prefix("file:") == Some(filename))
}

/// A process handoff prepared from a verified installer but not yet allowed to run.
pub trait PreparedInstallerHandoff {
    type Receipt;
    type Error: std::error::Error + Send + Sync + 'static;

    /// Allow the already-created installer process to begin execution.
    fn commit(self) -> Result<Self::Receipt, Self::Error>;
}

/// Creates a suspended/passive installer handoff from verified bytes only.
pub trait InstallerLauncher {
    type Prepared: PreparedInstallerHandoff;

    fn prepare(
        &mut self,
        installer: VerifiedInstaller,
    ) -> Result<Self::Prepared, <Self::Prepared as PreparedInstallerHandoff>::Error>;
}

/// Shutdown effects that must become durable before Clipline exits for an update.
pub trait UpdateShutdown {
    type Error: std::error::Error + Send + Sync + 'static;

    fn publish_durable_state(&mut self) -> Result<(), Self::Error>;
    fn stop_window_media(&mut self) -> Result<(), Self::Error>;
    fn stop_recorder(&mut self) -> Result<(), Self::Error>;
    fn flush_diagnostics(&mut self) -> Result<(), Self::Error>;
    fn request_exit(&mut self) -> Result<(), Self::Error>;
}

#[derive(Debug, Error)]
pub enum InstallFlowError<LaunchError, ShutdownError> {
    #[error("prepare passive installer handoff: {0}")]
    Prepare(LaunchError),
    #[error("publish durable shutdown state: {0}")]
    DurableState(ShutdownError),
    #[error("stop window media: {0}")]
    WindowMedia(ShutdownError),
    #[error("stop recorder: {0}")]
    Recorder(ShutdownError),
    #[error("flush diagnostics: {0}")]
    Diagnostics(ShutdownError),
    #[error("launch passive installer: {0}")]
    Commit(LaunchError),
    #[error("request application exit: {0}")]
    Exit(ShutdownError),
}

pub type LauncherReceipt<L> =
    <<L as InstallerLauncher>::Prepared as PreparedInstallerHandoff>::Receipt;
pub type LauncherError<L> = <<L as InstallerLauncher>::Prepared as PreparedInstallerHandoff>::Error;
pub type InstallFlowResult<L, S> =
    Result<LauncherReceipt<L>, InstallFlowError<LauncherError<L>, <S as UpdateShutdown>::Error>>;

/// Prepare the verified child first, then complete the durable shutdown sequence, then run it.
///
/// A prepared implementation must keep the installer suspended and terminate it on drop. Thus a
/// service-stop failure leaves Clipline running and cannot race an active installer.
pub fn install_verified<L, S>(
    launcher: &mut L,
    shutdown: &mut S,
    installer: VerifiedInstaller,
) -> InstallFlowResult<L, S>
where
    L: InstallerLauncher,
    S: UpdateShutdown,
{
    let prepared = launcher
        .prepare(installer)
        .map_err(InstallFlowError::Prepare)?;
    shutdown
        .publish_durable_state()
        .map_err(InstallFlowError::DurableState)?;
    shutdown
        .stop_window_media()
        .map_err(InstallFlowError::WindowMedia)?;
    shutdown
        .stop_recorder()
        .map_err(InstallFlowError::Recorder)?;
    shutdown
        .flush_diagnostics()
        .map_err(InstallFlowError::Diagnostics)?;
    let receipt = prepared.commit().map_err(InstallFlowError::Commit)?;
    shutdown.request_exit().map_err(InstallFlowError::Exit)?;
    Ok(receipt)
}
