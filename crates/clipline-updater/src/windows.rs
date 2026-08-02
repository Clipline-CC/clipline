//! Safe Windows passive-installer handoff.
//!
//! The child process is created suspended. This proves the executable can be launched without
//! letting installer code run before Clipline's durable shutdown acknowledgements complete.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use thiserror::Error;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    CreateProcessW, ResumeThread, TerminateProcess, WaitForSingleObject, CREATE_SUSPENDED,
    PROCESS_INFORMATION, STARTUPINFOW,
};

use crate::download::DownloadTelemetry;
use crate::{InstallerLauncher, PreparedInstallerHandoff, VerifiedInstaller};

const TERMINATE_WAIT_MS: u32 = 5_000;
const RESUME_FAILED: u32 = u32::MAX;

#[derive(Debug, Error)]
pub enum WindowsInstallerError {
    #[error("the verified installer path contains an embedded NUL")]
    EmbeddedNul,
    #[error("create the suspended passive installer process: {0}")]
    CreateProcess(#[source] windows::core::Error),
    #[error("resume the verified passive installer process: {0}")]
    ResumeProcess(#[source] windows::core::Error),
}

#[derive(Debug, Default)]
pub struct WindowsInstallerLauncher;

#[derive(Debug)]
pub struct PreparedWindowsInstaller {
    process: OwnedHandle,
    thread: OwnedHandle,
    process_id: u32,
    path: PathBuf,
    telemetry: DownloadTelemetry,
    armed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsInstallerReceipt {
    process_id: u32,
    retained_installer: PathBuf,
    telemetry: DownloadTelemetry,
}

impl WindowsInstallerReceipt {
    #[must_use]
    pub const fn process_id(&self) -> u32 {
        self.process_id
    }

    /// The invocation-owned installer remains until the next successful startup cleanup.
    /// This matches the prior updater's keep-on-success behavior and avoids deleting an image
    /// while the child still maps it.
    #[must_use]
    pub fn retained_installer(&self) -> &Path {
        &self.retained_installer
    }

    #[must_use]
    pub const fn telemetry(&self) -> &DownloadTelemetry {
        &self.telemetry
    }
}

impl InstallerLauncher for WindowsInstallerLauncher {
    type Prepared = PreparedWindowsInstaller;

    fn prepare(
        &mut self,
        installer: VerifiedInstaller,
    ) -> Result<Self::Prepared, WindowsInstallerError> {
        let (path, telemetry) = installer.transfer_cleanup();
        match create_suspended(&path, telemetry) {
            Ok(prepared) => Ok(prepared),
            Err(error) => {
                let _ = std::fs::remove_file(path);
                Err(error)
            }
        }
    }
}

impl PreparedInstallerHandoff for PreparedWindowsInstaller {
    type Receipt = WindowsInstallerReceipt;
    type Error = WindowsInstallerError;

    fn commit(mut self) -> Result<Self::Receipt, Self::Error> {
        let previous_suspend_count = unsafe { ResumeThread(self.thread.raw) };
        if previous_suspend_count == RESUME_FAILED {
            return Err(WindowsInstallerError::ResumeProcess(
                windows::core::Error::from_thread(),
            ));
        }

        self.armed = false;
        Ok(WindowsInstallerReceipt {
            process_id: self.process_id,
            retained_installer: self.path.clone(),
            telemetry: self.telemetry.clone(),
        })
    }
}

impl Drop for PreparedWindowsInstaller {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let _ = unsafe { TerminateProcess(self.process.raw, 1) };
        let _ =
            unsafe { WaitForSingleObject(self.process.raw, TERMINATE_WAIT_MS) } == WAIT_OBJECT_0;
        let _ = std::fs::remove_file(&self.path);
    }
}

fn create_suspended(
    path: &Path,
    telemetry: DownloadTelemetry,
) -> Result<PreparedWindowsInstaller, WindowsInstallerError> {
    let application = wide_nul(path.as_os_str())?;
    let mut command = wide_nul(OsStr::new(&installer_command_line(path)))?;
    let startup = STARTUPINFOW {
        cb: u32::try_from(std::mem::size_of::<STARTUPINFOW>()).expect("STARTUPINFOW fits u32"),
        ..Default::default()
    };
    let mut process_info = PROCESS_INFORMATION::default();

    unsafe {
        CreateProcessW(
            PCWSTR(application.as_ptr()),
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_SUSPENDED,
            None,
            PCWSTR::null(),
            &raw const startup,
            &raw mut process_info,
        )
    }
    .map_err(WindowsInstallerError::CreateProcess)?;

    // Both handles are non-null after a successful CreateProcessW call.
    Ok(PreparedWindowsInstaller {
        process: OwnedHandle::new(process_info.hProcess),
        thread: OwnedHandle::new(process_info.hThread),
        process_id: process_info.dwProcessId,
        path: path.to_path_buf(),
        telemetry,
        armed: true,
    })
}

fn installer_command_line(path: &Path) -> String {
    // These are the exact passive/restart/update arguments used by Tauri's NSIS updater. The
    // application path is also the argv[0] token expected by CreateProcessW.
    format!(
        "{} /P /R /UPDATE /ARGS",
        quote_windows_argument(path.as_os_str())
    )
}

fn quote_windows_argument(argument: &OsStr) -> String {
    let argument = argument.to_string_lossy();
    let mut quoted = String::from("\"");
    let mut backslashes = 0_usize;
    for character in argument.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                quoted.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                quoted.extend(std::iter::repeat_n('\\', backslashes));
                quoted.push(character);
                backslashes = 0;
            }
        }
    }
    quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
    quoted.push('"');
    quoted
}

fn wide_nul(value: &OsStr) -> Result<Vec<u16>, WindowsInstallerError> {
    let mut wide = value.encode_wide().collect::<Vec<_>>();
    if wide.contains(&0) {
        return Err(WindowsInstallerError::EmbeddedNul);
    }
    wide.push(0);
    Ok(wide)
}

#[derive(Debug)]
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
    fn command_line_uses_exact_passive_update_arguments() {
        let path = Path::new(r"C:\Temp Folder\Clipline_1.2.3_x64-setup.exe");
        assert_eq!(
            installer_command_line(path),
            r#""C:\Temp Folder\Clipline_1.2.3_x64-setup.exe" /P /R /UPDATE /ARGS"#
        );
    }

    #[test]
    fn quoting_preserves_trailing_slashes_and_quotes() {
        assert_eq!(
            quote_windows_argument(OsStr::new("C:\\odd path\\")),
            "\"C:\\odd path\\\\\""
        );
        assert_eq!(
            quote_windows_argument(OsStr::new("odd\\\"name")),
            "\"odd\\\\\\\"name\""
        );
    }
}
