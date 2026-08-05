//! Safe wrappers for the remaining app-local Win32 filesystem and WebView surface.

mod webview_memory;

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

pub(crate) use webview_memory::{set_memory_target, MemoryTarget};
use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

pub fn current_process_is_elevated() -> Result<bool, String> {
    clipline_shell::windows::process::current_process_is_elevated()
        .map_err(|error| error.to_string())
}

pub fn process_is_elevated(process_id: u32) -> Result<bool, String> {
    clipline_shell::windows::process::process_is_elevated(process_id)
        .map_err(|error| error.to_string())
}

pub fn process_instance_id(process_id: u32) -> Result<String, String> {
    clipline_shell::windows::process::process_instance_id(process_id)
        .map_err(|error| error.to_string())
}

pub fn launch_elevated_after(parent_process_id: u32) -> Result<(), String> {
    clipline_shell::windows::process::launch_elevated_after(parent_process_id)
        .map_err(|error| error.to_string())
}

pub(crate) fn wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

pub(crate) fn last_os_error(action: &str) -> String {
    format!("{action}: {}", std::io::Error::last_os_error())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_services_are_reached_through_the_safe_shell_boundary() {
        current_process_is_elevated().expect("query this test process token");
        let instance =
            process_instance_id(std::process::id()).expect("query this test process creation time");
        assert!(instance.starts_with(&format!("{}:", std::process::id())));
    }

    #[test]
    fn app_local_wide_string_helper_remains_null_terminated() {
        assert_eq!(
            wide_null(OsStr::new("Clipline")),
            [67, 108, 105, 112, 108, 105, 110, 101, 0]
        );
    }
}
