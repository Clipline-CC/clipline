//! Safe process and elevation handoff services for the Windows desktop shell.

use std::ffi::{c_void, OsStr};
use std::path::Path;

use thiserror::Error;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_INVALID_PARAMETER, FILETIME, HANDLE, WAIT_FAILED,
    WAIT_OBJECT_0,
};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, OpenProcessToken, WaitForSingleObject, INFINITE,
    PROCESS_ACCESS_RIGHTS, PROCESS_QUERY_LIMITED_INFORMATION,
};

use crate::{ProcessIdentity, ShellLaunch, ShellLaunchError};

use super::shell_execute::{run_as, WindowsShellExecuteError};

pub const ELEVATED_AFTER_ARGUMENT: &str = "--clipline-elevated-after";
const PROCESS_SYNCHRONIZE: u32 = 0x0010_0000;

#[derive(Debug, Error)]
pub enum WindowsProcessError {
    #[error("{operation}: {message}")]
    Native { operation: String, message: String },
    #[error("query process elevation {process_id}: Windows returned {returned} bytes")]
    TruncatedElevation { process_id: u32, returned: u32 },
    #[error("parse shell launch: {0}")]
    Launch(#[from] ShellLaunchError),
    #[error("locate Clipline executable for administrator restart: {0}")]
    CurrentExecutable(#[source] std::io::Error),
    #[error(transparent)]
    Shell(#[from] WindowsShellExecuteError),
    #[error("Administrator restart was cancelled or denied; Clipline is still running normally.")]
    ElevationDenied,
    #[error("wait for Clipline process {process_id}: unexpected wait result {result}")]
    UnexpectedWait { process_id: u32, result: u32 },
}

pub fn current_process_is_elevated() -> Result<bool, WindowsProcessError> {
    process_is_elevated(std::process::id())
}

pub fn process_is_elevated(process_id: u32) -> Result<bool, WindowsProcessError> {
    let process = open_process(
        PROCESS_QUERY_LIMITED_INFORMATION,
        process_id,
        format!("open process {process_id}"),
    )?;
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(process.raw, TOKEN_QUERY, &mut token) }
        .map_err(|error| native(format!("open process token {process_id}"), error))?;
    let token = OwnedHandle::new(token);

    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0_u32;
    unsafe {
        GetTokenInformation(
            token.raw,
            TokenElevation,
            Some((&raw mut elevation).cast::<c_void>()),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &raw mut returned,
        )
    }
    .map_err(|error| native(format!("query process elevation {process_id}"), error))?;
    if returned < std::mem::size_of::<TOKEN_ELEVATION>() as u32 {
        return Err(WindowsProcessError::TruncatedElevation {
            process_id,
            returned,
        });
    }
    Ok(elevation.TokenIsElevated != 0)
}

pub fn process_identity(process_id: u32) -> Result<ProcessIdentity, WindowsProcessError> {
    let process = open_process(
        PROCESS_QUERY_LIMITED_INFORMATION,
        process_id,
        format!("open process {process_id}"),
    )?;
    process_identity_from_handle(process_id, process.raw)
}

pub fn process_instance_id(process_id: u32) -> Result<String, WindowsProcessError> {
    let identity = process_identity(process_id)?;
    Ok(format!(
        "{}:{}",
        identity.process_id(),
        identity.creation_time()
    ))
}

/// Launches the current executable through UAC and tells the child to wait for this exact parent
/// process instance. The creation time prevents a recycled PID from satisfying the handoff.
pub fn launch_elevated_after(parent_process_id: u32) -> Result<(), WindowsProcessError> {
    let executable = std::env::current_exe().map_err(WindowsProcessError::CurrentExecutable)?;
    let parent = process_identity(parent_process_id)?;
    launch_elevated_executable(&executable, parent)
}

pub fn launch_elevated_executable(
    executable: &Path,
    parent: ProcessIdentity,
) -> Result<(), WindowsProcessError> {
    let parameters = elevation_restart_parameters(parent);
    match run_as(
        executable,
        OsStr::new(&parameters),
        "restart Clipline as administrator",
    ) {
        Ok(()) => {}
        Err(WindowsShellExecuteError::ShellCode { .. }) => {
            return Err(WindowsProcessError::ElevationDenied);
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

#[must_use]
pub fn elevation_restart_parameters(parent: ProcessIdentity) -> String {
    format!(
        "{ELEVATED_AFTER_ARGUMENT} {} {}",
        parent.process_id(),
        parent.creation_time()
    )
}

/// Parses the current process arguments with the shared bounded launch parser, then waits for an
/// exact elevation parent when the handoff flag is present.
pub fn wait_for_elevation_parent_from_args() -> Result<(), WindowsProcessError> {
    let launch = ShellLaunch::parse(std::env::args())?;
    wait_for_elevation_parent(launch.elevation_parent())
}

pub fn wait_for_elevation_parent(
    parent: Option<ProcessIdentity>,
) -> Result<(), WindowsProcessError> {
    parent.map_or(Ok(()), wait_for_process_exit)
}

pub fn wait_for_process_exit(parent: ProcessIdentity) -> Result<(), WindowsProcessError> {
    let desired_access =
        PROCESS_ACCESS_RIGHTS(PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION.0);
    let process = match unsafe { OpenProcess(desired_access, false, parent.process_id()) } {
        Ok(process) => OwnedHandle::new(process),
        Err(error) => {
            let code = unsafe { GetLastError() };
            if code == ERROR_INVALID_PARAMETER {
                // The parent completed after launching the elevated child but before this open.
                return Ok(());
            }
            return Err(native(
                format!(
                    "open Clipline process {} for elevation handoff",
                    parent.process_id()
                ),
                error,
            ));
        }
    };
    let actual = process_identity_from_handle(parent.process_id(), process.raw)?;
    if !process_identity_matches(parent, actual) {
        // The PID was recycled. Waiting would block on an unrelated process.
        return Ok(());
    }

    let result = unsafe { WaitForSingleObject(process.raw, INFINITE) };
    if result == WAIT_FAILED {
        return Err(last_native(format!(
            "wait for Clipline process {}",
            parent.process_id()
        )));
    }
    if result != WAIT_OBJECT_0 {
        return Err(WindowsProcessError::UnexpectedWait {
            process_id: parent.process_id(),
            result: result.0,
        });
    }
    Ok(())
}

#[must_use]
pub const fn process_identity_matches(expected: ProcessIdentity, actual: ProcessIdentity) -> bool {
    expected.process_id() == actual.process_id()
        && expected.creation_time() == actual.creation_time()
}

fn open_process(
    access: PROCESS_ACCESS_RIGHTS,
    process_id: u32,
    operation: String,
) -> Result<OwnedHandle, WindowsProcessError> {
    let process = unsafe { OpenProcess(access, false, process_id) }
        .map_err(|error| native(operation, error))?;
    Ok(OwnedHandle::new(process))
}

fn process_identity_from_handle(
    process_id: u32,
    process: HANDLE,
) -> Result<ProcessIdentity, WindowsProcessError> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe {
        GetProcessTimes(
            process,
            &raw mut creation,
            &raw mut exit,
            &raw mut kernel,
            &raw mut user,
        )
    }
    .map_err(|error| native(format!("query process creation time {process_id}"), error))?;
    let creation_time =
        (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
    ProcessIdentity::new(process_id, creation_time).map_err(WindowsProcessError::Launch)
}

fn native(operation: String, error: windows::core::Error) -> WindowsProcessError {
    WindowsProcessError::Native {
        operation,
        message: error.to_string(),
    }
}

fn last_native(operation: String) -> WindowsProcessError {
    WindowsProcessError::Native {
        operation,
        message: format!("Windows error {}", unsafe { GetLastError() }.0),
    }
}

struct OwnedHandle {
    raw: HANDLE,
}

impl OwnedHandle {
    const fn new(raw: HANDLE) -> Self {
        Self { raw }
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.raw) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elevated_restart_parameters_are_exact() {
        let parent = ProcessIdentity::new(4_242, 987_654_321).expect("identity");
        assert_eq!(
            elevation_restart_parameters(parent),
            "--clipline-elevated-after 4242 987654321"
        );
    }

    #[test]
    fn process_identity_match_rejects_a_recycled_pid() {
        let original = ProcessIdentity::new(4_242, 100).expect("original");
        let recycled = ProcessIdentity::new(4_242, 200).expect("recycled");
        assert!(!process_identity_matches(original, recycled));
        assert!(process_identity_matches(original, original));
    }
}
