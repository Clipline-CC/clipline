//! Safe Win32 filesystem operations shared by framework-neutral crates.

use std::fs::{File, OpenOptions};
use std::mem::{offset_of, size_of};
use std::os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle};
use std::path::{Path, PathBuf};

use windows::core::{Error as WindowsError, PCWSTR};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    FileDispositionInfo, FileRenameInfo, GetFileInformationByHandle, MoveFileExW,
    SetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_RENAME_INFO, FILE_RENAME_INFO_0,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES,
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
};

use crate::{FileIdentity, FileMutationError};

fn wide_null(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

pub fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    let from = wide_null(from);
    let to = wide_null(to);
    unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
        .map_err(io_error)
    }
}

pub fn file_identity_components(path: &Path) -> std::io::Result<(u64, u64)> {
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES.0)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(path)?;
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    unsafe {
        GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut information)
            .map_err(io_error)?;
    }
    let file_index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    Ok((u64::from(information.dwVolumeSerialNumber), file_index))
}

pub fn opened_file_identity_components(file: &File) -> std::io::Result<(u64, u64)> {
    let information = information(file)?;
    let file_index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    Ok((u64::from(information.dwVolumeSerialNumber), file_index))
}

pub fn open_regular_file_nofollow(path: &Path) -> std::io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)?;
    let information = information(&file)?;
    if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        || information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to open a filesystem link, reparse point, or non-file",
        ));
    }
    Ok(file)
}

pub fn open_regular_file_nofollow_for_metadata_write(path: &Path) -> std::io::Result<File> {
    let file = OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES.0)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)?;
    let information = information(&file)?;
    if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        || information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to open a filesystem link, reparse point, or non-file",
        ));
    }
    Ok(file)
}

fn io_error(error: WindowsError) -> std::io::Error {
    let hresult = error.code().0 as u32;
    if hresult & 0xFFFF_0000 == 0x8007_0000 {
        std::io::Error::from_raw_os_error((hresult & 0xFFFF) as i32)
    } else {
        std::io::Error::other(error.to_string())
    }
}

fn information(file: &File) -> std::io::Result<BY_HANDLE_FILE_INFORMATION> {
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    unsafe {
        GetFileInformationByHandle(raw_handle(file), &mut information).map_err(io_error)?;
    }
    Ok(information)
}

fn identity(information: &BY_HANDLE_FILE_INFORMATION) -> FileIdentity {
    let file = (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    FileIdentity::from_components(u64::from(information.dwVolumeSerialNumber), file)
}

fn raw_handle(file: &File) -> HANDLE {
    HANDLE(file.as_raw_handle())
}

pub fn open_directory_nofollow(path: &Path) -> std::io::Result<File> {
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES.0)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
        .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
        .open(path)?;
    let information = information(&file)?;
    if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        || information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to open a filesystem link, reparse point, or non-directory",
        ));
    }
    Ok(file)
}

pub fn sync_directory(path: &Path) -> std::io::Result<()> {
    match open_directory_nofollow(path)?.sync_all() {
        Ok(()) => Ok(()),
        // Windows commonly refuses FlushFileBuffers on a directory handle even
        // though the journal file itself was flushed. NTFS still preserves the
        // journal-before-rename ordering; treat the directory flush as best effort.
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

fn open_source(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .access_mode((DELETE | FILE_READ_ATTRIBUTES).0)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)
}

fn require_expected(
    information: &BY_HANDLE_FILE_INFORMATION,
    expected: FileIdentity,
    directory: bool,
) -> std::io::Result<()> {
    let attributes = information.dwFileAttributes;
    let actual_directory = attributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        || actual_directory != directory
        || identity(information) != expected
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "filesystem object changed while acquiring its mutation fence",
        ));
    }
    Ok(())
}

/// A source and parent-directory handle retained across one mutation transaction.
#[derive(Debug)]
pub struct FileMutationFence {
    source: File,
    _parent: File,
    current_path: PathBuf,
    parent_path: PathBuf,
}

impl FileMutationFence {
    pub fn acquire(
        path: &Path,
        source_identity: FileIdentity,
        parent_identity: FileIdentity,
    ) -> std::io::Result<Self> {
        let parent_path = path.parent().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "file has no parent")
        })?;
        let parent = open_directory_nofollow(parent_path)?;
        require_expected(&information(&parent)?, parent_identity, true)?;
        let source = open_source(path)?;
        require_expected(&information(&source)?, source_identity, false)?;
        Ok(Self {
            source,
            _parent: parent,
            current_path: path.to_path_buf(),
            parent_path: parent_path.to_path_buf(),
        })
    }

    pub fn rename_noreplace(&mut self, target: &Path) -> Result<(), FileMutationError> {
        if target.parent() != Some(self.parent_path.as_path()) {
            return Err(FileMutationError::unchanged(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "mutation target must be a sibling",
            )));
        }
        let wide: Vec<u16> = target.as_os_str().encode_wide().collect();
        let name_bytes = wide
            .len()
            .checked_mul(size_of::<u16>())
            .and_then(|bytes| u32::try_from(bytes).ok())
            .ok_or_else(|| {
                FileMutationError::unchanged(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "target name is too long",
                ))
            })?;
        let byte_length = offset_of!(FILE_RENAME_INFO, FileName)
            .checked_add(name_bytes as usize)
            .and_then(|bytes| bytes.checked_add(size_of::<u16>()))
            .ok_or_else(|| {
                FileMutationError::unchanged(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "target name is too long",
                ))
            })?;
        let word_length = byte_length.div_ceil(size_of::<usize>());
        let mut storage = vec![0_usize; word_length];
        let rename = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        unsafe {
            (*rename).Anonymous = FILE_RENAME_INFO_0 {
                ReplaceIfExists: false,
            };
            (*rename).RootDirectory = HANDLE::default();
            (*rename).FileNameLength = name_bytes;
            std::ptr::copy_nonoverlapping(
                wide.as_ptr(),
                std::ptr::addr_of_mut!((*rename).FileName).cast::<u16>(),
                wide.len(),
            );
            SetFileInformationByHandle(
                raw_handle(&self.source),
                FileRenameInfo,
                rename.cast(),
                u32::try_from(byte_length).map_err(|_| {
                    FileMutationError::unchanged(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "rename information is too large",
                    ))
                })?,
            )
            .map_err(io_error)
            .map_err(FileMutationError::unchanged)?;
        }
        self.current_path = target.to_path_buf();
        Ok(())
    }

    pub fn delete(&mut self) -> Result<(), FileMutationError> {
        let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
        unsafe {
            SetFileInformationByHandle(
                raw_handle(&self.source),
                FileDispositionInfo,
                std::ptr::addr_of!(disposition).cast(),
                u32::try_from(size_of::<FILE_DISPOSITION_INFO>()).expect("fixed Win32 structure"),
            )
            .map_err(io_error)
            .map_err(FileMutationError::unchanged)
        }
    }
}
