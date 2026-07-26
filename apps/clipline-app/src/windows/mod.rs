//! Safe wrappers for the small Win32 surface owned by the application shell.

mod credential_store;
mod webview_memory;

use std::ffi::{c_void, OsStr};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

pub(crate) use credential_store::CredentialStore;
pub(crate) use webview_memory::{set_memory_target, MemoryTarget};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_INVALID_PARAMETER, FILETIME, HANDLE,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
};
use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
};
use windows_sys::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, OpenProcessToken, WaitForSingleObject, INFINITE,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

const ELEVATED_AFTER_ARGUMENT: &str = "--clipline-elevated-after";
const PROCESS_SYNCHRONIZE: u32 = 0x0010_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProcessIdentity {
    process_id: u32,
    creation_time: u64,
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

pub fn current_process_is_elevated() -> Result<bool, String> {
    process_is_elevated(std::process::id())
}

pub fn process_is_elevated(process_id: u32) -> Result<bool, String> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if process.is_null() {
        return Err(last_error(format!("open process {process_id}")));
    }
    let process = OwnedHandle(process);

    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &mut token) } == 0 {
        return Err(last_error(format!("open process token {process_id}")));
    }
    let token = OwnedHandle(token);

    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0u32;
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast::<c_void>(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    } == 0
    {
        return Err(last_error(format!("query process elevation {process_id}")));
    }
    if returned < std::mem::size_of::<TOKEN_ELEVATION>() as u32 {
        return Err(format!(
            "query process elevation {process_id}: Windows returned {returned} bytes"
        ));
    }
    Ok(elevation.TokenIsElevated != 0)
}

pub fn process_instance_id(process_id: u32) -> Result<String, String> {
    let identity = query_process_identity(process_id)?;
    Ok(format!(
        "{}:{}",
        identity.process_id, identity.creation_time
    ))
}

/// Launch this exact Clipline executable through the UAC `runas` verb. The
/// elevated child verifies and waits for this exact process instance before
/// starting Tauri, so it cannot overlap the existing single-instance owner.
pub fn launch_elevated_after(parent_process_id: u32) -> Result<(), String> {
    let executable = std::env::current_exe()
        .map_err(|e| format!("locate Clipline executable for administrator restart: {e}"))?;
    let parent = query_process_identity(parent_process_id)?;
    launch_elevated_executable(&executable, parent)
}

fn launch_elevated_executable(executable: &Path, parent: ProcessIdentity) -> Result<(), String> {
    let verb = wide_null_checked(OsStr::new("runas"), "shell operation")?;
    let executable = wide_null_checked(executable.as_os_str(), "Clipline executable")?;
    let parameters = elevation_restart_parameters(parent);
    let parameters = wide_null_checked(OsStr::new(&parameters), "restart parameters")?;
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            executable.as_ptr(),
            parameters.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if result as isize <= 32 {
        return Err(
            "Administrator restart was cancelled or denied; Clipline is still running normally."
                .into(),
        );
    }
    Ok(())
}

pub fn wait_for_elevation_parent_from_args() -> Result<(), String> {
    let Some(parent) = elevation_parent_from_args(std::env::args())? else {
        return Ok(());
    };
    wait_for_process_exit(parent)
}

fn elevation_parent_from_args<I>(args: I) -> Result<Option<ProcessIdentity>, String>
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    let mut args = args.into_iter();
    while let Some(argument) = args.next() {
        if argument.as_ref() != ELEVATED_AFTER_ARGUMENT {
            continue;
        }
        let raw_process_id = args
            .next()
            .ok_or_else(|| format!("{ELEVATED_AFTER_ARGUMENT} requires a process id"))?;
        let process_id = raw_process_id
            .as_ref()
            .parse::<u32>()
            .map_err(|_| format!("invalid parent process id: {}", raw_process_id.as_ref()))?;
        let raw_creation_time = args
            .next()
            .ok_or_else(|| format!("{ELEVATED_AFTER_ARGUMENT} requires a creation timestamp"))?;
        let creation_time = raw_creation_time.as_ref().parse::<u64>().map_err(|_| {
            format!(
                "invalid parent process creation timestamp: {}",
                raw_creation_time.as_ref()
            )
        })?;
        return Ok(Some(ProcessIdentity {
            process_id,
            creation_time,
        }));
    }
    Ok(None)
}

fn wait_for_process_exit(parent: ProcessIdentity) -> Result<(), String> {
    let process = unsafe {
        OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            parent.process_id,
        )
    };
    if process.is_null() {
        return parent_open_failure(parent.process_id, unsafe { GetLastError() });
    }
    let process = OwnedHandle(process);
    let actual = process_identity_from_handle(parent.process_id, process.0)?;
    if !process_identity_matches(parent, actual) {
        return Ok(());
    }
    let result = unsafe { WaitForSingleObject(process.0, INFINITE) };
    if result == u32::MAX {
        return Err(last_error(format!(
            "wait for Clipline process {}",
            parent.process_id
        )));
    }
    Ok(())
}

fn parent_open_failure(process_id: u32, error_code: u32) -> Result<(), String> {
    if error_code == ERROR_INVALID_PARAMETER {
        // The parent completed between ShellExecute returning and this child
        // reaching main, leaving no process to wait for.
        Ok(())
    } else {
        Err(format!(
            "open Clipline process {process_id} for handoff: Windows error {error_code}"
        ))
    }
}

fn elevation_restart_parameters(parent: ProcessIdentity) -> String {
    format!(
        "{ELEVATED_AFTER_ARGUMENT} {} {}",
        parent.process_id, parent.creation_time
    )
}

fn query_process_identity(process_id: u32) -> Result<ProcessIdentity, String> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if process.is_null() {
        return Err(last_error(format!("open process {process_id}")));
    }
    let process = OwnedHandle(process);
    process_identity_from_handle(process_id, process.0)
}

fn process_identity_from_handle(
    process_id: u32,
    process: HANDLE,
) -> Result<ProcessIdentity, String> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    if unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) } == 0 {
        return Err(last_error(format!(
            "query process creation time {process_id}"
        )));
    }
    Ok(ProcessIdentity {
        process_id,
        creation_time: (u64::from(creation.dwHighDateTime) << 32)
            | u64::from(creation.dwLowDateTime),
    })
}

fn process_identity_matches(expected: ProcessIdentity, actual: ProcessIdentity) -> bool {
    expected == actual
}

pub(crate) fn wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

fn wide_null_checked(value: &OsStr, label: &str) -> Result<Vec<u16>, String> {
    let mut wide: Vec<_> = value.encode_wide().collect();
    if wide.contains(&0) {
        return Err(format!("{label} contains an embedded null"));
    }
    wide.push(0);
    Ok(wide)
}

pub(crate) fn last_os_error(action: &str) -> String {
    format!("{action}: {}", std::io::Error::last_os_error())
}

pub(crate) fn open_with_shell(target: &OsStr, context: &str) -> Result<(), String> {
    let operation = wide_null_checked(OsStr::new("open"), "shell operation")?;
    let target = wide_null_checked(target, "shell target")?;
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    classify_shell_result(result as isize, context)
}

fn classify_shell_result(result: isize, context: &str) -> Result<(), String> {
    if result <= 32 {
        Err(format!("{context} failed with shell code {result}"))
    } else {
        Ok(())
    }
}

pub(crate) fn available_space_bytes(path: &Path, context: &str) -> Result<u64, String> {
    let path = if path.as_os_str().is_empty() {
        OsStr::new(".")
    } else {
        path.as_os_str()
    };
    let wide = wide_null(path);
    let mut available = 0_u64;
    let result = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if result == 0 {
        return Err(last_os_error(context));
    }
    Ok(available)
}

pub(crate) fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    let from = wide_null(from.as_os_str());
    let to = wide_null(to.as_os_str());
    let result = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn last_error(context: String) -> String {
    let code = unsafe { GetLastError() };
    format!("{context}: Windows error {code}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elevated_restart_argument_round_trips_parent_process_instance() {
        let parent = ProcessIdentity {
            process_id: 4242,
            creation_time: 987_654_321,
        };
        let parameters = elevation_restart_parameters(parent);
        let args = ["clipline-app.exe", parameters.as_str()]
            .into_iter()
            .flat_map(str::split_whitespace);

        assert_eq!(elevation_parent_from_args(args).unwrap(), Some(parent));
    }

    #[test]
    fn recycled_parent_pid_does_not_match_original_process_instance() {
        let original = ProcessIdentity {
            process_id: 4242,
            creation_time: 100,
        };
        let recycled = ProcessIdentity {
            process_id: 4242,
            creation_time: 200,
        };

        assert!(!process_identity_matches(original, recycled));
    }

    #[test]
    fn ordinary_launch_has_no_elevation_parent() {
        assert_eq!(
            elevation_parent_from_args(["clipline-app.exe", "--autostart"]).unwrap(),
            None
        );
    }

    #[test]
    fn parent_open_failure_only_ignores_a_gone_process() {
        assert!(parent_open_failure(
            4242,
            windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER
        )
        .is_ok());
        assert!(
            parent_open_failure(4242, windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED).is_err()
        );
    }

    #[test]
    fn current_process_elevation_is_queryable() {
        current_process_is_elevated().expect("query this test process token");
    }

    #[test]
    fn current_process_instance_is_queryable() {
        let identity = query_process_identity(std::process::id())
            .expect("query this test process creation time");
        assert_eq!(identity.process_id, std::process::id());
        assert_ne!(identity.creation_time, 0);
    }

    #[test]
    fn wide_strings_are_null_terminated_and_checked_inputs_reject_interior_nulls() {
        assert_eq!(
            wide_null(OsStr::new("Clipline")),
            [67, 108, 105, 112, 108, 105, 110, 101, 0]
        );
        assert!(wide_null_checked(OsStr::new("bad\0target"), "target").is_err());
    }

    #[test]
    fn shell_result_codes_follow_the_win32_success_boundary() {
        assert!(classify_shell_result(33, "open target").is_ok());
        assert_eq!(
            classify_shell_result(32, "open target").unwrap_err(),
            "open target failed with shell code 32"
        );
    }
}
