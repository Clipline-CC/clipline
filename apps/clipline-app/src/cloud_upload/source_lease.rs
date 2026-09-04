//! Upload source immutability lease and active-source mutation guards.
use super::*;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct UploadSourceIdentity {
    volume_serial_number: u32,
    file_index: u64,
}

/// Keeps the upload source immutable for the complete upload attempt.
///
/// Windows sharing is checked against the underlying file, not merely this
/// path, so this also denies mutation through a hard link and prevents the
/// source from being deleted, renamed, or atomically replaced between hashing
/// and a retry reopening its bounded stream.
pub(crate) struct UploadSourceLease {
    file: Option<std::fs::File>,
    source_identity: UploadSourceIdentity,
}

impl UploadSourceLease {
    pub(crate) fn acquire(path: &Path) -> CloudApiResult<Self> {
        // Serialize identity discovery and registration with all app-side
        // mutation checks. If a mutation checked first, it wins; otherwise it
        // cannot observe an unregistered lease after this open succeeds.
        let _mutation_guard = crate::gc::lock_clip_mutations();
        let mut sources = ACTIVE_UPLOAD_SOURCES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(path)
            .map_err(|error| upload_file_error("lease upload source", path, error))?;
        let source_identity = opened_file_identity(&file)
            .map_err(|error| upload_file_error("identify upload source", path, error))?;
        *sources.entry(source_identity).or_default() += 1;
        Ok(Self {
            file: Some(file),
            source_identity,
        })
    }
}

impl Drop for UploadSourceLease {
    fn drop(&mut self) {
        // Release the kernel sharing lease before removing the user-facing
        // active marker, so a concurrent mutation never falls through to a
        // raw ERROR_SHARING_VIOLATION after being told the upload is idle.
        drop(self.file.take());
        let Some(sources) = ACTIVE_UPLOAD_SOURCES.get() else {
            return;
        };
        let mut sources = sources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let remove = match sources.get_mut(&self.source_identity) {
            Some(count) if *count > 1 => {
                *count -= 1;
                false
            }
            Some(_) => true,
            None => false,
        };
        if remove {
            sources.remove(&self.source_identity);
        }
    }
}

pub(crate) fn opened_file_identity(file: &File) -> std::io::Result<UploadSourceIdentity> {
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let result = unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) };
    if result == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(UploadSourceIdentity {
        volume_serial_number: info.dwVolumeSerialNumber,
        file_index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    })
}

pub(crate) fn is_active_upload_source(path: &Path) -> bool {
    let Some(sources) = ACTIVE_UPLOAD_SOURCES.get() else {
        return false;
    };
    let sources = sources
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if sources.is_empty() {
        return false;
    }
    let identity = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(path)
        .and_then(|file| opened_file_identity(&file));
    identity.is_ok_and(|identity| sources.contains_key(&identity))
}

pub(crate) fn active_upload_source_error(path: &Path) -> Option<String> {
    is_active_upload_source(path).then(|| ACTIVE_UPLOAD_MUTATION_ERROR.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_test_utils::TestDir;
    use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;

    #[test]
    fn upload_source_lease_denies_writes_and_deletes_until_drop() {
        let dir = TestDir::new("clipline-cloud-upload", "source-lease");
        let path = dir.path().join("clip.mp4");
        std::fs::write(&path, b"original").unwrap();

        let lease = UploadSourceLease::acquire(&path).unwrap();

        let write_error = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap_err();
        assert_eq!(
            write_error.raw_os_error(),
            Some(ERROR_SHARING_VIOLATION as i32),
            "write must fail specifically because the upload lease denies sharing"
        );
        assert!(
            std::fs::remove_file(&path).is_err(),
            "leased upload source must not be deletable"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"original");

        drop(lease);

        std::fs::write(&path, b"updated").expect("write succeeds after releasing upload lease");
        assert_eq!(std::fs::read(&path).unwrap(), b"updated");
    }

    #[test]
    fn upload_source_lease_reports_an_intentional_mutation_error_until_drop() {
        let dir = TestDir::new("clipline-cloud-upload", "source-mutation-message");
        let path = dir.path().join("clip.mp4");
        std::fs::write(&path, b"original").unwrap();

        assert_eq!(active_upload_source_error(&path), None);
        let lease = UploadSourceLease::acquire(&path).unwrap();
        assert_eq!(
            active_upload_source_error(&path).as_deref(),
            Some("clip is uploading; wait for the upload to finish")
        );

        drop(lease);
        assert_eq!(active_upload_source_error(&path), None);
    }

    #[test]
    fn upload_source_lease_refcounts_two_readers() {
        let dir = TestDir::new("clipline-cloud-upload", "source-mutation-refcount");
        let path = dir.path().join("clip.mp4");
        std::fs::write(&path, b"original").unwrap();

        let first = UploadSourceLease::acquire(&path).unwrap();
        let second = UploadSourceLease::acquire(&path).unwrap();
        drop(first);
        assert_eq!(
            active_upload_source_error(&path).as_deref(),
            Some("clip is uploading; wait for the upload to finish")
        );

        drop(second);
        assert_eq!(active_upload_source_error(&path), None);
    }

    #[test]
    fn upload_source_lease_matches_hard_link_aliases_by_file_identity() {
        let dir = TestDir::new("clipline-cloud-upload", "source-mutation-hard-link");
        let path = dir.path().join("clip.mp4");
        let alias = dir.path().join("clip-alias.mp4");
        std::fs::write(&path, b"original").unwrap();
        std::fs::hard_link(&path, &alias).unwrap();

        let lease = UploadSourceLease::acquire(&path).unwrap();
        assert!(is_active_upload_source(&alias));
        drop(lease);
        assert!(!is_active_upload_source(&alias));
    }

}