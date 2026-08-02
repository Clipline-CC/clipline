//! Safe wrappers around Windows shell activation.
//!
//! These functions accept targets separately from shell verbs and parameters. In
//! particular, paths are never interpolated into an `explorer.exe` command line.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use thiserror::Error;
use windows::core::PCWSTR;
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Shell::{
    ILCreateFromPathW, ILFree, SHOpenFolderAndSelectItems, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

const SHELL_SUCCESS_MINIMUM: isize = 33;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WindowsShellExecuteError {
    #[error("{field} contains an embedded NUL")]
    EmbeddedNul { field: &'static str },
    #[error("{context} failed with shell code {code}")]
    ShellCode { context: String, code: isize },
    #[error("{context}: Windows could not resolve the path")]
    PathResolution { context: String },
    #[error("{context}: {message}")]
    Native { context: String, message: String },
}

/// Opens a URL through the user's registered browser.
pub fn open_browser_url(url: &str, context: &str) -> Result<(), WindowsShellExecuteError> {
    execute(OsStr::new("open"), OsStr::new(url), None, None, context)
}

/// Opens a filesystem target through its registered Windows shell handler.
pub fn open_path(target: &Path, context: &str) -> Result<(), WindowsShellExecuteError> {
    execute(OsStr::new("open"), target.as_os_str(), None, None, context)
}

/// Opens a directory in the user's file explorer without constructing a command line.
pub fn open_folder(directory: &Path, context: &str) -> Result<(), WindowsShellExecuteError> {
    open_path(directory, context)
}

/// Opens Explorer with `target` selected.
///
/// `SHOpenFolderAndSelectItems` consumes a PIDL, so arbitrary spaces and shell metacharacters in
/// a path cannot be reinterpreted as command-line arguments.
pub fn reveal_in_explorer(target: &Path, context: &str) -> Result<(), WindowsShellExecuteError> {
    let _apartment = ComApartment::enter(context)?;
    let target = wide_nul_checked(target.as_os_str(), "explorer target")?;
    let item = unsafe { ILCreateFromPathW(PCWSTR(target.as_ptr())) };
    if item.is_null() {
        return Err(WindowsShellExecuteError::PathResolution {
            context: context.to_owned(),
        });
    }
    let item = OwnedItemIdList(item);
    unsafe { SHOpenFolderAndSelectItems(item.0, None, 0) }.map_err(|error| {
        WindowsShellExecuteError::Native {
            context: context.to_owned(),
            message: error.to_string(),
        }
    })
}

/// Compatibility entry point for callers whose target is not necessarily UTF-8.
pub fn open_with_shell(target: &OsStr, context: &str) -> Result<(), WindowsShellExecuteError> {
    execute(OsStr::new("open"), target, None, None, context)
}

pub(crate) fn run_as(
    executable: &Path,
    parameters: &OsStr,
    context: &str,
) -> Result<(), WindowsShellExecuteError> {
    execute(
        OsStr::new("runas"),
        executable.as_os_str(),
        Some(parameters),
        None,
        context,
    )
}

fn execute(
    verb: &OsStr,
    target: &OsStr,
    parameters: Option<&OsStr>,
    working_directory: Option<&Path>,
    context: &str,
) -> Result<(), WindowsShellExecuteError> {
    let verb = wide_nul_checked(verb, "shell operation")?;
    let target = wide_nul_checked(target, "shell target")?;
    let parameters = parameters
        .map(|value| wide_nul_checked(value, "shell parameters"))
        .transpose()?;
    let working_directory = working_directory
        .map(|value| wide_nul_checked(value.as_os_str(), "shell working directory"))
        .transpose()?;

    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(target.as_ptr()),
            parameters
                .as_ref()
                .map_or(PCWSTR::null(), |value| PCWSTR(value.as_ptr())),
            working_directory
                .as_ref()
                .map_or(PCWSTR::null(), |value| PCWSTR(value.as_ptr())),
            SW_SHOWNORMAL,
        )
    };
    classify_shell_result(result.0 as isize, context)
}

pub fn classify_shell_result(result: isize, context: &str) -> Result<(), WindowsShellExecuteError> {
    if result < SHELL_SUCCESS_MINIMUM {
        Err(WindowsShellExecuteError::ShellCode {
            context: context.to_owned(),
            code: result,
        })
    } else {
        Ok(())
    }
}

fn wide_nul_checked(
    value: &OsStr,
    field: &'static str,
) -> Result<Vec<u16>, WindowsShellExecuteError> {
    let mut wide = value.encode_wide().collect::<Vec<_>>();
    if wide.contains(&0) {
        return Err(WindowsShellExecuteError::EmbeddedNul { field });
    }
    wide.push(0);
    Ok(wide)
}

struct OwnedItemIdList(*mut windows::Win32::UI::Shell::Common::ITEMIDLIST);

impl Drop for OwnedItemIdList {
    fn drop(&mut self) {
        unsafe { ILFree(Some(self.0)) };
    }
}

struct ComApartment {
    must_uninitialize: bool,
}

impl ComApartment {
    fn enter(context: &str) -> Result<Self, WindowsShellExecuteError> {
        let result = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if result.is_ok() {
            return Ok(Self {
                // Both S_OK and S_FALSE increment the per-thread COM reference count.
                must_uninitialize: true,
            });
        }
        if result == RPC_E_CHANGED_MODE {
            // COM is already usable on this thread with another apartment model. We did not
            // increment its reference count, so the guard must not uninitialize it.
            return Ok(Self {
                must_uninitialize: false,
            });
        }
        Err(WindowsShellExecuteError::Native {
            context: context.to_owned(),
            message: windows::core::Error::from(result).to_string(),
        })
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.must_uninitialize {
            unsafe { CoUninitialize() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_result_uses_the_documented_success_boundary() {
        assert!(classify_shell_result(33, "open target").is_ok());
        assert_eq!(
            classify_shell_result(32, "open target").expect_err("failure"),
            WindowsShellExecuteError::ShellCode {
                context: "open target".into(),
                code: 32,
            }
        );
    }

    #[test]
    fn shell_targets_reject_embedded_nuls_before_calling_windows() {
        assert_eq!(
            wide_nul_checked(OsStr::new("bad\0target"), "shell target").expect_err("embedded NUL"),
            WindowsShellExecuteError::EmbeddedNul {
                field: "shell target"
            }
        );
    }
}
