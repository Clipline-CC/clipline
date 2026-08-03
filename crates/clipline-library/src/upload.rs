//! Durable upload source and temporary-file ownership.
//!
//! This module deliberately stops at the filesystem ownership boundary. The
//! transport/state machine can retain these guards across preparation, HTTP,
//! persistence, and optional local deletion without depending on a UI runtime.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use clipline_shell::{opened_file_identity, FileIdentity};

use crate::{
    CloudAccountGeneration, CloudAccountKey, DurableUploadToken, LocalClipId, MutationLease,
    MutationPermit, ValidatedClipPath, ACTIVE_UPLOAD_MUTATION_ERROR,
};

const MAX_TEMP_RESERVATION_ATTEMPTS: usize = 128;
static UPLOAD_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UploadOwnershipError {
    #[error("upload source token does not match the validated clip path")]
    SourceTokenMismatch,
    #[error("another upload already owns this account and local clip")]
    DuplicateUpload,
    #[error("clip is being modified; retry the upload")]
    MutationActive,
    #[error("clip changed while acquiring the upload lease; retry the upload")]
    SourceChanged,
    #[error("other uploads still retain the source")]
    OtherReadersActive,
    #[error("upload mutation generation is exhausted")]
    GenerationExhausted,
    #[error("upload ownership registry is unavailable")]
    RegistryUnavailable,
    #[error("{action} {path:?}: {message}")]
    File {
        action: &'static str,
        path: PathBuf,
        message: String,
    },
}

impl UploadOwnershipError {
    fn file(action: &'static str, path: &Path, error: impl std::fmt::Display) -> Self {
        Self::File {
            action,
            path: path.to_path_buf(),
            message: error.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct UploadOwnerKey {
    account_key: CloudAccountKey,
    account_generation: CloudAccountGeneration,
    local_clip_id: LocalClipId,
}

impl From<&DurableUploadToken> for UploadOwnerKey {
    fn from(token: &DurableUploadToken) -> Self {
        Self {
            account_key: token.account_key.clone(),
            account_generation: token.account_generation,
            local_clip_id: token.local_clip_id.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UploadRegistration {
    token: DurableUploadToken,
    identity: FileIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExclusiveOwner {
    Mutation(u64),
    UploadDelete(DurableUploadToken),
}

#[derive(Debug, Default)]
struct SourceActivity {
    readers: HashSet<UploadOwnerKey>,
    exclusive: Option<ExclusiveOwner>,
}

#[derive(Debug, Default)]
struct RegistryState {
    files: HashMap<FileIdentity, SourceActivity>,
    uploads: HashMap<UploadOwnerKey, UploadRegistration>,
    mutation_sequence: u64,
}

#[derive(Debug, Default)]
struct RegistryInner {
    state: Mutex<RegistryState>,
}

/// Process-wide ownership seam shared by upload jobs and Library mutations.
///
/// The registry keys filesystem exclusion by stable file identity, so hard-link
/// aliases cannot bypass it. Upload admission is additionally keyed by exact
/// account generation and local clip ID; a newer upload generation cannot run
/// concurrently with an older job for the same durable owner.
#[derive(Debug, Clone, Default)]
pub struct ActiveFileRegistry {
    inner: Arc<RegistryInner>,
}

impl ActiveFileRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquire the original validated clip before any payload preparation.
    pub fn acquire_upload(
        &self,
        source: &ValidatedClipPath,
        token: DurableUploadToken,
    ) -> Result<UploadSourceLease, UploadOwnershipError> {
        if &token.source_path != source.comparison_identity() {
            return Err(UploadOwnershipError::SourceTokenMismatch);
        }
        let owner = UploadOwnerKey::from(&token);
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| UploadOwnershipError::RegistryUnavailable)?;
        if state.uploads.contains_key(&owner) {
            return Err(UploadOwnershipError::DuplicateUpload);
        }
        if state
            .files
            .get(&source.file_identity())
            .is_some_and(|activity| activity.exclusive.is_some())
        {
            return Err(UploadOwnershipError::MutationActive);
        }

        // Keep the registry lock across the restrictive no-follow open and
        // registration. A Clipline mutation either wins first or observes the
        // registered reader; it cannot pass through the middle.
        let file = clipline_shell::open_regular_file_nofollow_for_upload(source.canonical_path())
            .map_err(|error| {
            UploadOwnershipError::file("lease upload source", source.canonical_path(), error)
        })?;
        let identity = opened_file_identity(&file).map_err(|error| {
            UploadOwnershipError::file("identify upload source", source.canonical_path(), error)
        })?;
        if identity != source.file_identity() {
            return Err(UploadOwnershipError::SourceChanged);
        }

        let activity = state.files.entry(identity).or_default();
        if activity.exclusive.is_some() {
            return Err(UploadOwnershipError::MutationActive);
        }
        let inserted = activity.readers.insert(owner.clone());
        debug_assert!(inserted, "duplicate admission was checked before opening");
        state.uploads.insert(
            owner.clone(),
            UploadRegistration {
                token: token.clone(),
                identity,
            },
        );
        drop(state);

        Ok(UploadSourceLease {
            registry: self.clone(),
            owner,
            token,
            canonical_path: source.canonical_path().to_path_buf(),
            identity,
            file: Some(file),
            registered: true,
        })
    }

    /// True only while this exact token, including its upload generation and
    /// source path identity, still owns its registered source.
    pub fn is_current(&self, token: &DurableUploadToken) -> bool {
        let owner = UploadOwnerKey::from(token);
        self.inner.state.lock().is_ok_and(|state| {
            state
                .uploads
                .get(&owner)
                .is_some_and(|registration| &registration.token == token)
        })
    }
}

impl MutationLease for ActiveFileRegistry {
    fn acquire(
        &self,
        _canonical_path: &Path,
        identity: FileIdentity,
    ) -> Result<Box<dyn MutationPermit>, String> {
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| "upload ownership registry is unavailable".to_string())?;
        let activity = state.files.entry(identity).or_default();
        if !activity.readers.is_empty() || activity.exclusive.is_some() {
            return Err(ACTIVE_UPLOAD_MUTATION_ERROR.to_string());
        }
        state.mutation_sequence = state
            .mutation_sequence
            .checked_add(1)
            .ok_or_else(|| UploadOwnershipError::GenerationExhausted.to_string())?;
        let generation = state.mutation_sequence;
        state
            .files
            .get_mut(&identity)
            .expect("the source activity was inserted above")
            .exclusive = Some(ExclusiveOwner::Mutation(generation));
        Ok(Box::new(ActiveMutationPermit {
            registry: self.clone(),
            identity,
            generation,
        }))
    }
}

/// Retains both logical ownership and the platform's restrictive source handle.
pub struct UploadSourceLease {
    registry: ActiveFileRegistry,
    owner: UploadOwnerKey,
    token: DurableUploadToken,
    canonical_path: PathBuf,
    identity: FileIdentity,
    file: Option<File>,
    registered: bool,
}

impl std::fmt::Debug for UploadSourceLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UploadSourceLease")
            .field("token", &self.token)
            .field("canonical_path", &self.canonical_path)
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

impl UploadSourceLease {
    #[must_use]
    pub const fn token(&self) -> &DurableUploadToken {
        &self.token
    }

    #[must_use]
    pub fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }

    #[must_use]
    pub const fn identity(&self) -> FileIdentity {
        self.identity
    }

    /// Atomically become the sole delete owner for this source.
    ///
    /// The registry remains locked while the reader registration changes to an
    /// exclusive owner and the restrictive Windows handle is closed. Other
    /// Clipline mutations therefore cannot observe an idle source in between.
    pub fn into_delete_permit(mut self) -> Result<UploadDeletePermit, UploadOwnershipError> {
        let mut state = self
            .registry
            .inner
            .state
            .lock()
            .map_err(|_| UploadOwnershipError::RegistryUnavailable)?;
        let registration = state
            .uploads
            .get(&self.owner)
            .filter(|registration| {
                registration.token == self.token && registration.identity == self.identity
            })
            .ok_or(UploadOwnershipError::SourceChanged)?;
        debug_assert_eq!(registration.identity, self.identity);
        let activity = state
            .files
            .get_mut(&self.identity)
            .ok_or(UploadOwnershipError::SourceChanged)?;
        if activity.exclusive.is_some() || !activity.readers.contains(&self.owner) {
            return Err(UploadOwnershipError::SourceChanged);
        }
        if activity.readers.len() != 1 {
            return Err(UploadOwnershipError::OtherReadersActive);
        }
        activity.readers.remove(&self.owner);
        activity.exclusive = Some(ExclusiveOwner::UploadDelete(self.token.clone()));

        // Close the kernel handle before allowing the identity-fenced delete,
        // but while the logical exclusive owner is still protected by `state`.
        drop(self.file.take());
        self.registered = false;
        drop(state);

        Ok(UploadDeletePermit {
            registry: self.registry.clone(),
            owner: self.owner.clone(),
            token: self.token.clone(),
            canonical_path: self.canonical_path.clone(),
            identity: self.identity,
            registered: true,
        })
    }

    fn release(&mut self) {
        if !self.registered {
            return;
        }
        let Ok(mut state) = self.registry.inner.state.lock() else {
            // Fail closed: a poisoned registry keeps the ownership marker rather
            // than risking release of a different generation.
            return;
        };
        let exact = state.uploads.get(&self.owner).is_some_and(|registration| {
            registration.token == self.token && registration.identity == self.identity
        });
        if !exact {
            return;
        }

        // Mutations are serialized on the same registry lock. Release the OS
        // handle first so the next permitted mutation cannot receive a sharing
        // violation after being told the upload is inactive.
        drop(self.file.take());
        state.uploads.remove(&self.owner);
        if let Some(activity) = state.files.get_mut(&self.identity) {
            activity.readers.remove(&self.owner);
            if activity.readers.is_empty() && activity.exclusive.is_none() {
                state.files.remove(&self.identity);
            }
        }
        self.registered = false;
    }
}

impl Drop for UploadSourceLease {
    fn drop(&mut self) {
        self.release();
    }
}

/// Exclusive source owner used for delete-local-after-upload.
pub struct UploadDeletePermit {
    registry: ActiveFileRegistry,
    owner: UploadOwnerKey,
    token: DurableUploadToken,
    canonical_path: PathBuf,
    identity: FileIdentity,
    registered: bool,
}

impl std::fmt::Debug for UploadDeletePermit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UploadDeletePermit")
            .field("token", &self.token)
            .field("canonical_path", &self.canonical_path)
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

impl UploadDeletePermit {
    #[must_use]
    pub const fn token(&self) -> &DurableUploadToken {
        &self.token
    }

    #[must_use]
    pub fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }

    #[must_use]
    pub const fn identity(&self) -> FileIdentity {
        self.identity
    }

    /// Delete the exact source authorized by this permit. A replacement at the
    /// same path is preserved by the shell identity fence.
    pub fn delete_source_if_current(&self) -> Result<(), UploadOwnershipError> {
        if !self.registry.is_current(&self.token) {
            return Err(UploadOwnershipError::SourceChanged);
        }
        clipline_shell::remove_file_if_identity(&self.canonical_path, self.identity).map_err(
            |error| UploadOwnershipError::file("delete upload source", &self.canonical_path, error),
        )
    }
}

impl MutationPermit for UploadDeletePermit {}

impl Drop for UploadDeletePermit {
    fn drop(&mut self) {
        if !self.registered {
            return;
        }
        let Ok(mut state) = self.registry.inner.state.lock() else {
            return;
        };
        let exact_registration = state.uploads.get(&self.owner).is_some_and(|registration| {
            registration.token == self.token && registration.identity == self.identity
        });
        let exact_exclusive = state
            .files
            .get(&self.identity)
            .and_then(|activity| activity.exclusive.as_ref())
            == Some(&ExclusiveOwner::UploadDelete(self.token.clone()));
        if !exact_registration || !exact_exclusive {
            return;
        }
        state.uploads.remove(&self.owner);
        if let Some(activity) = state.files.get_mut(&self.identity) {
            activity.exclusive = None;
            if activity.readers.is_empty() {
                state.files.remove(&self.identity);
            }
        }
        self.registered = false;
    }
}

#[derive(Debug)]
struct ActiveMutationPermit {
    registry: ActiveFileRegistry,
    identity: FileIdentity,
    generation: u64,
}

impl MutationPermit for ActiveMutationPermit {}

impl Drop for ActiveMutationPermit {
    fn drop(&mut self) {
        let Ok(mut state) = self.registry.inner.state.lock() else {
            return;
        };
        let exact = state
            .files
            .get(&self.identity)
            .and_then(|activity| activity.exclusive.as_ref())
            == Some(&ExclusiveOwner::Mutation(self.generation));
        if !exact {
            return;
        }
        if let Some(activity) = state.files.get_mut(&self.identity) {
            activity.exclusive = None;
            if activity.readers.is_empty() {
                state.files.remove(&self.identity);
            }
        }
    }
}

/// Create-new upload payload with exact Drop ownership.
///
/// After sealing, downstream transport may reopen the path. Drop removes it
/// only if the path still names the exact file created here.
pub struct OwnedUploadTemp {
    path: PathBuf,
    identity: FileIdentity,
    file: Option<File>,
}

impl std::fmt::Debug for OwnedUploadTemp {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OwnedUploadTemp")
            .field("path", &self.path)
            .field("identity", &self.identity)
            .field("open", &self.file.is_some())
            .finish()
    }
}

impl OwnedUploadTemp {
    pub fn create_near(source: &Path) -> Result<Self, UploadOwnershipError> {
        let parent = source.parent().ok_or_else(|| {
            UploadOwnershipError::file(
                "reserve upload payload",
                source,
                "source path has no parent directory",
            )
        })?;
        let file_name = source.file_name().ok_or_else(|| {
            UploadOwnershipError::file(
                "reserve upload payload",
                source,
                "source path has no file name",
            )
        })?;
        for _ in 0..MAX_TEMP_RESERVATION_ATTEMPTS {
            let sequence = UPLOAD_TEMP_SEQUENCE
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                    value.checked_add(1)
                })
                .map_err(|_| UploadOwnershipError::GenerationExhausted)?
                + 1;
            let mut temp_name = file_name.to_os_string();
            temp_name.push(format!(
                ".clipline-upload-{}-{sequence}.tmp",
                std::process::id()
            ));
            let path = parent.join(temp_name);
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => {
                    let identity = opened_file_identity(&file).map_err(|error| {
                        UploadOwnershipError::file("identify upload payload", &path, error)
                    })?;
                    return Ok(Self {
                        path,
                        identity,
                        file: Some(file),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(UploadOwnershipError::file(
                        "reserve upload payload",
                        &path,
                        error,
                    ));
                }
            }
        }
        Err(UploadOwnershipError::file(
            "reserve upload payload",
            source,
            "unique-name attempts exhausted",
        ))
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn identity(&self) -> FileIdentity {
        self.identity
    }

    pub fn file_mut(&mut self) -> Result<&mut File, UploadOwnershipError> {
        self.file.as_mut().ok_or_else(|| {
            UploadOwnershipError::file("write upload payload", &self.path, "payload is sealed")
        })
    }

    /// Sync, close, and verify the exact file before it is handed to transport.
    pub fn seal(&mut self) -> Result<(), UploadOwnershipError> {
        let Some(file) = self.file.take() else {
            return self.verify_current();
        };
        file.sync_all().map_err(|error| {
            UploadOwnershipError::file("sync upload payload", &self.path, error)
        })?;
        drop(file);
        self.verify_current()
    }

    pub fn verify_current(&self) -> Result<(), UploadOwnershipError> {
        let file = clipline_shell::open_regular_file_nofollow(&self.path).map_err(|error| {
            UploadOwnershipError::file("open upload payload", &self.path, error)
        })?;
        let identity = opened_file_identity(&file).map_err(|error| {
            UploadOwnershipError::file("identify upload payload", &self.path, error)
        })?;
        if identity == self.identity {
            Ok(())
        } else {
            Err(UploadOwnershipError::SourceChanged)
        }
    }
}

impl Drop for OwnedUploadTemp {
    fn drop(&mut self) {
        drop(self.file.take());
        let _ = clipline_shell::remove_file_if_identity(&self.path, self.identity);
    }
}
