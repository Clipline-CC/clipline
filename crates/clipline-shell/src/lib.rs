//! Framework-neutral desktop shell contract for Clipline.

pub mod activation;
mod channel;
mod contract;
pub mod diagnostics;
pub mod hotkey;
mod shutdown;

#[cfg(windows)]
pub mod windows;

pub use channel::{
    shell_command_channel, shell_command_channel_starting_at, SequencedShellCommand,
    ShellCommandPublishOutcome, ShellCommandReceiveError, ShellCommandReceiver,
    ShellCommandSendError, ShellCommandSender, SHELL_COMMAND_CAPACITY,
};
pub use contract::{
    LaunchMode, ProcessIdentity, ShellCommand, ShellCounterError, ShellGeneration, ShellLaunch,
    ShellLaunchError, ShellSequence, WindowEffect, WindowEvent, WindowMode, WindowPolicy,
    MAX_LAUNCH_ARGUMENTS, MAX_LAUNCH_ARGUMENT_BYTES, MAX_LAUNCH_TOTAL_BYTES,
};
pub use shutdown::{
    ShutdownAcknowledgement, ShutdownCoordinator, ShutdownEffect, ShutdownError, ShutdownGate,
    ShutdownLease, ShutdownOwnershipError, ShutdownReason, ShutdownStage, MAX_SHUTDOWN_TIMEOUT_MS,
};

/// Stable identity of an existing filesystem object for mutation fencing.
///
/// Paths remain display/reconciliation values. Callers compare this identity
/// immediately before mutation so a replaced path cannot inherit prior authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct FileIdentity {
    device: u64,
    file: u64,
}

const REPLACEMENT_JOURNAL_SCHEMA: u32 = 1;
const REPLACEMENT_JOURNAL_PREFIX: &str = ".clipline-replace-journal-";
const REPLACEMENT_JOURNAL_SUFFIX: &str = ".json";
const REPLACEMENT_BACKUP_PREFIX: &str = ".clipline-replace-old-";
const MAX_REPLACEMENT_JOURNAL_BYTES: u64 = 16 * 1024;
pub const MAX_PENDING_REPLACEMENT_JOURNALS: usize = 128;
pub const MAX_REPLACEMENT_RECOVERY_ENTRIES: usize = 100_000;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReplacementJournal {
    schema_version: u32,
    parent_identity: FileIdentity,
    target_name: String,
    backup_name: String,
    replacement_name: String,
    target_identity: FileIdentity,
    replacement_identity: FileIdentity,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplacementRecoveryReport {
    pub rolled_back: Vec<std::path::PathBuf>,
    pub completed: Vec<std::path::PathBuf>,
    pub stale: Vec<std::path::PathBuf>,
    pub unresolved: Vec<(std::path::PathBuf, String)>,
    pub entries_examined: usize,
    pub journals_examined: usize,
}

impl FileIdentity {
    #[cfg(windows)]
    pub(crate) const fn from_components(device: u64, file: u64) -> Self {
        Self { device, file }
    }

    #[must_use]
    pub const fn device(self) -> u64 {
        self.device
    }

    #[must_use]
    pub const fn file(self) -> u64 {
        self.file
    }
}

/// Observable location after a fenced mutation fails.
///
/// `TargetOrUnknown` is deliberately conservative: the directory entry moved,
/// but a later identity check or rollback could not prove where the selected
/// file finally resides. Callers must journal the attempted target and use the
/// same identity-fenced reverse operation before reporting rollback complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileMutationLocation {
    Source,
    TargetOrUnknown(std::path::PathBuf),
}

/// Mutation failure carrying whether an atomic move may already have committed.
#[derive(Debug)]
pub struct FileMutationError {
    error: std::io::Error,
    location: FileMutationLocation,
}

impl FileMutationError {
    #[must_use]
    pub fn unchanged(error: std::io::Error) -> Self {
        Self {
            error,
            location: FileMutationLocation::Source,
        }
    }

    #[must_use]
    pub fn target_or_unknown(error: std::io::Error, target: &std::path::Path) -> Self {
        Self {
            error,
            location: FileMutationLocation::TargetOrUnknown(target.to_path_buf()),
        }
    }

    #[must_use]
    pub const fn location(&self) -> &FileMutationLocation {
        &self.location
    }

    #[must_use]
    pub const fn may_have_moved(&self) -> bool {
        matches!(self.location, FileMutationLocation::TargetOrUnknown(_))
    }

    #[must_use]
    pub fn kind(&self) -> std::io::ErrorKind {
        self.error.kind()
    }
}

impl std::fmt::Display for FileMutationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.location {
            FileMutationLocation::Source => self.error.fmt(formatter),
            FileMutationLocation::TargetOrUnknown(path) => write!(
                formatter,
                "{}; selected file may be recoverable at {path:?}",
                self.error
            ),
        }
    }
}

impl std::error::Error for FileMutationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// Identify an existing filesystem object without exposing platform APIs.
pub fn file_identity(path: &std::path::Path) -> std::io::Result<FileIdentity> {
    #[cfg(windows)]
    let (device, file) = windows::filesystem::file_identity_components(path)?;

    #[cfg(unix)]
    let (device, file) = {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(path)?;
        (metadata.dev(), metadata.ino())
    };

    #[cfg(not(any(windows, unix)))]
    return Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "stable filesystem identity is unavailable on this platform",
    ));

    Ok(FileIdentity { device, file })
}

/// Identify a file through the already-open handle used for its I/O.
pub fn opened_file_identity(file: &std::fs::File) -> std::io::Result<FileIdentity> {
    #[cfg(windows)]
    let (device, file) = windows::filesystem::opened_file_identity_components(file)?;

    #[cfg(unix)]
    let (device, file) = {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        (metadata.dev(), metadata.ino())
    };

    #[cfg(not(any(windows, unix)))]
    return Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "stable filesystem identity is unavailable on this platform",
    ));

    Ok(FileIdentity { device, file })
}

/// Open one regular file without following its final link/reparse component.
pub fn open_regular_file_nofollow(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    #[cfg(windows)]
    {
        windows::filesystem::open_regular_file_nofollow(path)
    }

    #[cfg(unix)]
    {
        use rustix::fs::{Mode, OFlags};

        let owned = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?;
        let file = std::fs::File::from(owned);
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "refusing to open a filesystem link or non-file",
            ));
        }
        Ok(file)
    }

    #[cfg(not(any(windows, unix)))]
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no-follow regular file opens are unavailable on this platform",
        ))
    }
}

/// Update one regular file's modification time through a no-follow handle only
/// while it still has the exact identity authorized by the caller.
pub fn set_regular_file_modified_if_identity(
    path: &std::path::Path,
    expected: FileIdentity,
    modified: std::time::SystemTime,
) -> std::io::Result<()> {
    #[cfg(windows)]
    let file = windows::filesystem::open_regular_file_nofollow_for_metadata_write(path)?;

    #[cfg(not(windows))]
    let file = open_regular_file_nofollow(path)?;

    if opened_file_identity(&file)? != expected {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "regular file identity changed before metadata update",
        ));
    }
    file.set_modified(modified)
}

/// Open one directory without following its final link/reparse component.
pub fn open_directory_nofollow(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    #[cfg(windows)]
    {
        windows::filesystem::open_directory_nofollow(path)
    }

    #[cfg(unix)]
    {
        use rustix::fs::{Mode, OFlags};

        let owned = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::DIRECTORY,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?;
        let file = std::fs::File::from(owned);
        if !file.metadata()?.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "refusing to open a filesystem link or non-directory",
            ));
        }
        Ok(file)
    }

    #[cfg(not(any(windows, unix)))]
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no-follow directory opens are unavailable on this platform",
        ))
    }
}

/// Retained authority for creating, inspecting, and mutating regular-file children of one
/// selected directory.
///
/// Child operations accept exactly one normal path component and remain bound to the opened
/// directory identity. Unix uses descriptor-relative filesystem operations. Windows retains a
/// directory handle opened without `FILE_SHARE_DELETE`, which pins the selected directory while
/// the existing path-based child fallback runs. Replacing an ancestor component is outside the
/// current Windows wrapper's authority ceiling; supporting that requires NT handle-relative open.
#[derive(Debug)]
pub struct DirectoryAuthority {
    display_path: std::path::PathBuf,
    directory: std::fs::File,
    identity: FileIdentity,
}

impl DirectoryAuthority {
    /// Open and retain one no-follow directory authority.
    pub fn open(path: &std::path::Path) -> std::io::Result<Self> {
        let directory = open_directory_nofollow(path)?;
        let identity = opened_file_identity(&directory)?;
        Ok(Self {
            display_path: path.to_path_buf(),
            directory,
            identity,
        })
    }

    /// Stable identity of the selected directory.
    #[must_use]
    pub const fn identity(&self) -> FileIdentity {
        self.identity
    }

    /// Original display path used to open the authority.
    #[must_use]
    pub fn display_path(&self) -> &std::path::Path {
        &self.display_path
    }

    /// Exclusively create one regular-file child under the selected directory.
    pub fn create_new_regular_file(
        &self,
        name: &std::ffi::OsStr,
    ) -> std::io::Result<std::fs::File> {
        validate_child_name(name)?;
        require_directory_authority(self)?;
        create_new_regular_file_in_directory(&self.directory, &self.display_path, name)
    }

    /// Return the stable identity of one existing regular-file child, or `None` when absent.
    pub fn regular_file_identity(
        &self,
        name: &std::ffi::OsStr,
    ) -> std::io::Result<Option<FileIdentity>> {
        validate_child_name(name)?;
        require_directory_authority(self)?;
        regular_file_identity_in_directory_if_present(&self.directory, &self.display_path, name)
    }

    /// Move one selected child to an unused sibling name under this authority.
    pub fn rename_file_noreplace_if_identity(
        &self,
        from: &std::ffi::OsStr,
        to: &std::ffi::OsStr,
        source_identity: FileIdentity,
    ) -> Result<(), FileMutationError> {
        validate_child_name(from).map_err(FileMutationError::unchanged)?;
        validate_child_name(to).map_err(FileMutationError::unchanged)?;
        if from == to {
            return Err(FileMutationError::unchanged(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "source and target child names must differ",
            )));
        }
        require_directory_authority(self).map_err(FileMutationError::unchanged)?;
        rename_file_in_directory_noreplace_if_identity(
            &self.directory,
            &self.display_path,
            from,
            to,
            source_identity,
            self.identity,
        )
    }

    /// Remove one selected regular-file child under this authority.
    pub fn remove_file_if_identity(
        &self,
        name: &std::ffi::OsStr,
        source_identity: FileIdentity,
    ) -> Result<(), FileMutationError> {
        validate_child_name(name).map_err(FileMutationError::unchanged)?;
        require_directory_authority(self).map_err(FileMutationError::unchanged)?;
        remove_file_in_directory_if_identity(
            &self.directory,
            &self.display_path,
            name,
            source_identity,
            self.identity,
        )
    }

    /// Publish one synchronized selected child over another selected child.
    pub fn replace_file_if_identities(
        &self,
        replacement: &std::ffi::OsStr,
        replacement_identity: FileIdentity,
        target: &std::ffi::OsStr,
        target_identity: FileIdentity,
    ) -> Result<(), FileMutationError> {
        validate_child_name(replacement).map_err(FileMutationError::unchanged)?;
        validate_child_name(target).map_err(FileMutationError::unchanged)?;
        if replacement == target {
            return Err(FileMutationError::unchanged(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "replacement and target child names must differ",
            )));
        }
        require_directory_authority(self).map_err(FileMutationError::unchanged)?;
        replace_file_if_identities_with_authority(
            self,
            replacement,
            replacement_identity,
            target,
            target_identity,
        )
    }
}

fn validate_child_name(name: &std::ffi::OsStr) -> std::io::Result<()> {
    let path = std::path::Path::new(name);
    if path.file_name() != Some(name)
        || !matches!(
            path.components().next(),
            Some(std::path::Component::Normal(_))
        )
        || path.components().nth(1).is_some()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "directory child name must be one normal path component",
        ));
    }
    Ok(())
}

fn require_directory_authority(authority: &DirectoryAuthority) -> std::io::Result<()> {
    if opened_file_identity(&authority.directory)? != authority.identity {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "selected directory authority changed",
        ));
    }
    Ok(())
}

/// Atomically replace `to` with the already-synchronized sibling file `from`.
/// Platform-specific replacement stays behind this safe shell boundary.
pub fn replace_file(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        windows::filesystem::replace_file(from, to)
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(from, to)
    }
}

/// Retained authority for one existing file and its containing directory.
///
/// The fence binds mutations to the identities selected by the caller. On
/// Windows the implementation keeps both objects open without delete sharing
/// and renames/deletes the file by handle, closing the path-replacement window.
#[cfg(windows)]
pub use windows::filesystem::FileMutationFence;

#[cfg(not(windows))]
#[derive(Debug)]
pub struct FileMutationFence {
    _file: std::fs::File,
    current_path: std::path::PathBuf,
    source_identity: FileIdentity,
    parent: std::fs::File,
    parent_identity: FileIdentity,
}

#[cfg(not(windows))]
impl FileMutationFence {
    pub fn acquire(
        path: &std::path::Path,
        source_identity: FileIdentity,
        parent_identity: FileIdentity,
    ) -> std::io::Result<Self> {
        let parent_path = path.parent().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "file has no parent")
        })?;
        let source_metadata = std::fs::symlink_metadata(path)?;
        let parent_metadata = std::fs::symlink_metadata(parent_path)?;
        if source_metadata.file_type().is_symlink()
            || !source_metadata.is_file()
            || parent_metadata.file_type().is_symlink()
            || !parent_metadata.is_dir()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "refusing to fence a filesystem link or non-file source",
            ));
        }
        let file = open_regular_file_nofollow(path)?;
        let parent = open_directory_nofollow(parent_path)?;
        if opened_file_identity(&file)? != source_identity
            || opened_file_identity(&parent)? != parent_identity
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "filesystem object changed while acquiring its mutation fence",
            ));
        }
        Ok(Self {
            _file: file,
            current_path: path.to_path_buf(),
            source_identity,
            parent,
            parent_identity,
        })
    }

    pub fn rename_noreplace(&mut self, target: &std::path::Path) -> Result<(), FileMutationError> {
        if target.parent() != self.current_path.parent() {
            return Err(FileMutationError::unchanged(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "mutation target must be a sibling",
            )));
        }
        if opened_file_identity(&self._file).map_err(FileMutationError::unchanged)?
            != self.source_identity
            || opened_file_identity(&self.parent).map_err(FileMutationError::unchanged)?
                != self.parent_identity
        {
            return Err(FileMutationError::unchanged(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "filesystem object changed before rename",
            )));
        }
        let source_path = self.current_path.clone();
        let source_name = self
            .current_name()
            .map_err(FileMutationError::unchanged)?
            .to_os_string();
        let target_name = target
            .file_name()
            .ok_or_else(|| {
                FileMutationError::unchanged(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "target has no file name",
                ))
            })?
            .to_os_string();
        if self
            .entry_identity(&source_name)
            .map_err(FileMutationError::unchanged)?
            != self.source_identity
        {
            return Err(FileMutationError::unchanged(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "filesystem directory entry changed before rename",
            )));
        }

        #[cfg(any(
            target_os = "android",
            target_os = "linux",
            target_os = "macos",
            target_os = "ios",
            target_os = "redox"
        ))]
        rustix::fs::renameat_with(
            &self.parent,
            &source_name,
            &self.parent,
            &target_name,
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .map_err(std::io::Error::from)
        .map_err(FileMutationError::unchanged)?;

        #[cfg(not(any(
            target_os = "android",
            target_os = "linux",
            target_os = "macos",
            target_os = "ios",
            target_os = "redox"
        )))]
        std::fs::hard_link(&self.current_path, target).map_err(FileMutationError::unchanged)?;

        self.current_path = target.to_path_buf();
        if self.entry_identity(&target_name).ok() != Some(self.source_identity) {
            return Err(self.rollback_moved_after_error(
                &target_name,
                &source_name,
                &source_path,
                target,
                std::io::Error::other("filesystem object changed during rename"),
            ));
        }

        #[cfg(not(any(
            target_os = "android",
            target_os = "linux",
            target_os = "macos",
            target_os = "ios",
            target_os = "redox"
        )))]
        std::fs::remove_file(&source_path)
            .map_err(|error| FileMutationError::target_or_unknown(error, target))?;

        Ok(())
    }

    pub fn delete(&mut self) -> Result<(), FileMutationError> {
        if opened_file_identity(&self._file).map_err(FileMutationError::unchanged)?
            != self.source_identity
            || opened_file_identity(&self.parent).map_err(FileMutationError::unchanged)?
                != self.parent_identity
        {
            return Err(FileMutationError::unchanged(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "filesystem object changed before delete",
            )));
        }

        #[cfg(any(
            target_os = "android",
            target_os = "linux",
            target_os = "macos",
            target_os = "ios",
            target_os = "redox"
        ))]
        {
            use std::sync::atomic::{AtomicU64, Ordering};

            static DELETE_COUNTER: AtomicU64 = AtomicU64::new(0);
            let source_path = self.current_path.clone();
            let source_name = self
                .current_name()
                .map_err(FileMutationError::unchanged)?
                .to_os_string();
            if self
                .entry_identity(&source_name)
                .map_err(FileMutationError::unchanged)?
                != self.source_identity
            {
                return Err(FileMutationError::unchanged(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "filesystem directory entry changed before delete",
                )));
            }
            let mut tombstone = None;
            for _ in 0..16 {
                let sequence = DELETE_COUNTER.fetch_add(1, Ordering::Relaxed);
                let candidate = std::ffi::OsString::from(format!(
                    ".clipline-delete-{}-{sequence}.tmp",
                    std::process::id()
                ));
                match rustix::fs::renameat_with(
                    &self.parent,
                    &source_name,
                    &self.parent,
                    &candidate,
                    rustix::fs::RenameFlags::NOREPLACE,
                ) {
                    Ok(()) => {
                        tombstone = Some(candidate);
                        break;
                    }
                    Err(rustix::io::Errno::EXIST) => {}
                    Err(error) => {
                        return Err(FileMutationError::unchanged(std::io::Error::from(error)));
                    }
                }
            }
            let tombstone = tombstone.ok_or_else(|| {
                FileMutationError::unchanged(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "could not reserve a bounded delete tombstone",
                ))
            })?;
            let tombstone_path = source_path.with_file_name(&tombstone);
            self.current_path = tombstone_path.clone();
            if self.entry_identity(&tombstone).ok() != Some(self.source_identity) {
                return Err(self.rollback_moved_after_error(
                    &tombstone,
                    &source_name,
                    &source_path,
                    &tombstone_path,
                    std::io::Error::other("filesystem object changed during delete"),
                ));
            }
            if let Err(error) =
                rustix::fs::unlinkat(&self.parent, &tombstone, rustix::fs::AtFlags::empty())
            {
                return Err(self.rollback_moved_after_error(
                    &tombstone,
                    &source_name,
                    &source_path,
                    &tombstone_path,
                    std::io::Error::other(format!(
                        "delete selected file: {}",
                        std::io::Error::from(error)
                    )),
                ));
            }
            Ok(())
        }

        #[cfg(not(any(
            target_os = "android",
            target_os = "linux",
            target_os = "macos",
            target_os = "ios",
            target_os = "redox"
        )))]
        {
            std::fs::remove_file(&self.current_path).map_err(FileMutationError::unchanged)
        }
    }

    fn current_name(&self) -> std::io::Result<&std::ffi::OsStr> {
        self.current_path.file_name().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "file has no name")
        })
    }

    #[cfg(unix)]
    fn entry_identity(&self, name: &std::ffi::OsStr) -> std::io::Result<FileIdentity> {
        let stat = rustix::fs::statat(&self.parent, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
            .map_err(std::io::Error::from)?;
        if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::RegularFile {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "mutation path is no longer a regular file",
            ));
        }
        Ok(FileIdentity {
            device: stat.st_dev as u64,
            file: stat.st_ino as u64,
        })
    }

    #[cfg(any(
        target_os = "android",
        target_os = "linux",
        target_os = "macos",
        target_os = "ios",
        target_os = "redox"
    ))]
    fn rename_relative_noreplace(
        &self,
        from: &std::ffi::OsStr,
        to: &std::ffi::OsStr,
    ) -> std::io::Result<()> {
        rustix::fs::renameat_with(
            &self.parent,
            from,
            &self.parent,
            to,
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .map_err(std::io::Error::from)
    }

    #[cfg(all(
        unix,
        not(any(
            target_os = "android",
            target_os = "linux",
            target_os = "macos",
            target_os = "ios",
            target_os = "redox"
        ))
    ))]
    fn rename_relative_noreplace(
        &self,
        from: &std::ffi::OsStr,
        to: &std::ffi::OsStr,
    ) -> std::io::Result<()> {
        let parent = self.current_path.parent().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "file has no parent")
        })?;
        let from = parent.join(from);
        let to = parent.join(to);
        std::fs::hard_link(&from, &to)?;
        std::fs::remove_file(from)
    }

    fn rollback_moved_after_error(
        &mut self,
        moved_name: &std::ffi::OsStr,
        source_name: &std::ffi::OsStr,
        source_path: &std::path::Path,
        moved_path: &std::path::Path,
        failure: std::io::Error,
    ) -> FileMutationError {
        match self.entry_identity(moved_name) {
            Ok(identity) if identity == self.source_identity => {
                match self.rename_relative_noreplace(moved_name, source_name) {
                    Ok(()) => {
                        self.current_path = source_path.to_path_buf();
                        FileMutationError::unchanged(failure)
                    }
                    Err(rollback) => FileMutationError::target_or_unknown(
                        std::io::Error::other(format!("{failure}; rollback failed: {rollback}")),
                        moved_path,
                    ),
                }
            }
            Ok(_) => FileMutationError::target_or_unknown(
                std::io::Error::other(format!(
                    "{failure}; refusing to roll back an unverified destination"
                )),
                moved_path,
            ),
            Err(verify) => FileMutationError::target_or_unknown(
                std::io::Error::other(format!(
                    "{failure}; could not verify destination for rollback: {verify}"
                )),
                moved_path,
            ),
        }
    }
}

/// Move an existing file to an unused sibling name without replacing anything.
pub fn rename_file_noreplace(
    from: &std::path::Path,
    to: &std::path::Path,
) -> Result<(), FileMutationError> {
    let source_identity = file_identity(from).map_err(FileMutationError::unchanged)?;
    rename_file_noreplace_if_identity(from, to, source_identity)
}

/// Move the selected file to an unused sibling only while its identity still matches.
pub fn rename_file_noreplace_if_identity(
    from: &std::path::Path,
    to: &std::path::Path,
    source_identity: FileIdentity,
) -> Result<(), FileMutationError> {
    let parent = from.parent().ok_or_else(|| {
        FileMutationError::unchanged(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "file has no parent",
        ))
    })?;
    let parent_file = open_directory_nofollow(parent).map_err(FileMutationError::unchanged)?;
    let parent_identity =
        opened_file_identity(&parent_file).map_err(FileMutationError::unchanged)?;
    rename_file_noreplace_if_identities(from, to, source_identity, parent_identity)
}

/// Move the selected file within the exact selected parent directory.
pub fn rename_file_noreplace_if_identities(
    from: &std::path::Path,
    to: &std::path::Path,
    source_identity: FileIdentity,
    parent_identity: FileIdentity,
) -> Result<(), FileMutationError> {
    let mut fence = FileMutationFence::acquire(from, source_identity, parent_identity)
        .map_err(FileMutationError::unchanged)?;
    fence.rename_noreplace(to)
}

/// Remove the selected file only while its identity still matches.
pub fn remove_file_if_identity(
    path: &std::path::Path,
    source_identity: FileIdentity,
) -> Result<(), FileMutationError> {
    let parent = path.parent().ok_or_else(|| {
        FileMutationError::unchanged(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "file has no parent",
        ))
    })?;
    let parent_file = open_directory_nofollow(parent).map_err(FileMutationError::unchanged)?;
    let parent_identity =
        opened_file_identity(&parent_file).map_err(FileMutationError::unchanged)?;
    remove_file_if_identities(path, source_identity, parent_identity)
}

/// Remove the selected file only within the exact selected parent directory.
pub fn remove_file_if_identities(
    path: &std::path::Path,
    source_identity: FileIdentity,
    parent_identity: FileIdentity,
) -> Result<(), FileMutationError> {
    let mut fence = FileMutationFence::acquire(path, source_identity, parent_identity)
        .map_err(FileMutationError::unchanged)?;
    fence.delete()
}

fn encode_journal_name(name: &std::ffi::OsStr) -> String {
    use std::fmt::Write as _;

    #[cfg(windows)]
    let units: Vec<u16> = {
        use std::os::windows::ffi::OsStrExt as _;
        name.encode_wide().collect()
    };
    #[cfg(unix)]
    let units: Vec<u16> = {
        use std::os::unix::ffi::OsStrExt as _;
        name.as_bytes()
            .iter()
            .map(|byte| u16::from(*byte))
            .collect()
    };
    #[cfg(not(any(windows, unix)))]
    let units: Vec<u16> = name.to_string_lossy().encode_utf16().collect();

    let mut encoded = String::with_capacity(units.len().saturating_mul(4));
    for unit in units {
        let _ = write!(encoded, "{unit:04x}");
    }
    encoded
}

fn decode_journal_name(encoded: &str) -> std::io::Result<std::ffi::OsString> {
    if encoded.is_empty() || !encoded.len().is_multiple_of(4) || encoded.len() > 4096 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "replacement journal contains an invalid file name",
        ));
    }
    let units = encoded
        .as_bytes()
        .chunks_exact(4)
        .map(|digits| {
            let text = std::str::from_utf8(digits).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid journal name")
            })?;
            u16::from_str_radix(text, 16).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid journal name")
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    #[cfg(windows)]
    let name = {
        use std::os::windows::ffi::OsStringExt as _;
        std::ffi::OsString::from_wide(&units)
    };
    #[cfg(unix)]
    let name = {
        use std::os::unix::ffi::OsStringExt as _;
        let bytes = units
            .into_iter()
            .map(|unit| {
                u8::try_from(unit).map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "replacement journal contains a non-byte Unix file name",
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        std::ffi::OsString::from_vec(bytes)
    };
    #[cfg(not(any(windows, unix)))]
    let name = std::ffi::OsString::from(String::from_utf16(&units).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "replacement journal contains invalid UTF-16",
        )
    })?);

    let path = std::path::Path::new(&name);
    if path.file_name() != Some(name.as_os_str())
        || !matches!(
            path.components().next(),
            Some(std::path::Component::Normal(_))
        )
        || path.components().nth(1).is_some()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "replacement journal file name is not one safe component",
        ));
    }
    Ok(name)
}

fn open_regular_file_in_directory(
    parent_file: &std::fs::File,
    parent: &std::path::Path,
    name: &std::ffi::OsStr,
) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use rustix::fs::{Mode, OFlags};

        let _ = parent;
        let owned = rustix::fs::openat(
            parent_file,
            name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?;
        let file = std::fs::File::from(owned);
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "refusing to open a filesystem link or non-file",
            ));
        }
        Ok(file)
    }

    #[cfg(windows)]
    {
        let _ = parent_file;
        open_regular_file_nofollow(&parent.join(name))
    }

    #[cfg(not(any(windows, unix)))]
    {
        let _ = (parent_file, parent, name);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "directory-relative regular file opens are unavailable on this platform",
        ))
    }
}

fn create_new_regular_file_in_directory(
    parent_file: &std::fs::File,
    parent: &std::path::Path,
    name: &std::ffi::OsStr,
) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use rustix::fs::{Mode, OFlags};

        let _ = parent;
        let owned = rustix::fs::openat(
            parent_file,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(std::io::Error::from)?;
        Ok(std::fs::File::from(owned))
    }

    #[cfg(windows)]
    {
        let _ = parent_file;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(parent.join(name))
    }

    #[cfg(not(any(windows, unix)))]
    {
        let _ = (parent_file, parent, name);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "directory-relative file creation is unavailable on this platform",
        ))
    }
}

fn open_directory_in_directory(
    parent_file: &std::fs::File,
    parent: &std::path::Path,
    name: &std::ffi::OsStr,
) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use rustix::fs::{Mode, OFlags};

        let _ = parent;
        let owned = rustix::fs::openat(
            parent_file,
            name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::DIRECTORY,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?;
        let file = std::fs::File::from(owned);
        if !file.metadata()?.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "refusing to open a filesystem link or non-directory",
            ));
        }
        Ok(file)
    }

    #[cfg(windows)]
    {
        let _ = parent_file;
        open_directory_nofollow(&parent.join(name))
    }

    #[cfg(not(any(windows, unix)))]
    {
        let _ = (parent_file, parent, name);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "directory-relative directory opens are unavailable on this platform",
        ))
    }
}

fn directory_names_bounded(
    parent_file: &std::fs::File,
    parent: &std::path::Path,
    maximum_entries: usize,
) -> std::io::Result<Vec<std::ffi::OsString>> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;

        let _ = parent;
        let mut names = Vec::new();
        let directory = rustix::fs::Dir::read_from(parent_file).map_err(std::io::Error::from)?;
        for entry in directory {
            let entry = entry.map_err(std::io::Error::from)?;
            let bytes = entry.file_name().to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            if names.len() >= maximum_entries {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "replacement recovery exceeds its directory-entry budget",
                ));
            }
            names.push(std::ffi::OsStr::from_bytes(bytes).to_owned());
        }
        Ok(names)
    }

    #[cfg(windows)]
    {
        let _ = parent_file;
        let mut names = Vec::new();
        for entry in std::fs::read_dir(parent)? {
            if names.len() >= maximum_entries {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "replacement recovery exceeds its directory-entry budget",
                ));
            }
            names.push(entry?.file_name());
        }
        Ok(names)
    }

    #[cfg(not(any(windows, unix)))]
    {
        let _ = (parent_file, parent, maximum_entries);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "bounded directory enumeration is unavailable on this platform",
        ))
    }
}

fn regular_file_identity_in_directory_if_present(
    parent_file: &std::fs::File,
    parent: &std::path::Path,
    name: &std::ffi::OsStr,
) -> std::io::Result<Option<FileIdentity>> {
    match open_regular_file_in_directory(parent_file, parent, name) {
        Ok(file) => opened_file_identity(&file).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn mutation_fence_in_directory(
    parent_file: &std::fs::File,
    parent: &std::path::Path,
    name: &std::ffi::OsStr,
    source_identity: FileIdentity,
    parent_identity: FileIdentity,
) -> std::io::Result<FileMutationFence> {
    #[cfg(unix)]
    {
        let file = open_regular_file_in_directory(parent_file, parent, name)?;
        if opened_file_identity(&file)? != source_identity
            || opened_file_identity(parent_file)? != parent_identity
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "filesystem object changed while acquiring a relative mutation fence",
            ));
        }
        Ok(FileMutationFence {
            _file: file,
            current_path: parent.join(name),
            source_identity,
            parent: parent_file.try_clone()?,
            parent_identity,
        })
    }

    #[cfg(windows)]
    {
        let _ = parent_file;
        FileMutationFence::acquire(&parent.join(name), source_identity, parent_identity)
    }

    #[cfg(not(any(windows, unix)))]
    {
        let _ = (parent_file, parent, name, source_identity, parent_identity);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "directory-relative mutation fences are unavailable on this platform",
        ))
    }
}

fn remove_file_in_directory_if_identity(
    parent_file: &std::fs::File,
    parent: &std::path::Path,
    name: &std::ffi::OsStr,
    source_identity: FileIdentity,
    parent_identity: FileIdentity,
) -> Result<(), FileMutationError> {
    #[cfg(unix)]
    {
        let mut fence = mutation_fence_in_directory(
            parent_file,
            parent,
            name,
            source_identity,
            parent_identity,
        )
        .map_err(FileMutationError::unchanged)?;
        fence.delete()
    }

    #[cfg(windows)]
    {
        let mut fence = mutation_fence_in_directory(
            parent_file,
            parent,
            name,
            source_identity,
            parent_identity,
        )
        .map_err(FileMutationError::unchanged)?;
        fence.delete()
    }

    #[cfg(not(any(windows, unix)))]
    {
        let _ = (parent_file, parent, name, source_identity, parent_identity);
        Err(FileMutationError::unchanged(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "directory-relative deletion is unavailable on this platform",
        )))
    }
}

fn rename_file_in_directory_noreplace_if_identity(
    parent_file: &std::fs::File,
    parent: &std::path::Path,
    from_name: &std::ffi::OsStr,
    to_name: &std::ffi::OsStr,
    source_identity: FileIdentity,
    parent_identity: FileIdentity,
) -> Result<(), FileMutationError> {
    #[cfg(unix)]
    {
        let mut fence = mutation_fence_in_directory(
            parent_file,
            parent,
            from_name,
            source_identity,
            parent_identity,
        )
        .map_err(FileMutationError::unchanged)?;
        fence.rename_noreplace(&parent.join(to_name))
    }

    #[cfg(windows)]
    {
        let mut fence = mutation_fence_in_directory(
            parent_file,
            parent,
            from_name,
            source_identity,
            parent_identity,
        )
        .map_err(FileMutationError::unchanged)?;
        fence.rename_noreplace(&parent.join(to_name))
    }

    #[cfg(not(any(windows, unix)))]
    {
        let _ = (
            parent_file,
            parent,
            from_name,
            to_name,
            source_identity,
            parent_identity,
        );
        Err(FileMutationError::unchanged(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "directory-relative rename is unavailable on this platform",
        )))
    }
}

fn sync_directory_authority(directory: &std::fs::File) -> std::io::Result<()> {
    match directory.sync_all() {
        Ok(()) => Ok(()),
        #[cfg(windows)]
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::InvalidInput
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn create_replacement_journal(
    parent_file: &std::fs::File,
    parent: &std::path::Path,
    journal_name: &std::ffi::OsStr,
    record: &ReplacementJournal,
) -> Result<FileIdentity, FileMutationError> {
    use std::io::Write as _;

    #[cfg(unix)]
    let mut file = {
        use rustix::fs::{Mode, OFlags};

        let owned = rustix::fs::openat(
            parent_file,
            journal_name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(std::io::Error::from)
        .map_err(FileMutationError::unchanged)?;
        std::fs::File::from(owned)
    };
    #[cfg(windows)]
    let mut file = {
        let _ = parent_file;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(parent.join(journal_name))
            .map_err(FileMutationError::unchanged)?
    };
    #[cfg(not(any(windows, unix)))]
    let mut file = {
        let _ = (parent_file, parent, journal_name);
        return Err(FileMutationError::unchanged(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "directory-relative journal creation is unavailable on this platform",
        )));
    };
    let identity = opened_file_identity(&file).map_err(FileMutationError::unchanged)?;
    let bytes = serde_json::to_vec(record).map_err(|error| {
        FileMutationError::unchanged(std::io::Error::other(format!(
            "serialize replacement journal: {error}"
        )))
    })?;
    if bytes.len() as u64 > MAX_REPLACEMENT_JOURNAL_BYTES {
        let _ = remove_file_in_directory_if_identity(
            parent_file,
            parent,
            journal_name,
            identity,
            record.parent_identity,
        );
        return Err(FileMutationError::unchanged(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "replacement journal exceeds its byte limit",
        )));
    }
    if let Err(error) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = remove_file_in_directory_if_identity(
            parent_file,
            parent,
            journal_name,
            identity,
            record.parent_identity,
        );
        return Err(FileMutationError::unchanged(error));
    }
    drop(file);
    if let Err(error) = sync_directory_authority(parent_file) {
        let _ = remove_file_in_directory_if_identity(
            parent_file,
            parent,
            journal_name,
            identity,
            record.parent_identity,
        );
        return Err(FileMutationError::unchanged(error));
    }
    Ok(identity)
}

fn finish_recovered_journal(
    parent_file: &std::fs::File,
    parent: &std::path::Path,
    journal_name: &std::ffi::OsStr,
    journal_identity: FileIdentity,
    parent_identity: FileIdentity,
) -> Result<(), String> {
    sync_directory_authority(parent_file)
        .map_err(|error| format!("sync recovered namespace before journal removal: {error}"))?;
    remove_file_in_directory_if_identity(
        parent_file,
        parent,
        journal_name,
        journal_identity,
        parent_identity,
    )
    .map_err(|error| format!("remove completed replacement journal: {error}"))?;
    sync_directory_authority(parent_file)
        .map_err(|error| format!("sync replacement journal removal: {error}"))
}

/// Recover bounded replacement journals in one already-validated directory.
///
/// Recovery is idempotent and identity-gated. A foreign winner is never moved
/// or deleted; its journal and selected backup remain explicit in `unresolved`.
pub fn recover_pending_replacements(
    parent: &std::path::Path,
) -> std::io::Result<ReplacementRecoveryReport> {
    let parent_file = open_directory_nofollow(parent)?;
    recover_pending_replacements_with_authority(
        parent,
        &parent_file,
        MAX_REPLACEMENT_RECOVERY_ENTRIES,
        MAX_PENDING_REPLACEMENT_JOURNALS,
    )
}

/// Recover journals only while the directory path still names the selected parent.
pub fn recover_pending_replacements_if_identity(
    parent: &std::path::Path,
    expected_parent_identity: FileIdentity,
) -> std::io::Result<ReplacementRecoveryReport> {
    let parent_file = open_directory_nofollow(parent)?;
    let parent_identity = opened_file_identity(&parent_file)?;
    if parent_identity != expected_parent_identity {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "replacement recovery directory changed after validation",
        ));
    }
    recover_pending_replacements_with_authority(
        parent,
        &parent_file,
        MAX_REPLACEMENT_RECOVERY_ENTRIES,
        MAX_PENDING_REPLACEMENT_JOURNALS,
    )
}

/// Recover replacement journals at the selected root and one session level.
///
/// One retained no-follow root authority governs discovery and every child
/// open. The total work and journal count are bounded across the whole tree.
pub fn recover_pending_replacements_in_tree(
    root: &std::path::Path,
) -> std::io::Result<ReplacementRecoveryReport> {
    let root_file = open_directory_nofollow(root)?;
    recover_pending_replacements_in_tree_with_authority(root, &root_file)
}

/// Recover a selected root tree only if the path still has its validated identity.
pub fn recover_pending_replacements_in_tree_if_identity(
    root: &std::path::Path,
    expected_root_identity: FileIdentity,
) -> std::io::Result<ReplacementRecoveryReport> {
    let root_file = open_directory_nofollow(root)?;
    if opened_file_identity(&root_file)? != expected_root_identity {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "replacement recovery root changed after validation",
        ));
    }
    recover_pending_replacements_in_tree_with_authority(root, &root_file)
}

fn recover_pending_replacements_in_tree_with_authority(
    root: &std::path::Path,
    root_file: &std::fs::File,
) -> std::io::Result<ReplacementRecoveryReport> {
    let mut remaining_entries = MAX_REPLACEMENT_RECOVERY_ENTRIES;
    let mut remaining_journals = MAX_PENDING_REPLACEMENT_JOURNALS;
    let mut report = recover_pending_replacements_with_authority(
        root,
        root_file,
        remaining_entries,
        remaining_journals,
    )?;
    remaining_entries = remaining_entries.saturating_sub(report.entries_examined);
    remaining_journals = remaining_journals.saturating_sub(report.journals_examined);

    let mut session_names = directory_names_bounded(root_file, root, remaining_entries)?;
    session_names.sort();
    remaining_entries = remaining_entries.saturating_sub(session_names.len());
    report.entries_examined = report.entries_examined.saturating_add(session_names.len());
    for session_name in session_names {
        let session_path = root.join(&session_name);
        let session_file = match open_directory_in_directory(root_file, root, &session_name) {
            Ok(file) => file,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::NotADirectory
                        | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                continue;
            }
            Err(error) => {
                report.unresolved.push((
                    session_path,
                    format!("open selected recovery session directory: {error}"),
                ));
                continue;
            }
        };
        let recovered = recover_pending_replacements_with_authority(
            &session_path,
            &session_file,
            remaining_entries,
            remaining_journals,
        )?;
        remaining_entries = remaining_entries.saturating_sub(recovered.entries_examined);
        remaining_journals = remaining_journals.saturating_sub(recovered.journals_examined);
        extend_recovery_report(&mut report, recovered);
    }
    Ok(report)
}

fn extend_recovery_report(
    report: &mut ReplacementRecoveryReport,
    recovered: ReplacementRecoveryReport,
) {
    report.rolled_back.extend(recovered.rolled_back);
    report.completed.extend(recovered.completed);
    report.stale.extend(recovered.stale);
    report.unresolved.extend(recovered.unresolved);
    report.entries_examined = report
        .entries_examined
        .saturating_add(recovered.entries_examined);
    report.journals_examined = report
        .journals_examined
        .saturating_add(recovered.journals_examined);
}

fn recover_pending_replacements_with_authority(
    parent: &std::path::Path,
    parent_file: &std::fs::File,
    maximum_entries: usize,
    maximum_journals: usize,
) -> std::io::Result<ReplacementRecoveryReport> {
    use std::io::Read as _;

    let parent_identity = opened_file_identity(parent_file)?;
    let names = directory_names_bounded(parent_file, parent, maximum_entries)?;
    let entries_examined = names.len();
    let mut journals: Vec<_> = names
        .into_iter()
        .filter(|name| {
            let text = name.to_string_lossy();
            text.starts_with(REPLACEMENT_JOURNAL_PREFIX)
                && text.ends_with(REPLACEMENT_JOURNAL_SUFFIX)
        })
        .collect();
    if journals.len() > maximum_journals {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "too many pending replacement journals",
        ));
    }
    journals.sort();
    let mut report = ReplacementRecoveryReport {
        entries_examined,
        journals_examined: journals.len(),
        ..ReplacementRecoveryReport::default()
    };
    for journal_name in journals {
        let journal_path = parent.join(&journal_name);
        let mut journal_file =
            match open_regular_file_in_directory(parent_file, parent, &journal_name) {
                Ok(file) => file,
                Err(error) => {
                    report.unresolved.push((journal_path, error.to_string()));
                    continue;
                }
            };
        let journal_identity = match opened_file_identity(&journal_file) {
            Ok(identity) => identity,
            Err(error) => {
                report.unresolved.push((journal_path, error.to_string()));
                continue;
            }
        };
        let length = match journal_file.metadata() {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                report.unresolved.push((journal_path, error.to_string()));
                continue;
            }
        };
        if length > MAX_REPLACEMENT_JOURNAL_BYTES {
            report.unresolved.push((
                journal_path,
                "replacement journal exceeds its byte limit".to_owned(),
            ));
            continue;
        }
        let mut bytes = Vec::new();
        if let Err(error) =
            std::io::Read::take(&mut journal_file, MAX_REPLACEMENT_JOURNAL_BYTES + 1)
                .read_to_end(&mut bytes)
        {
            report.unresolved.push((journal_path, error.to_string()));
            continue;
        }
        if bytes.len() as u64 > MAX_REPLACEMENT_JOURNAL_BYTES {
            report.unresolved.push((
                journal_path,
                "replacement journal exceeds its byte limit".to_owned(),
            ));
            continue;
        }
        let record: ReplacementJournal = match serde_json::from_slice(&bytes) {
            Ok(record) => record,
            Err(error) => {
                report.unresolved.push((journal_path, error.to_string()));
                continue;
            }
        };
        if record.schema_version != REPLACEMENT_JOURNAL_SCHEMA
            || record.parent_identity != parent_identity
        {
            report.unresolved.push((
                journal_path,
                "replacement journal schema or parent identity changed".to_owned(),
            ));
            continue;
        }
        let target_name = match decode_journal_name(&record.target_name) {
            Ok(name) => name,
            Err(error) => {
                report.unresolved.push((journal_path, error.to_string()));
                continue;
            }
        };
        let backup_name = match decode_journal_name(&record.backup_name) {
            Ok(name)
                if name
                    .to_string_lossy()
                    .starts_with(REPLACEMENT_BACKUP_PREFIX) =>
            {
                name
            }
            Ok(_) => {
                report.unresolved.push((
                    journal_path,
                    "replacement journal backup is outside the reserved namespace".to_owned(),
                ));
                continue;
            }
            Err(error) => {
                report.unresolved.push((journal_path, error.to_string()));
                continue;
            }
        };
        let replacement_name = match decode_journal_name(&record.replacement_name) {
            Ok(name) => name,
            Err(error) => {
                report.unresolved.push((journal_path, error.to_string()));
                continue;
            }
        };
        let target = parent.join(&target_name);
        let backup = parent.join(&backup_name);
        let target_state =
            regular_file_identity_in_directory_if_present(parent_file, parent, &target_name);
        let backup_state =
            regular_file_identity_in_directory_if_present(parent_file, parent, &backup_name);
        let replacement_state =
            regular_file_identity_in_directory_if_present(parent_file, parent, &replacement_name);
        let (target_state, backup_state, replacement_state) = match (
            target_state,
            backup_state,
            replacement_state,
        ) {
            (Ok(target), Ok(backup), Ok(replacement)) => (target, backup, replacement),
            (target, backup, replacement) => {
                report.unresolved.push((
                        journal_path,
                        format!(
                            "could not classify replacement state: target={target:?}, backup={backup:?}, replacement={replacement:?}"
                        ),
                    ));
                continue;
            }
        };

        if target_state == Some(record.replacement_identity) {
            if replacement_state.is_some() {
                report.unresolved.push((
                    journal_path,
                    "foreign entry occupies the published replacement temp path".to_owned(),
                ));
                continue;
            }
            match backup_state {
                Some(identity) if identity == record.target_identity => {
                    if let Err(error) = remove_file_in_directory_if_identity(
                        parent_file,
                        parent,
                        &backup_name,
                        record.target_identity,
                        parent_identity,
                    ) {
                        report.unresolved.push((
                            journal_path,
                            format!("remove selected replacement backup: {error}"),
                        ));
                        continue;
                    }
                }
                None => {}
                Some(_) => {
                    report.unresolved.push((
                        journal_path,
                        "foreign entry occupies the selected replacement backup path".to_owned(),
                    ));
                    continue;
                }
            }
            if let Err(error) = finish_recovered_journal(
                parent_file,
                parent,
                &journal_name,
                journal_identity,
                parent_identity,
            ) {
                report.unresolved.push((journal_path, error));
                continue;
            }
            report.completed.push(target);
        } else if target_state.is_none() && backup_state == Some(record.target_identity) {
            if let Err(error) = rename_file_in_directory_noreplace_if_identity(
                parent_file,
                parent,
                &backup_name,
                &target_name,
                record.target_identity,
                parent_identity,
            ) {
                report.unresolved.push((journal_path, error.to_string()));
                continue;
            }
            if replacement_state == Some(record.replacement_identity) {
                if let Err(error) = remove_file_in_directory_if_identity(
                    parent_file,
                    parent,
                    &replacement_name,
                    record.replacement_identity,
                    parent_identity,
                ) {
                    report.unresolved.push((
                        journal_path,
                        format!("remove rolled-back replacement: {error}"),
                    ));
                    continue;
                }
            } else if replacement_state.is_some() {
                report.unresolved.push((
                    journal_path,
                    "foreign entry occupies the rolled-back replacement temp path".to_owned(),
                ));
                continue;
            }
            if let Err(error) = finish_recovered_journal(
                parent_file,
                parent,
                &journal_name,
                journal_identity,
                parent_identity,
            ) {
                report.unresolved.push((journal_path, error));
                continue;
            }
            report.rolled_back.push(target);
        } else if target_state == Some(record.target_identity) && backup_state.is_none() {
            if replacement_state == Some(record.replacement_identity) {
                if let Err(error) = remove_file_in_directory_if_identity(
                    parent_file,
                    parent,
                    &replacement_name,
                    record.replacement_identity,
                    parent_identity,
                ) {
                    report.unresolved.push((
                        journal_path,
                        format!("remove unpublished replacement: {error}"),
                    ));
                    continue;
                }
            } else if replacement_state.is_some() {
                report.unresolved.push((
                    journal_path,
                    "foreign entry occupies the unpublished replacement temp path".to_owned(),
                ));
                continue;
            }
            if let Err(error) = finish_recovered_journal(
                parent_file,
                parent,
                &journal_name,
                journal_identity,
                parent_identity,
            ) {
                report.unresolved.push((journal_path, error));
                continue;
            }
            report.rolled_back.push(target);
        } else if target_state.is_none() && backup_state.is_none() && replacement_state.is_none() {
            if let Err(error) = finish_recovered_journal(
                parent_file,
                parent,
                &journal_name,
                journal_identity,
                parent_identity,
            ) {
                report.unresolved.push((journal_path, error));
                continue;
            }
            report.stale.push(journal_path);
        } else {
            report.unresolved.push((
                journal_path,
                format!("foreign or ambiguous replacement state; selected backup is {backup:?}"),
            ));
        }
    }
    Ok(report)
}

#[cfg(any(
    target_os = "android",
    target_os = "linux",
    target_os = "macos",
    target_os = "ios"
))]
fn try_exchange_replacement(
    replacement_fence: &mut FileMutationFence,
    target_fence: &mut FileMutationFence,
    replacement: &std::path::Path,
    target: &std::path::Path,
) -> Result<Option<()>, FileMutationError> {
    if opened_file_identity(&replacement_fence._file).map_err(FileMutationError::unchanged)?
        != replacement_fence.source_identity
        || opened_file_identity(&target_fence._file).map_err(FileMutationError::unchanged)?
            != target_fence.source_identity
        || replacement_fence.parent_identity != target_fence.parent_identity
    {
        return Err(FileMutationError::unchanged(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "filesystem object changed before atomic replacement",
        )));
    }
    let replacement_name = replacement.file_name().ok_or_else(|| {
        FileMutationError::unchanged(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "replacement has no file name",
        ))
    })?;
    let target_name = target.file_name().ok_or_else(|| {
        FileMutationError::unchanged(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "replacement target has no file name",
        ))
    })?;
    if replacement_fence
        .entry_identity(replacement_name)
        .map_err(FileMutationError::unchanged)?
        != replacement_fence.source_identity
        || replacement_fence
            .entry_identity(target_name)
            .map_err(FileMutationError::unchanged)?
            != target_fence.source_identity
    {
        return Err(FileMutationError::unchanged(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "filesystem directory entry changed before atomic replacement",
        )));
    }

    match rustix::fs::renameat_with(
        &replacement_fence.parent,
        replacement_name,
        &replacement_fence.parent,
        target_name,
        rustix::fs::RenameFlags::EXCHANGE,
    ) {
        Ok(()) => {}
        Err(rustix::io::Errno::INVAL | rustix::io::Errno::NOSYS | rustix::io::Errno::NOTSUP) => {
            return Ok(None);
        }
        Err(error) => return Err(FileMutationError::unchanged(std::io::Error::from(error))),
    }

    replacement_fence.current_path = target.to_path_buf();
    target_fence.current_path = replacement.to_path_buf();
    let post_matches = replacement_fence.entry_identity(target_name).ok()
        == Some(replacement_fence.source_identity)
        && replacement_fence.entry_identity(replacement_name).ok()
            == Some(target_fence.source_identity);
    if !post_matches {
        let rollback_safe = replacement_fence.entry_identity(target_name).ok()
            == Some(replacement_fence.source_identity)
            && replacement_fence.entry_identity(replacement_name).ok()
                == Some(target_fence.source_identity);
        if rollback_safe
            && rustix::fs::renameat_with(
                &replacement_fence.parent,
                replacement_name,
                &replacement_fence.parent,
                target_name,
                rustix::fs::RenameFlags::EXCHANGE,
            )
            .is_ok()
        {
            replacement_fence.current_path = replacement.to_path_buf();
            target_fence.current_path = target.to_path_buf();
            return Err(FileMutationError::unchanged(std::io::Error::other(
                "filesystem object changed during atomic replacement; exchange rolled back",
            )));
        }
        return Err(FileMutationError::target_or_unknown(
            std::io::Error::other(
                "filesystem object changed during atomic replacement; exchange state is uncertain",
            ),
            replacement,
        ));
    }

    if let Err(sync_error) = sync_directory_authority(&replacement_fence.parent) {
        let rollback_safe = replacement_fence.entry_identity(target_name).ok()
            == Some(replacement_fence.source_identity)
            && replacement_fence.entry_identity(replacement_name).ok()
                == Some(target_fence.source_identity);
        if rollback_safe
            && rustix::fs::renameat_with(
                &replacement_fence.parent,
                replacement_name,
                &replacement_fence.parent,
                target_name,
                rustix::fs::RenameFlags::EXCHANGE,
            )
            .is_ok()
        {
            replacement_fence.current_path = replacement.to_path_buf();
            target_fence.current_path = target.to_path_buf();
            return Err(FileMutationError::unchanged(std::io::Error::other(
                format!("sync atomic replacement directory: {sync_error}; exchange rolled back"),
            )));
        }
        return Err(FileMutationError::target_or_unknown(
            std::io::Error::other(format!(
                "sync atomic replacement directory: {sync_error}; exchange state is uncertain"
            )),
            replacement,
        ));
    }

    let _ = target_fence.delete();
    let _ = sync_directory_authority(&replacement_fence.parent);
    Ok(Some(()))
}

/// Publish one synchronized sibling over a selected target without ever replacing
/// an object whose identity changed after validation.
///
/// Supported Unix filesystems exchange the two selected entries atomically. The
/// Windows/portable path records a durable bounded journal before staging the old
/// target, so a crash in its necessarily non-atomic visibility window is recovered
/// on the next repository open. A concurrent destination always wins safely.
pub fn replace_file_if_identities(
    replacement: &std::path::Path,
    replacement_identity: FileIdentity,
    target: &std::path::Path,
    target_identity: FileIdentity,
) -> Result<(), FileMutationError> {
    let parent = replacement.parent().ok_or_else(|| {
        FileMutationError::unchanged(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "replacement file has no parent",
        ))
    })?;
    if target.parent() != Some(parent) || replacement == target {
        return Err(FileMutationError::unchanged(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "replacement and target must be distinct siblings",
        )));
    }
    let authority = DirectoryAuthority::open(parent).map_err(FileMutationError::unchanged)?;
    authority.replace_file_if_identities(
        replacement
            .file_name()
            .expect("replacement parent validated"),
        replacement_identity,
        target.file_name().expect("target parent validated"),
        target_identity,
    )
}

fn replace_file_if_identities_with_authority(
    authority: &DirectoryAuthority,
    replacement_name: &std::ffi::OsStr,
    replacement_identity: FileIdentity,
    target_name: &std::ffi::OsStr,
    target_identity: FileIdentity,
) -> Result<(), FileMutationError> {
    use std::sync::atomic::{AtomicU64, Ordering};

    static REPLACE_COUNTER: AtomicU64 = AtomicU64::new(0);

    let parent = authority.display_path.as_path();
    let parent_file = authority
        .directory
        .try_clone()
        .map_err(FileMutationError::unchanged)?;
    let parent_identity = authority.identity;
    let replacement_path = parent.join(replacement_name);
    let replacement = replacement_path.as_path();
    let target_path = parent.join(target_name);
    let target = target_path.as_path();
    let mut replacement_fence = mutation_fence_in_directory(
        &parent_file,
        parent,
        replacement_name,
        replacement_identity,
        parent_identity,
    )
    .map_err(FileMutationError::unchanged)?;
    let mut target_fence = mutation_fence_in_directory(
        &parent_file,
        parent,
        target_name,
        target_identity,
        parent_identity,
    )
    .map_err(FileMutationError::unchanged)?;

    #[cfg(any(
        target_os = "android",
        target_os = "linux",
        target_os = "macos",
        target_os = "ios"
    ))]
    if try_exchange_replacement(
        &mut replacement_fence,
        &mut target_fence,
        replacement,
        target,
    )?
    .is_some()
    {
        return Ok(());
    }

    let pending_journals =
        directory_names_bounded(&parent_file, parent, MAX_REPLACEMENT_RECOVERY_ENTRIES)
            .map_err(FileMutationError::unchanged)?
            .into_iter()
            .filter(|name| {
                let text = name.to_string_lossy();
                text.starts_with(REPLACEMENT_JOURNAL_PREFIX)
                    && text.ends_with(REPLACEMENT_JOURNAL_SUFFIX)
            })
            .count();
    if pending_journals >= MAX_PENDING_REPLACEMENT_JOURNALS {
        return Err(FileMutationError::unchanged(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "too many pending replacement journals",
        )));
    }

    let mut backup = None;
    let mut journal = None;
    for _ in 0..16 {
        let sequence = REPLACE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".clipline-replace-old-{}-{sequence}.tmp",
            std::process::id()
        ));
        let journal_path = parent.join(format!(
            "{REPLACEMENT_JOURNAL_PREFIX}{}-{sequence}{REPLACEMENT_JOURNAL_SUFFIX}",
            std::process::id()
        ));
        let record = ReplacementJournal {
            schema_version: REPLACEMENT_JOURNAL_SCHEMA,
            parent_identity,
            target_name: encode_journal_name(target.file_name().expect("validated sibling")),
            backup_name: encode_journal_name(candidate.file_name().expect("generated sibling")),
            replacement_name: encode_journal_name(
                replacement.file_name().expect("validated sibling"),
            ),
            target_identity,
            replacement_identity,
        };
        let journal_name = journal_path.file_name().expect("generated sibling");
        let journal_identity =
            match create_replacement_journal(&parent_file, parent, journal_name, &record) {
                Ok(identity) => identity,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            };
        match target_fence.rename_noreplace(&candidate) {
            Ok(()) => {
                backup = Some(candidate);
                journal = Some((journal_path, journal_identity));
                break;
            }
            Err(error)
                if !error.may_have_moved() && error.kind() == std::io::ErrorKind::AlreadyExists =>
            {
                let _ = remove_file_in_directory_if_identity(
                    &parent_file,
                    parent,
                    journal_name,
                    journal_identity,
                    parent_identity,
                );
            }
            Err(error) if error.may_have_moved() => {
                return match target_fence.rename_noreplace(target) {
                    Ok(()) => {
                        let _ = remove_file_in_directory_if_identity(
                            &parent_file,
                            parent,
                            journal_name,
                            journal_identity,
                            parent_identity,
                        );
                        Err(FileMutationError::unchanged(std::io::Error::other(
                            format!("stage selected target: {error}; rollback succeeded"),
                        )))
                    }
                    Err(rollback) => Err(FileMutationError::target_or_unknown(
                        std::io::Error::other(format!(
                            "stage selected target: {error}; rollback failed: {rollback}; recovery journal is {journal_path:?}"
                        )),
                        &candidate,
                    )),
                };
            }
            Err(error) => {
                let _ = remove_file_in_directory_if_identity(
                    &parent_file,
                    parent,
                    journal_name,
                    journal_identity,
                    parent_identity,
                );
                return Err(error);
            }
        }
    }
    let backup = backup.ok_or_else(|| {
        FileMutationError::unchanged(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not reserve a bounded replacement backup",
        ))
    })?;
    let (journal_path, journal_identity) = journal.expect("backup and journal commit together");

    if let Err(error) = replacement_fence.rename_noreplace(target) {
        return match target_fence.rename_noreplace(target) {
            Ok(()) if !error.may_have_moved() => {
                let _ = remove_file_in_directory_if_identity(
                    &parent_file,
                    parent,
                    journal_path.file_name().expect("generated sibling"),
                    journal_identity,
                    parent_identity,
                );
                Err(FileMutationError::unchanged(std::io::Error::other(
                    format!("publish replacement: {error}; selected target restored"),
                )))
            }
            Ok(()) => Err(FileMutationError::target_or_unknown(
                std::io::Error::other(format!(
                    "publish replacement: {error}; selected target restored but replacement location is uncertain; recovery journal is {journal_path:?}"
                )),
                target,
            )),
            Err(rollback) => Err(FileMutationError::target_or_unknown(
                std::io::Error::other(format!(
                    "publish replacement: {error}; rollback failed: {rollback}; selected target remains at {backup:?}; recovery journal is {journal_path:?}"
                )),
                if error.may_have_moved() {
                    target
                } else {
                    &backup
                },
            )),
        };
    }

    // Publication is committed. Failures after this point never turn success into
    // an error. Keeping the journal makes any old backup recoverable/cleanable on
    // the next repository open.
    let backup_removed = target_fence.delete().is_ok();
    let directory_synced = sync_directory_authority(&parent_file).is_ok();
    if backup_removed && directory_synced {
        let _ = remove_file_in_directory_if_identity(
            &parent_file,
            parent,
            journal_path.file_name().expect("generated sibling"),
            journal_identity,
            parent_identity,
        );
        let _ = sync_directory_authority(&parent_file);
    }
    Ok(())
}

#[cfg(test)]
mod replacement_recovery_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDirectory(std::path::PathBuf);

    impl TestDirectory {
        fn new(case: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "clipline-recovery-{case}-{}-{nonce}",
                std::process::id()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct RecoveryFixture {
        _directory: TestDirectory,
        target: std::path::PathBuf,
        backup: std::path::PathBuf,
        replacement: std::path::PathBuf,
        journal: std::path::PathBuf,
        target_identity: FileIdentity,
        replacement_identity: FileIdentity,
        parent_identity: FileIdentity,
    }

    impl RecoveryFixture {
        fn new(case: &str) -> Self {
            let directory = TestDirectory::new(case);
            let target = directory.0.join("metadata.json");
            let backup = directory.0.join(".clipline-replace-old-test.tmp");
            let replacement = directory.0.join("metadata.clipline-tmp");
            let journal = directory.0.join(".clipline-replace-journal-test.json");
            std::fs::write(&target, b"old").unwrap();
            std::fs::write(&replacement, b"new").unwrap();
            let target_identity = file_identity(&target).unwrap();
            let replacement_identity = file_identity(&replacement).unwrap();
            let parent_identity = file_identity(&directory.0).unwrap();
            let record = ReplacementJournal {
                schema_version: REPLACEMENT_JOURNAL_SCHEMA,
                parent_identity,
                target_name: encode_journal_name(target.file_name().unwrap()),
                backup_name: encode_journal_name(backup.file_name().unwrap()),
                replacement_name: encode_journal_name(replacement.file_name().unwrap()),
                target_identity,
                replacement_identity,
            };
            let parent_file = open_directory_nofollow(&directory.0).unwrap();
            create_replacement_journal(
                &parent_file,
                &directory.0,
                journal.file_name().unwrap(),
                &record,
            )
            .unwrap();
            Self {
                _directory: directory,
                target,
                backup,
                replacement,
                journal,
                target_identity,
                replacement_identity,
                parent_identity,
            }
        }

        fn stage_old_target(&self) {
            let mut fence = FileMutationFence::acquire(
                &self.target,
                self.target_identity,
                self.parent_identity,
            )
            .unwrap();
            fence.rename_noreplace(&self.backup).unwrap();
        }

        fn publish_replacement(&self) {
            let mut fence = FileMutationFence::acquire(
                &self.replacement,
                self.replacement_identity,
                self.parent_identity,
            )
            .unwrap();
            fence.rename_noreplace(&self.target).unwrap();
        }

        fn recover(&self) -> ReplacementRecoveryReport {
            recover_pending_replacements(&self._directory.0).unwrap()
        }
    }

    #[test]
    fn recovery_rolls_back_a_crash_between_stage_and_publish_and_is_idempotent() {
        let fixture = RecoveryFixture::new("stage-crash");
        fixture.stage_old_target();

        let report = fixture.recover();

        assert_eq!(report.rolled_back, vec![fixture.target.clone()]);
        assert_eq!(std::fs::read(&fixture.target).unwrap(), b"old");
        assert!(!fixture.backup.exists());
        assert!(!fixture.replacement.exists());
        assert!(!fixture.journal.exists());
        let repeated = fixture.recover();
        assert!(repeated.rolled_back.is_empty());
        assert!(repeated.completed.is_empty());
        assert!(repeated.stale.is_empty());
        assert!(repeated.unresolved.is_empty());
        assert_eq!(repeated.journals_examined, 0);
    }

    #[test]
    fn recovery_finishes_a_rollback_restored_before_its_sync_barrier() {
        let fixture = RecoveryFixture::new("rollback-pre-sync-crash");
        fixture.stage_old_target();
        let mut backup_fence = FileMutationFence::acquire(
            &fixture.backup,
            fixture.target_identity,
            fixture.parent_identity,
        )
        .unwrap();
        backup_fence.rename_noreplace(&fixture.target).unwrap();

        let report = fixture.recover();

        assert_eq!(report.rolled_back, vec![fixture.target.clone()]);
        assert_eq!(std::fs::read(&fixture.target).unwrap(), b"old");
        assert!(!fixture.backup.exists());
        assert!(!fixture.replacement.exists());
        assert!(!fixture.journal.exists());
    }

    #[test]
    fn recovery_finishes_cleanup_after_a_committed_publish() {
        let fixture = RecoveryFixture::new("published-crash");
        fixture.stage_old_target();
        fixture.publish_replacement();

        let report = fixture.recover();

        assert_eq!(report.completed, vec![fixture.target.clone()]);
        assert_eq!(std::fs::read(&fixture.target).unwrap(), b"new");
        assert!(!fixture.backup.exists());
        assert!(!fixture.journal.exists());
    }

    #[test]
    fn recovery_preserves_a_foreign_winner_and_the_selected_backup() {
        let fixture = RecoveryFixture::new("foreign-winner");
        fixture.stage_old_target();
        std::fs::write(&fixture.target, b"foreign").unwrap();

        let report = fixture.recover();

        assert_eq!(report.unresolved.len(), 1);
        assert_eq!(std::fs::read(&fixture.target).unwrap(), b"foreign");
        assert_eq!(std::fs::read(&fixture.backup).unwrap(), b"old");
        assert_eq!(std::fs::read(&fixture.replacement).unwrap(), b"new");
        assert!(fixture.journal.exists());
    }

    #[test]
    fn tree_recovery_rejects_a_root_replaced_after_identity_validation() {
        let directory = TestDirectory::new("root-identity-swap");
        let expected = file_identity(&directory.0).unwrap();
        let selected = directory.0.with_extension("selected");
        std::fs::rename(&directory.0, &selected).unwrap();
        std::fs::create_dir(&directory.0).unwrap();
        let foreign = directory.0.join("foreign.txt");
        std::fs::write(&foreign, b"foreign").unwrap();

        let error =
            recover_pending_replacements_in_tree_if_identity(&directory.0, expected).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(std::fs::read(&foreign).unwrap(), b"foreign");
        std::fs::remove_dir_all(&directory.0).unwrap();
        std::fs::rename(selected, &directory.0).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn recovery_uses_the_retained_directory_after_its_display_path_is_replaced() {
        let fixture = RecoveryFixture::new("retained-parent");
        fixture.stage_old_target();
        let authority = open_directory_nofollow(&fixture._directory.0).unwrap();
        let selected = fixture._directory.0.with_extension("selected");
        std::fs::rename(&fixture._directory.0, &selected).unwrap();
        std::fs::create_dir(&fixture._directory.0).unwrap();
        let foreign = fixture._directory.0.join("foreign.txt");
        std::fs::write(&foreign, b"foreign").unwrap();

        let report = recover_pending_replacements_with_authority(
            &fixture._directory.0,
            &authority,
            MAX_REPLACEMENT_RECOVERY_ENTRIES,
            MAX_PENDING_REPLACEMENT_JOURNALS,
        )
        .unwrap();

        assert_eq!(report.rolled_back, vec![fixture.target.clone()]);
        assert_eq!(
            std::fs::read(selected.join("metadata.json")).unwrap(),
            b"old"
        );
        assert_eq!(std::fs::read(&foreign).unwrap(), b"foreign");
        drop(authority);
        std::fs::remove_dir_all(&fixture._directory.0).unwrap();
        std::fs::rename(selected, &fixture._directory.0).unwrap();
    }

    #[test]
    fn recovery_rejects_work_past_its_total_entry_budget() {
        let directory = TestDirectory::new("entry-budget");
        std::fs::write(directory.0.join("one"), b"1").unwrap();
        std::fs::write(directory.0.join("two"), b"2").unwrap();
        let authority = open_directory_nofollow(&directory.0).unwrap();

        let error = recover_pending_replacements_with_authority(
            &directory.0,
            &authority,
            1,
            MAX_PENDING_REPLACEMENT_JOURNALS,
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn unreadable_journal_entry_is_reported_without_aborting_recovery() {
        let fixture = RecoveryFixture::new("journal-open-failure");
        std::fs::remove_file(&fixture.journal).unwrap();
        std::fs::create_dir(&fixture.journal).unwrap();

        let report = fixture.recover();

        assert_eq!(report.unresolved.len(), 1);
        assert_eq!(std::fs::read(&fixture.target).unwrap(), b"old");
        assert_eq!(std::fs::read(&fixture.replacement).unwrap(), b"new");
    }

    #[test]
    fn recovery_removes_a_stale_record_without_touching_other_files() {
        let fixture = RecoveryFixture::new("stale-record");
        std::fs::remove_file(&fixture.target).unwrap();
        std::fs::remove_file(&fixture.replacement).unwrap();
        let unrelated = fixture._directory.0.join("unrelated.json");
        std::fs::write(&unrelated, b"foreign").unwrap();

        let report = fixture.recover();

        assert_eq!(report.stale, vec![fixture.journal.clone()]);
        assert_eq!(std::fs::read(unrelated).unwrap(), b"foreign");
        assert!(!fixture.journal.exists());
    }
}
