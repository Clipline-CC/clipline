//! Process error mode: keep Windows from raising modal hard-error dialogs.
//!
//! Smart App Control or a WDAC policy can refuse an FFmpeg DLL. Under the
//! default error mode, every ffmpeg child then raises a modal "Bad Image"
//! loader dialog, and startup probing alone launches about a dozen. The error
//! mode is inherited by child processes (std `Command` never passes
//! `CREATE_DEFAULT_ERROR_MODE`), so setting it once here makes a blocked child
//! exit silently with the NTSTATUS as its exit code, which
//! `clipline_capture::app_control` then classifies.
//!
//! Only `SEM_FAILCRITICALERRORS` is added. `SEM_NOGPFAULTERRORBOX` would also
//! turn off Windows Error Reporting for Clipline and every child, losing crash
//! reports and dumps, and it does nothing for loader errors.

use windows_sys::Win32::System::Diagnostics::Debug::{
    GetErrorMode, SetErrorMode, SEM_FAILCRITICALERRORS,
};

/// OR `SEM_FAILCRITICALERRORS` into this process's error mode, keeping any
/// bits already set, and return the mode now in effect. Call it first thing
/// in `main`, before any child process is spawned.
pub(crate) fn suppress_critical_error_dialogs() -> u32 {
    let current = current_error_mode();
    let wanted = current | SEM_FAILCRITICALERRORS;
    if wanted != current {
        // SAFETY: takes and returns plain flags; it only changes this
        // process's error mode, which children inherit at creation.
        unsafe {
            SetErrorMode(wanted);
        }
    }
    current_error_mode()
}

pub(crate) fn current_error_mode() -> u32 {
    // SAFETY: no arguments; reads this process's error mode.
    unsafe { GetErrorMode() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    const MODE_ENV: &str = "CLIPLINE_TEST_ERROR_MODE";
    const TEST_NAME: &str = "windows::error_mode::tests::children_inherit_failcriticalerrors";
    const CREATE_DEFAULT_ERROR_MODE: u32 = 0x0400_0000;
    const SEM_NOOPENFILEERRORBOX: u32 = 0x8000;

    fn helper(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(MODE_ENV, mode);
        command
    }

    fn reported_mode(mut command: Command) -> u32 {
        let status = command.status().expect("spawn error-mode reporter");
        status.code().expect("reporter exit code") as u32
    }

    /// Process isolation: the error mode is process-wide, so it is only ever
    /// changed in a child of the test runner, never in the runner itself.
    #[test]
    fn children_inherit_failcriticalerrors() {
        match std::env::var(MODE_ENV).as_deref() {
            // Grandchild: report the error mode this process started with.
            Ok("report") => std::process::exit(current_error_mode() as i32),
            // Child: set an unrelated bit first, apply the app's setting, then
            // check that it kept that bit and that grandchildren inherit it.
            Ok("apply") => {
                // SAFETY: plain integer in/out; affects only this helper process.
                unsafe {
                    windows_sys::Win32::System::Diagnostics::Debug::SetErrorMode(
                        SEM_NOOPENFILEERRORBOX,
                    );
                }
                let mode = suppress_critical_error_dialogs();
                assert_eq!(mode, SEM_NOOPENFILEERRORBOX | SEM_FAILCRITICALERRORS);

                // Spawned exactly like an ffmpeg child.
                let mut inherited = helper("report");
                clipline_capture::ffmpeg::suppress_console(&mut inherited);
                let inherited = reported_mode(inherited);
                assert_ne!(inherited & SEM_FAILCRITICALERRORS, 0, "mode {inherited:#x}");

                // Control: opting out of inheritance really drops the bit, so
                // the reporter is not trivially always set.
                let mut fresh = helper("report");
                std::os::windows::process::CommandExt::creation_flags(
                    &mut fresh,
                    CREATE_DEFAULT_ERROR_MODE,
                );
                let fresh = reported_mode(fresh);
                assert_eq!(fresh & SEM_FAILCRITICALERRORS, 0, "mode {fresh:#x}");
                std::process::exit(0);
            }
            _ => {
                let status = helper("apply").status().expect("spawn error-mode child");
                assert!(status.success(), "error-mode inheritance check failed: {status}");
            }
        }
    }
}
