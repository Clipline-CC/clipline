//! Canonical, framework-neutral authority for local Library mutations.

use std::fmt;
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use clipline_shell::{
    open_directory_nofollow, open_regular_file_nofollow, opened_file_identity, FileIdentity,
    FileMutationError, FileMutationFence, ReplacementRecoveryReport,
};
use serde::{Deserialize, Serialize};

use crate::{
    inferred_clip_kind_for_path, normalized_clip_file_name, normalized_clip_title,
    ClipPathIdentity, MAX_MUTATION_ITEMS, MAX_MUTATION_PATH_BYTES,
};

pub const ACTIVE_UPLOAD_MUTATION_ERROR: &str = "clip is uploading; wait for the upload to finish";
pub const INVALID_CLIP_PATH_ERROR: &str = "refusing to access a clip outside the clips directory";
pub const CHANGED_CLIP_ERROR: &str =
    "clip changed since it was selected; refresh the Library and try again";

const CLIP_COLLISION_ERROR: &str = "a clip with that name already exists";
const MARKER_COLLISION_ERROR: &str = "a marker sidecar with that name already exists";
const METADATA_COLLISION_ERROR: &str = "a clip metadata sidecar with that name already exists";
const PENDING_OSU_COLLISION_ERROR: &str =
    "an osu! enrichment sidecar with that name already exists";
const POSTER_COLLISION_ERROR: &str = "a poster sidecar with that name already exists";
pub const MAX_PENDING_OSU_BYTES: u64 = 8 * 1024 * 1024;
const MAX_CLIP_METADATA_BYTES: u64 = 64 * 1024;
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileSystemEntry {
    regular_file: bool,
    length: u64,
    identity: FileIdentity,
}

impl FileSystemEntry {
    #[must_use]
    pub const fn new(regular_file: bool, length: u64, identity: FileIdentity) -> Self {
        Self {
            regular_file,
            length,
            identity,
        }
    }

    #[must_use]
    pub const fn is_regular_file(self) -> bool {
        self.regular_file
    }

    #[must_use]
    pub const fn length(self) -> u64 {
        self.length
    }

    #[must_use]
    pub const fn identity(self) -> FileIdentity {
        self.identity
    }
}

/// Synchronous filesystem seam used by the repository and deterministic tests.
///
/// Platform replacement and stable identity remain behind `clipline-shell`'s safe
/// wrappers. Production callers normally use [`StandardRepositoryFileSystem`].
pub trait RepositoryFileSystem: Send + Sync {
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;
    fn is_directory(&self, path: &Path) -> io::Result<bool>;
    fn entry(&self, path: &Path) -> io::Result<FileSystemEntry>;
    fn try_exists(&self, path: &Path) -> io::Result<bool>;
    fn recover_pending_replacements(
        &self,
        root: &Path,
        expected_root_identity: FileIdentity,
    ) -> io::Result<ReplacementRecoveryReport>;
    fn read_bounded_if_identity(
        &self,
        path: &Path,
        expected_identity: FileIdentity,
        maximum_bytes: u64,
    ) -> io::Result<Vec<u8>>;
    fn create_new_synced(
        &self,
        path: &Path,
        bytes: &[u8],
    ) -> Result<FileIdentity, CreateNewFileError>;
    fn acquire_mutation_fence(
        &self,
        path: &Path,
        source_identity: FileIdentity,
        parent_identity: FileIdentity,
    ) -> io::Result<Box<dyn RepositoryMutationFence>>;
    fn rename_noreplace_if_identity(
        &self,
        from: &Path,
        to: &Path,
        identity: FileIdentity,
    ) -> Result<(), FileMutationError>;
    fn replace_if_identities(
        &self,
        from: &Path,
        from_identity: FileIdentity,
        to: &Path,
        to_identity: FileIdentity,
    ) -> Result<(), FileMutationError>;
    fn remove_file_if_identity(
        &self,
        path: &Path,
        identity: FileIdentity,
    ) -> Result<(), FileMutationError>;
}

/// Creation failure that records whether the failing call owns the path.
///
/// Callers may clean up only `created_path()` failures. A pre-create collision
/// belongs to somebody else and must never be removed as rollback.
#[derive(Debug)]
pub struct CreateNewFileError {
    error: io::Error,
    created_path: bool,
    created_identity: Option<FileIdentity>,
}

impl CreateNewFileError {
    #[must_use]
    pub fn before_create(error: io::Error) -> Self {
        Self {
            error,
            created_path: false,
            created_identity: None,
        }
    }

    #[must_use]
    pub fn after_create(error: io::Error, created_identity: Option<FileIdentity>) -> Self {
        Self {
            error,
            created_path: true,
            created_identity,
        }
    }

    #[must_use]
    pub const fn created_path(&self) -> bool {
        self.created_path
    }

    #[must_use]
    pub const fn created_identity(&self) -> Option<FileIdentity> {
        self.created_identity
    }
}

impl fmt::Display for CreateNewFileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for CreateNewFileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// Handle-backed authority used for the selected primary clip.
pub trait RepositoryMutationFence: Send {
    fn rename_noreplace(&mut self, target: &Path) -> Result<(), FileMutationError>;
    fn delete(&mut self) -> Result<(), FileMutationError>;
}

struct StandardMutationFence(FileMutationFence);

impl RepositoryMutationFence for StandardMutationFence {
    fn rename_noreplace(&mut self, target: &Path) -> Result<(), FileMutationError> {
        self.0.rename_noreplace(target)
    }

    fn delete(&mut self) -> Result<(), FileMutationError> {
        self.0.delete()
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct StandardRepositoryFileSystem;

impl RepositoryFileSystem for StandardRepositoryFileSystem {
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        path.canonicalize()
    }

    fn is_directory(&self, path: &Path) -> io::Result<bool> {
        match open_directory_nofollow(path) {
            Ok(_) => Ok(true),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotADirectory | io::ErrorKind::PermissionDenied
                ) =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    fn entry(&self, path: &Path) -> io::Result<FileSystemEntry> {
        let metadata = std::fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() || has_windows_reparse_attribute(&metadata) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "refusing to follow a filesystem link or reparse point",
            ));
        }
        if metadata.is_file() {
            let file = open_regular_file_nofollow(path)?;
            let opened_metadata = file.metadata()?;
            return Ok(FileSystemEntry {
                regular_file: opened_metadata.is_file(),
                length: opened_metadata.len(),
                identity: opened_file_identity(&file)?,
            });
        }
        if metadata.is_dir() {
            let directory = open_directory_nofollow(path)?;
            let opened_metadata = directory.metadata()?;
            return Ok(FileSystemEntry {
                regular_file: false,
                length: opened_metadata.len(),
                identity: opened_file_identity(&directory)?,
            });
        }
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing to inspect a non-file, non-directory filesystem object",
        ))
    }

    fn try_exists(&self, path: &Path) -> io::Result<bool> {
        path.try_exists()
    }

    fn recover_pending_replacements(
        &self,
        root: &Path,
        expected_root_identity: FileIdentity,
    ) -> io::Result<ReplacementRecoveryReport> {
        clipline_shell::recover_pending_replacements_in_tree_if_identity(
            root,
            expected_root_identity,
        )
    }

    fn read_bounded_if_identity(
        &self,
        path: &Path,
        expected_identity: FileIdentity,
        maximum_bytes: u64,
    ) -> io::Result<Vec<u8>> {
        let mut file = open_regular_file_nofollow(path)?;
        if opened_file_identity(&file)? != expected_identity {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "filesystem object changed before bounded read",
            ));
        }
        if file.metadata()?.len() > maximum_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file exceeds the configured byte limit",
            ));
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(maximum_bytes.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > maximum_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file exceeds the configured byte limit",
            ));
        }
        Ok(bytes)
    }

    fn create_new_synced(
        &self,
        path: &Path,
        bytes: &[u8],
    ) -> Result<FileIdentity, CreateNewFileError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(CreateNewFileError::before_create)?;
        let identity = opened_file_identity(&file)
            .map_err(|error| CreateNewFileError::after_create(error, None))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| CreateNewFileError::after_create(error, Some(identity)))?;
        Ok(identity)
    }

    fn acquire_mutation_fence(
        &self,
        path: &Path,
        source_identity: FileIdentity,
        parent_identity: FileIdentity,
    ) -> io::Result<Box<dyn RepositoryMutationFence>> {
        FileMutationFence::acquire(path, source_identity, parent_identity)
            .map(StandardMutationFence)
            .map(|fence| Box::new(fence) as Box<dyn RepositoryMutationFence>)
    }

    fn rename_noreplace_if_identity(
        &self,
        from: &Path,
        to: &Path,
        identity: FileIdentity,
    ) -> Result<(), FileMutationError> {
        clipline_shell::rename_file_noreplace_if_identity(from, to, identity)
    }

    fn replace_if_identities(
        &self,
        from: &Path,
        from_identity: FileIdentity,
        to: &Path,
        to_identity: FileIdentity,
    ) -> Result<(), FileMutationError> {
        clipline_shell::replace_file_if_identities(from, from_identity, to, to_identity)
    }

    fn remove_file_if_identity(
        &self,
        path: &Path,
        identity: FileIdentity,
    ) -> Result<(), FileMutationError> {
        clipline_shell::remove_file_if_identity(path, identity)
    }
}

/// Permit retained for the entire destructive transaction.
pub trait MutationPermit: Send {}

/// Lease seam used to atomically exclude uploads and destructive mutations.
pub trait MutationLease: Send + Sync {
    fn acquire(
        &self,
        canonical_path: &Path,
        identity: FileIdentity,
    ) -> Result<Box<dyn MutationPermit>, String>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct NoActiveMutationLease;

#[derive(Debug)]
struct NoopMutationPermit;

impl MutationPermit for NoopMutationPermit {}

impl MutationLease for NoActiveMutationLease {
    fn acquire(
        &self,
        _canonical_path: &Path,
        _identity: FileIdentity,
    ) -> Result<Box<dyn MutationPermit>, String> {
        Ok(Box::new(NoopMutationPermit))
    }
}

/// Validated authority for one selected clip.
///
/// `display_path` is reconciliation/presentation data only. All I/O uses the
/// canonical path, and destructive operations compare `file_identity` again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedClipPath {
    display_path: String,
    canonical_path: PathBuf,
    comparison_identity: ClipPathIdentity,
    file_identity: FileIdentity,
    parent_identity: FileIdentity,
}

impl ValidatedClipPath {
    #[must_use]
    pub fn display_path(&self) -> &str {
        &self.display_path
    }

    #[must_use]
    pub fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }

    #[must_use]
    pub const fn comparison_identity(&self) -> &ClipPathIdentity {
        &self.comparison_identity
    }

    #[must_use]
    pub const fn file_identity(&self) -> FileIdentity {
        self.file_identity
    }

    #[must_use]
    pub const fn parent_identity(&self) -> FileIdentity {
        self.parent_identity
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryError {
    primary: String,
    rollback_failures: Vec<String>,
}

impl RepositoryError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            primary: message.into(),
            rollback_failures: Vec::new(),
        }
    }

    fn with_rollback(message: impl Into<String>, rollback_failures: Vec<String>) -> Self {
        Self {
            primary: message.into(),
            rollback_failures,
        }
    }

    #[must_use]
    pub fn primary(&self) -> &str {
        &self.primary
    }

    #[must_use]
    pub fn rollback_failures(&self) -> &[String] {
        &self.rollback_failures
    }
}

impl fmt::Display for RepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.primary)?;
        if !self.rollback_failures.is_empty() {
            formatter.write_str("; rollback failures: ")?;
            for (index, failure) in self.rollback_failures.iter().enumerate() {
                if index != 0 {
                    formatter.write_str(" | ")?;
                }
                formatter.write_str(failure)?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for RepositoryError {}

impl From<io::Error> for RepositoryError {
    fn from(error: io::Error) -> Self {
        Self::new(error.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenamedClipInfo {
    pub old_path: String,
    pub path: String,
    pub name: String,
    pub title: Option<String>,
    pub kind: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeletedClipsReport {
    pub deleted: Vec<String>,
    pub failed: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformEffect {
    RevealClip(PathBuf),
    OpenFolder(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipSidecarPaths {
    pub markers: PathBuf,
    pub metadata: PathBuf,
    pub pending_osu: PathBuf,
    pub poster: PathBuf,
}

impl ClipSidecarPaths {
    #[must_use]
    pub fn into_array(self) -> [PathBuf; 4] {
        [self.markers, self.metadata, self.pending_osu, self.poster]
    }
}

#[must_use]
pub fn clip_sidecar_paths(clip: &Path) -> ClipSidecarPaths {
    ClipSidecarPaths {
        markers: clip.with_extension("markers.json"),
        metadata: clip.with_extension("clipline.json"),
        pending_osu: clip.with_extension("osu-enrichment.json"),
        poster: clip.with_extension("poster.jpg"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OsuTitleEvent {
    pub unix_s: i64,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OsuEnrichmentStatus {
    Pending,
    Complete,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OsuPendingEnrichment {
    pub schema_version: u32,
    pub clip_path: String,
    pub recording_start_unix: i64,
    pub recording_end_unix: i64,
    pub clip_duration_s: f64,
    pub status: OsuEnrichmentStatus,
    pub attempts: u32,
    #[serde(default)]
    pub pagination_ceiling_reached: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub title_events: Vec<OsuTitleEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ClipMetadata {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    kind: Option<String>,
}

/// Shipping compatibility title for upload records and notifications.
///
/// Metadata reads use the same bounded, link-rejecting projection as repository
/// mutations. Missing, corrupt, untrusted, or oversized metadata falls back to the
/// clip filename exactly as the legacy adapter did.
#[must_use]
pub fn compatibility_clip_title(path: &Path) -> String {
    let file_system = StandardRepositoryFileSystem;
    let metadata = read_metadata_with(&file_system, path);
    metadata_title(&metadata).unwrap_or_else(|| {
        path.file_stem()
            .or_else(|| path.file_name())
            .map(|value| value.to_string_lossy().into_owned())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "Clipline clip".to_string())
    })
}

/// Shipping compatibility kind for upload records and notifications.
#[must_use]
pub fn compatibility_clip_kind(path: &Path) -> String {
    let file_system = StandardRepositoryFileSystem;
    metadata_kind(path, &read_metadata_with(&file_system, path))
}

#[derive(Clone)]
pub struct LocalLibraryRepository {
    canonical_root: PathBuf,
    file_system: Arc<dyn RepositoryFileSystem>,
    mutation_lease: Arc<dyn MutationLease>,
    recovery_report: ReplacementRecoveryReport,
}

impl LocalLibraryRepository {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, RepositoryError> {
        Self::with_seams(
            root,
            Arc::new(StandardRepositoryFileSystem),
            Arc::new(NoActiveMutationLease),
        )
    }

    pub fn with_seams(
        root: impl AsRef<Path>,
        file_system: Arc<dyn RepositoryFileSystem>,
        mutation_lease: Arc<dyn MutationLease>,
    ) -> Result<Self, RepositoryError> {
        let canonical_root = file_system.canonicalize(root.as_ref())?;
        let root_entry = file_system.entry(&canonical_root)?;
        if root_entry.is_regular_file() {
            return Err(RepositoryError::new(
                "Library media root is not a directory",
            ));
        }
        let recovery_report =
            file_system.recover_pending_replacements(&canonical_root, root_entry.identity())?;
        Ok(Self {
            canonical_root,
            file_system,
            mutation_lease,
            recovery_report,
        })
    }

    #[must_use]
    pub fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }

    #[must_use]
    pub const fn recovery_report(&self) -> &ReplacementRecoveryReport {
        &self.recovery_report
    }

    pub fn validate_clip_path(
        &self,
        display_path: &str,
    ) -> Result<ValidatedClipPath, RepositoryError> {
        let original_path = Path::new(display_path);
        if original_path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(RepositoryError::new(INVALID_CLIP_PATH_ERROR));
        }
        let original_entry = match self.file_system.entry(original_path) {
            Ok(entry) => entry,
            Err(error) => {
                if self.file_system.try_exists(original_path).unwrap_or(false) {
                    return Err(RepositoryError::new(INVALID_CLIP_PATH_ERROR));
                }
                return Err(RepositoryError::new(error.to_string()));
            }
        };
        let original_parent = original_path
            .parent()
            .ok_or_else(|| RepositoryError::new(INVALID_CLIP_PATH_ERROR))?;
        if !self
            .file_system
            .is_directory(original_parent)
            .unwrap_or(false)
        {
            return Err(RepositoryError::new(INVALID_CLIP_PATH_ERROR));
        }
        let canonical_path = self
            .file_system
            .canonicalize(original_path)
            .map_err(|_| RepositoryError::new(INVALID_CLIP_PATH_ERROR))?;
        let entry = self
            .file_system
            .entry(&canonical_path)
            .map_err(|_| RepositoryError::new(INVALID_CLIP_PATH_ERROR))?;
        let valid_parent = canonical_path.parent() == Some(self.canonical_root.as_path())
            || canonical_path.parent().and_then(Path::parent)
                == Some(self.canonical_root.as_path());
        let valid_extension =
            canonical_path.extension().and_then(|value| value.to_str()) == Some("mp4");
        if !entry.is_regular_file()
            || entry.identity() != original_entry.identity()
            || !valid_parent
            || !valid_extension
        {
            return Err(RepositoryError::new(INVALID_CLIP_PATH_ERROR));
        }
        let canonical_parent = canonical_path
            .parent()
            .ok_or_else(|| RepositoryError::new(INVALID_CLIP_PATH_ERROR))?;
        if !self
            .file_system
            .is_directory(canonical_parent)
            .unwrap_or(false)
        {
            return Err(RepositoryError::new(INVALID_CLIP_PATH_ERROR));
        }
        let parent_identity = self
            .file_system
            .entry(canonical_parent)
            .map_err(|_| RepositoryError::new(INVALID_CLIP_PATH_ERROR))?
            .identity();
        let comparison_identity = ClipPathIdentity::from_text(display_path)
            .ok_or_else(|| RepositoryError::new(INVALID_CLIP_PATH_ERROR))?;
        Ok(ValidatedClipPath {
            display_path: display_path.to_owned(),
            canonical_path,
            comparison_identity,
            file_identity: entry.identity(),
            parent_identity,
        })
    }

    pub fn rename_title(
        &self,
        clip: &ValidatedClipPath,
        title: &str,
    ) -> Result<RenamedClipInfo, RepositoryError> {
        let title = normalized_clip_title(title).map_err(RepositoryError::new)?;
        let initial_identity = self.revalidate(clip)?;
        let _permit = self
            .mutation_lease
            .acquire(clip.canonical_path(), initial_identity)
            .map_err(RepositoryError::new)?;
        let _fence = self.acquire_fence(clip)?;
        let metadata_path = clip_sidecar_paths(clip.canonical_path()).metadata;
        let metadata_entry = self.validate_optional_sidecar(&metadata_path)?;
        let metadata_target = metadata_entry.map_or(AtomicWriteTarget::Absent, |entry| {
            AtomicWriteTarget::Existing(entry.identity())
        });
        let mut metadata = self.read_metadata_entry(&metadata_path, metadata_entry);
        let kind = metadata_kind(clip.canonical_path(), &metadata);
        metadata.title = Some(title.clone());
        metadata.kind = Some(kind.clone());
        let bytes = serde_json::to_vec_pretty(&metadata)
            .map_err(|error| RepositoryError::new(format!("serialize clip metadata: {error}")))?;

        self.write_atomic(
            &metadata_path,
            &bytes,
            metadata_target,
            "write clip metadata",
        )?;

        Ok(RenamedClipInfo {
            old_path: clip.display_path.clone(),
            path: clip.display_path.clone(),
            name: clip
                .canonical_path
                .file_name()
                .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
            title: Some(title),
            kind,
        })
    }

    pub fn rename_file(
        &self,
        clip: &ValidatedClipPath,
        requested_name: &str,
    ) -> Result<RenamedClipInfo, RepositoryError> {
        let target_name =
            normalized_clip_file_name(requested_name).map_err(RepositoryError::new)?;
        let source = clip.canonical_path();
        let parent = source
            .parent()
            .ok_or_else(|| RepositoryError::new("clip has no containing folder"))?;
        let target = parent.join(&target_name);
        let source_sidecars = clip_sidecar_paths(source);
        let target_sidecars = clip_sidecar_paths(&target);
        let renamed_display_path = display_renamed_path(clip.display_path(), &target_name, parent);

        // Acquire before any sidecar preflight or staging. The permit is retained
        // until the complete transaction (including rollback) leaves this scope.
        let initial_identity = self.revalidate(clip)?;
        let _permit = self
            .mutation_lease
            .acquire(source, initial_identity)
            .map_err(RepositoryError::new)?;
        let mut primary_fence = self.acquire_fence(clip)?;
        let source_markers_entry = self.validate_optional_sidecar(&source_sidecars.markers)?;
        let source_metadata_entry = self.validate_optional_sidecar(&source_sidecars.metadata)?;
        let source_pending_entry = self.validate_optional_sidecar(&source_sidecars.pending_osu)?;
        let source_poster_entry = self.validate_optional_sidecar(&source_sidecars.poster)?;
        let metadata = self.read_metadata_entry(&source_sidecars.metadata, source_metadata_entry);
        let title = metadata_title(&metadata);
        let kind = metadata_kind(source, &metadata);
        let source_metadata_identity = source_metadata_entry.map(FileSystemEntry::identity);

        self.reject_collision(source, &target, CLIP_COLLISION_ERROR, true)?;
        self.reject_collision(
            &source_sidecars.markers,
            &target_sidecars.markers,
            MARKER_COLLISION_ERROR,
            source_markers_entry.is_some(),
        )?;
        self.reject_collision(
            &source_sidecars.metadata,
            &target_sidecars.metadata,
            METADATA_COLLISION_ERROR,
            source_metadata_entry.is_some(),
        )?;
        self.reject_collision(
            &source_sidecars.pending_osu,
            &target_sidecars.pending_osu,
            PENDING_OSU_COLLISION_ERROR,
            source_pending_entry.is_some(),
        )?;
        self.reject_collision(
            &source_sidecars.poster,
            &target_sidecars.poster,
            POSTER_COLLISION_ERROR,
            source_poster_entry.is_some(),
        )?;

        if source == target {
            let bytes = rewritten_metadata_bytes(metadata, title.clone(), kind.clone())?;
            self.write_atomic(
                &source_sidecars.metadata,
                &bytes,
                source_metadata_identity
                    .map_or(AtomicWriteTarget::Absent, AtomicWriteTarget::Existing),
                "write clip metadata",
            )?;
            return Ok(RenamedClipInfo {
                old_path: clip.display_path.clone(),
                path: renamed_display_path,
                name: target_name,
                title,
                kind,
            });
        }

        let pending_stage = source_pending_entry
            .map(|entry| {
                self.prepare_pending_osu(
                    &source_sidecars.pending_osu,
                    entry,
                    &target,
                    &renamed_display_path,
                )
            })
            .transpose()?;

        let pending_backup = pending_stage.as_ref().map(|stage| stage.backup.clone());
        let mut journal = RenameJournal::default();
        let forward = (|| {
            self.move_primary(
                &mut *primary_fence,
                source,
                &target,
                "rename clip",
                &mut journal,
            )?;
            self.move_if_present(
                &source_sidecars.markers,
                &target_sidecars.markers,
                source_markers_entry,
                "rename clip markers",
                &mut journal,
            )?;
            self.move_if_present(
                &source_sidecars.metadata,
                &target_sidecars.metadata,
                source_metadata_entry,
                "rename clip metadata",
                &mut journal,
            )?;
            if let Some(stage) = &pending_stage {
                self.move_required(
                    &stage.source,
                    &stage.backup,
                    stage.source_identity,
                    "stage old osu! enrichment sidecar",
                    &mut journal,
                )?;
                self.move_required(
                    &stage.staged,
                    &stage.target,
                    stage.staged_identity,
                    "install renamed osu! enrichment sidecar",
                    &mut journal,
                )?;
            }
            self.move_if_present(
                &source_sidecars.poster,
                &target_sidecars.poster,
                source_poster_entry,
                "rename clip poster",
                &mut journal,
            )?;

            // Delete the rollback-only pending backup before the final metadata
            // commit. Its bounded original bytes can recreate it if that final
            // commit fails; after the metadata commit there are no fallible steps.
            if let Some(backup) = &pending_backup {
                let identity = pending_stage
                    .as_ref()
                    .expect("backup belongs to pending stage")
                    .source_identity;
                self.file_system
                    .remove_file_if_identity(backup, identity)
                    .map_err(|error| {
                        RepositoryError::new(format!(
                            "remove old osu! enrichment sidecar backup: {error}"
                        ))
                    })?;
                journal.pending_backup_removed = true;
            }

            let metadata_bytes = rewritten_metadata_bytes(metadata, title.clone(), kind.clone())?;
            self.write_atomic(
                &target_sidecars.metadata,
                &metadata_bytes,
                source_metadata_identity
                    .map_or(AtomicWriteTarget::Absent, AtomicWriteTarget::Existing),
                "write clip metadata",
            )?;
            Ok::<(), RepositoryError>(())
        })();

        if let Err(error) = forward {
            let rollback_failures =
                self.rollback_rename(&mut *primary_fence, &mut journal, pending_stage.as_ref());
            return Err(RepositoryError::with_rollback(
                error.primary,
                [error.rollback_failures, rollback_failures].concat(),
            ));
        }

        Ok(RenamedClipInfo {
            old_path: clip.display_path.clone(),
            path: renamed_display_path,
            name: target_name,
            title,
            kind,
        })
    }

    pub fn delete(&self, clip: &ValidatedClipPath) -> Result<(), RepositoryError> {
        let current_identity = self.revalidate(clip)?;
        let _permit = self
            .mutation_lease
            .acquire(clip.canonical_path(), current_identity)
            .map_err(RepositoryError::new)?;
        self.delete_under_existing_permit(clip)
    }

    /// Delete a previously validated clip while the caller retains an existing
    /// exclusive mutation permit for the exact source identity.
    ///
    /// This is crate-private so the upload ownership transition is the only
    /// path that can bypass acquiring a second, self-conflicting permit.
    pub(crate) fn delete_under_existing_permit(
        &self,
        clip: &ValidatedClipPath,
    ) -> Result<(), RepositoryError> {
        self.revalidate(clip)?;
        let mut fence = self.acquire_fence(clip)?;
        let sidecars = clip_sidecar_paths(clip.canonical_path());
        let sidecars = sidecars.into_array();
        let sidecar_entries = sidecars
            .iter()
            .map(|sidecar| self.validate_optional_sidecar(sidecar))
            .collect::<Result<Vec<_>, _>>()?;
        fence
            .delete()
            .map_err(|error| RepositoryError::new(error.to_string()))?;
        for (sidecar, entry) in sidecars.iter().zip(sidecar_entries) {
            if let Some(entry) = entry {
                let _ = self
                    .file_system
                    .remove_file_if_identity(sidecar, entry.identity());
            }
        }
        Ok(())
    }

    pub fn delete_many(&self, paths: &[String]) -> Result<DeletedClipsReport, RepositoryError> {
        if paths.len() > MAX_MUTATION_ITEMS {
            return Err(RepositoryError::new(format!(
                "delete.paths contains {} entries; maximum is {MAX_MUTATION_ITEMS}",
                paths.len()
            )));
        }
        let path_bytes = paths.iter().try_fold(0_usize, |total, path| {
            total.checked_add(path.len()).ok_or_else(|| {
                RepositoryError::new(format!(
                    "delete.path_bytes contains {} bytes or entries; maximum is {MAX_MUTATION_PATH_BYTES}",
                    usize::MAX
                ))
            })
        })?;
        if path_bytes > MAX_MUTATION_PATH_BYTES {
            return Err(RepositoryError::new(format!(
                "delete.path_bytes contains {path_bytes} bytes or entries; maximum is {MAX_MUTATION_PATH_BYTES}"
            )));
        }
        let mut report = DeletedClipsReport::default();
        let mut validated = Vec::with_capacity(paths.len());
        for path in paths {
            match self.validate_clip_path(path) {
                Ok(clip) => validated.push((path.clone(), clip)),
                Err(error) => report.failed.push((path.clone(), error.to_string())),
            }
        }
        for (path, clip) in validated {
            match self.delete(&clip) {
                Ok(()) => report.deleted.push(path),
                Err(error) => report.failed.push((path, error.to_string())),
            }
        }
        Ok(report)
    }

    pub fn reveal_effect(
        &self,
        clip: &ValidatedClipPath,
    ) -> Result<PlatformEffect, RepositoryError> {
        self.revalidate(clip)?;
        Ok(PlatformEffect::RevealClip(clip.canonical_path.clone()))
    }

    #[must_use]
    pub fn open_folder_effect(&self) -> PlatformEffect {
        PlatformEffect::OpenFolder(self.canonical_root.clone())
    }

    fn read_metadata_entry(
        &self,
        metadata_path: &Path,
        entry: Option<FileSystemEntry>,
    ) -> ClipMetadata {
        read_metadata_entry_with(self.file_system.as_ref(), metadata_path, entry)
    }

    fn revalidate(&self, clip: &ValidatedClipPath) -> Result<FileIdentity, RepositoryError> {
        let canonical_now = self
            .file_system
            .canonicalize(Path::new(clip.display_path()))
            .map_err(|_| RepositoryError::new(CHANGED_CLIP_ERROR))?;
        if canonical_now != clip.canonical_path {
            return Err(RepositoryError::new(CHANGED_CLIP_ERROR));
        }
        let entry = self
            .file_system
            .entry(&canonical_now)
            .map_err(|_| RepositoryError::new(CHANGED_CLIP_ERROR))?;
        if !entry.is_regular_file() || entry.identity() != clip.file_identity {
            return Err(RepositoryError::new(CHANGED_CLIP_ERROR));
        }
        Ok(entry.identity())
    }

    fn acquire_fence(
        &self,
        clip: &ValidatedClipPath,
    ) -> Result<Box<dyn RepositoryMutationFence>, RepositoryError> {
        self.file_system
            .acquire_mutation_fence(
                clip.canonical_path(),
                clip.file_identity(),
                clip.parent_identity(),
            )
            .map_err(|_| RepositoryError::new(CHANGED_CLIP_ERROR))
    }

    fn reject_collision(
        &self,
        source: &Path,
        target: &Path,
        message: &'static str,
        source_exists: bool,
    ) -> Result<(), RepositoryError> {
        if !self.file_system.try_exists(target)? {
            return Ok(());
        }
        if source_exists && self.same_case_alias(source, target) {
            return Ok(());
        }
        Err(RepositoryError::new(message))
    }

    fn validate_optional_sidecar(
        &self,
        path: &Path,
    ) -> Result<Option<FileSystemEntry>, RepositoryError> {
        if !self.file_system.try_exists(path)? {
            return Ok(None);
        }
        let entry = self.file_system.entry(path).map_err(|error| {
            RepositoryError::new(format!(
                "refusing to mutate untrusted clip sidecar {path:?}: {error}"
            ))
        })?;
        if !entry.is_regular_file() {
            return Err(RepositoryError::new(format!(
                "refusing to mutate untrusted clip sidecar {path:?}: not a regular file"
            )));
        }
        Ok(Some(entry))
    }

    fn same_case_alias(&self, first: &Path, second: &Path) -> bool {
        if first == second {
            return true;
        }
        if ClipPathIdentity::from_path(first) != ClipPathIdentity::from_path(second) {
            return false;
        }
        match (
            self.file_system.entry(first),
            self.file_system.entry(second),
        ) {
            (Ok(first), Ok(second)) => first.identity() == second.identity(),
            _ => false,
        }
    }

    fn prepare_pending_osu(
        &self,
        source: &Path,
        source_entry: FileSystemEntry,
        target_clip: &Path,
        rewritten_clip_path: &str,
    ) -> Result<PendingOsuStage, RepositoryError> {
        if source_entry.length() > MAX_PENDING_OSU_BYTES {
            return Err(RepositoryError::new(format!(
                "read osu! enrichment sidecar {source:?}: sidecar exceeds {MAX_PENDING_OSU_BYTES} bytes"
            )));
        }
        let bytes = self
            .file_system
            .read_bounded_if_identity(source, source_entry.identity(), MAX_PENDING_OSU_BYTES)
            .map_err(|error| {
                RepositoryError::new(format!("read osu! enrichment sidecar {source:?}: {error}"))
            })?;
        if bytes.len() as u64 > MAX_PENDING_OSU_BYTES {
            return Err(RepositoryError::new(format!(
                "read osu! enrichment sidecar {source:?}: sidecar exceeds {MAX_PENDING_OSU_BYTES} bytes"
            )));
        }
        let mut record: OsuPendingEnrichment = serde_json::from_slice(&bytes).map_err(|error| {
            RepositoryError::new(format!("parse osu! enrichment sidecar {source:?}: {error}"))
        })?;
        record.clip_path = rewritten_clip_path.to_owned();
        let target = clip_sidecar_paths(target_clip).pending_osu;
        let staged = target.with_extension("clipline-rename-tmp");
        let backup = source.with_extension("clipline-rename-backup");
        if self.file_system.try_exists(&staged)? {
            return Err(RepositoryError::new(format!(
                "staged osu! enrichment path already exists: {staged:?}"
            )));
        }
        if self.file_system.try_exists(&backup)? {
            return Err(RepositoryError::new(format!(
                "backup osu! enrichment path already exists: {backup:?}"
            )));
        }
        let rewritten = serde_json::to_vec_pretty(&record).map_err(|error| {
            RepositoryError::new(format!("serialize osu! enrichment sidecar: {error}"))
        })?;
        if rewritten.len() as u64 > MAX_PENDING_OSU_BYTES {
            return Err(RepositoryError::new(format!(
                "serialize osu! enrichment sidecar: sidecar exceeds {MAX_PENDING_OSU_BYTES} bytes"
            )));
        }
        let staged_identity = match self.file_system.create_new_synced(&staged, &rewritten) {
            Ok(identity) => identity,
            Err(error) => {
                let mut rollback = Vec::new();
                if let Some(identity) = error.created_identity() {
                    if let Err(cleanup) =
                        self.file_system.remove_file_if_identity(&staged, identity)
                    {
                        rollback.push(format!("remove partial osu! enrichment stage: {cleanup}"));
                    }
                }
                return Err(RepositoryError::with_rollback(
                    format!("stage osu! enrichment sidecar {staged:?}: {error}"),
                    rollback,
                ));
            }
        };
        Ok(PendingOsuStage {
            source: source.to_path_buf(),
            target,
            staged,
            backup,
            original_bytes: bytes,
            source_identity: source_entry.identity(),
            staged_identity,
        })
    }

    fn move_if_present(
        &self,
        source: &Path,
        target: &Path,
        source_entry: Option<FileSystemEntry>,
        context: &str,
        journal: &mut RenameJournal,
    ) -> Result<(), RepositoryError> {
        let Some(source_entry) = source_entry else {
            return Ok(());
        };
        if source == target {
            return Ok(());
        }
        self.move_required(source, target, source_entry.identity(), context, journal)
    }

    fn move_required(
        &self,
        source: &Path,
        target: &Path,
        source_identity: FileIdentity,
        context: &str,
        journal: &mut RenameJournal,
    ) -> Result<(), RepositoryError> {
        if let Err(error) =
            self.file_system
                .rename_noreplace_if_identity(source, target, source_identity)
        {
            if error.may_have_moved() {
                journal.moves.push(CompletedMove {
                    source: source.to_path_buf(),
                    target: target.to_path_buf(),
                    primary: false,
                    identity: Some(source_identity),
                });
            }
            return Err(RepositoryError::new(format!(
                "{context} from {source:?} to {target:?}: {error}"
            )));
        }
        journal.moves.push(CompletedMove {
            source: source.to_path_buf(),
            target: target.to_path_buf(),
            primary: false,
            identity: Some(source_identity),
        });
        Ok(())
    }

    fn move_primary(
        &self,
        fence: &mut dyn RepositoryMutationFence,
        source: &Path,
        target: &Path,
        context: &str,
        journal: &mut RenameJournal,
    ) -> Result<(), RepositoryError> {
        if let Err(error) = fence.rename_noreplace(target) {
            if error.may_have_moved() {
                journal.moves.push(CompletedMove {
                    source: source.to_path_buf(),
                    target: target.to_path_buf(),
                    primary: true,
                    identity: None,
                });
            }
            return Err(RepositoryError::new(format!("{context}: {error}")));
        }
        journal.moves.push(CompletedMove {
            source: source.to_path_buf(),
            target: target.to_path_buf(),
            primary: true,
            identity: None,
        });
        Ok(())
    }

    fn rollback_rename(
        &self,
        primary_fence: &mut dyn RepositoryMutationFence,
        journal: &mut RenameJournal,
        pending_stage: Option<&PendingOsuStage>,
    ) -> Vec<String> {
        let mut failures = Vec::new();
        for completed in journal.moves.iter().rev() {
            let restores_removed_pending = journal.pending_backup_removed
                && pending_stage.is_some_and(|stage| {
                    completed.source == stage.source && completed.target == stage.backup
                });
            let result: Result<(), String> = if restores_removed_pending {
                let stage = pending_stage.expect("matched pending stage");
                self.write_atomic(
                    &stage.source,
                    &stage.original_bytes,
                    AtomicWriteTarget::Absent,
                    "recreate original osu! enrichment sidecar",
                )
                .map_err(|error| error.to_string())
            } else if completed.primary {
                primary_fence
                    .rename_noreplace(&completed.source)
                    .map_err(|error| error.to_string())
            } else {
                self.file_system
                    .rename_noreplace_if_identity(
                        &completed.target,
                        &completed.source,
                        completed.identity.expect("non-primary move has identity"),
                    )
                    .map_err(|error| error.to_string())
            };
            if let Err(error) = result {
                failures.push(format!(
                    "restore {:?} to {:?}: {error}",
                    completed.target, completed.source
                ));
            }
        }
        if let Some(stage) = pending_stage {
            if self.file_system.try_exists(&stage.staged).unwrap_or(false) {
                if let Err(error) = self
                    .file_system
                    .remove_file_if_identity(&stage.staged, stage.staged_identity)
                {
                    failures.push(format!("remove staged osu! enrichment sidecar: {error}"));
                }
            }
        }
        failures
    }

    fn write_atomic(
        &self,
        target: &Path,
        bytes: &[u8],
        target_state: AtomicWriteTarget,
        context: &str,
    ) -> Result<(), RepositoryError> {
        let temp = unique_sibling_temp(target);
        let temp_identity = match self.file_system.create_new_synced(&temp, bytes) {
            Ok(identity) => identity,
            Err(error) => {
                let mut rollback = Vec::new();
                if let Some(identity) = error.created_identity() {
                    if let Err(cleanup) = self.file_system.remove_file_if_identity(&temp, identity)
                    {
                        rollback.push(format!("remove partial temporary metadata file: {cleanup}"));
                    }
                }
                return Err(RepositoryError::with_rollback(
                    format!("{context}: {error}"),
                    rollback,
                ));
            }
        };
        let result =
            match target_state {
                AtomicWriteTarget::Absent => {
                    self.file_system
                        .rename_noreplace_if_identity(&temp, target, temp_identity)
                }
                AtomicWriteTarget::Existing(target_identity) => self
                    .file_system
                    .replace_if_identities(&temp, temp_identity, target, target_identity),
            };
        if let Err(error) = result {
            let mut rollback = Vec::new();
            if let Err(cleanup) = self
                .file_system
                .remove_file_if_identity(&temp, temp_identity)
            {
                if cleanup.kind() != io::ErrorKind::NotFound {
                    rollback.push(format!("remove temporary metadata file: {cleanup}"));
                }
            }
            return Err(RepositoryError::with_rollback(
                format!("{context}: {error}"),
                rollback,
            ));
        }
        Ok(())
    }
}

fn read_metadata_with(file_system: &dyn RepositoryFileSystem, clip: &Path) -> ClipMetadata {
    let path = clip_sidecar_paths(clip).metadata;
    let entry = file_system.entry(&path).ok();
    read_metadata_entry_with(file_system, &path, entry)
}

fn read_metadata_entry_with(
    file_system: &dyn RepositoryFileSystem,
    path: &Path,
    entry: Option<FileSystemEntry>,
) -> ClipMetadata {
    let Some(entry) =
        entry.filter(|entry| entry.is_regular_file() && entry.length() <= MAX_CLIP_METADATA_BYTES)
    else {
        return ClipMetadata::default();
    };
    file_system
        .read_bounded_if_identity(path, entry.identity(), MAX_CLIP_METADATA_BYTES)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

#[derive(Debug)]
struct PendingOsuStage {
    source: PathBuf,
    target: PathBuf,
    staged: PathBuf,
    backup: PathBuf,
    original_bytes: Vec<u8>,
    source_identity: FileIdentity,
    staged_identity: FileIdentity,
}

#[derive(Debug, Clone, Copy)]
enum AtomicWriteTarget {
    Absent,
    Existing(FileIdentity),
}

#[derive(Debug, Default)]
struct RenameJournal {
    moves: Vec<CompletedMove>,
    pending_backup_removed: bool,
}

#[derive(Debug)]
struct CompletedMove {
    source: PathBuf,
    target: PathBuf,
    primary: bool,
    identity: Option<FileIdentity>,
}

fn metadata_title(metadata: &ClipMetadata) -> Option<String> {
    metadata
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_owned)
}

fn metadata_kind(path: &Path, metadata: &ClipMetadata) -> String {
    metadata
        .kind
        .as_deref()
        .map(str::trim)
        .filter(|kind| matches!(*kind, "replay" | "session" | "trim"))
        .map_or_else(
            || inferred_clip_kind_for_path(path).to_owned(),
            str::to_owned,
        )
}

fn rewritten_metadata_bytes(
    mut metadata: ClipMetadata,
    title: Option<String>,
    kind: String,
) -> Result<Vec<u8>, RepositoryError> {
    metadata.title = title;
    metadata.kind = Some(kind);
    serde_json::to_vec_pretty(&metadata)
        .map_err(|error| RepositoryError::new(format!("serialize clip metadata: {error}")))
}

fn display_renamed_path(old_path: &str, name: &str, fallback_parent: &Path) -> String {
    Path::new(old_path)
        .parent()
        .map(|parent| parent.join(name))
        .unwrap_or_else(|| fallback_parent.join(name))
        .display()
        .to_string()
}

fn unique_sibling_temp(target: &Path) -> PathBuf {
    let suffix = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = target
        .file_name()
        .map_or_else(|| "metadata".into(), |name| name.to_string_lossy());
    target.with_file_name(format!(
        ".{name}.clipline-write-{}-{suffix}.tmp",
        std::process::id()
    ))
}

#[cfg(windows)]
fn has_windows_reparse_attribute(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
const fn has_windows_reparse_attribute(_metadata: &std::fs::Metadata) -> bool {
    false
}
