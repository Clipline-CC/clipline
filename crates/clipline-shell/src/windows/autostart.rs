use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use thiserror::Error;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegGetValueW, RegOpenKeyExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ, RRF_RT_REG_SZ,
};

const RUN_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const AUTOSTART_ARGUMENT: &str = "--autostart";
const MAX_REGISTRY_VALUE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WindowsAutostartError {
    #[error("autostart value name cannot be empty")]
    EmptyValueName,
    #[error("autostart value name contains an embedded NUL")]
    ValueNameContainsNul,
    #[error("autostart executable path contains an embedded NUL")]
    ExecutableContainsNul,
    #[error("autostart command is not valid UTF-16")]
    InvalidCommandUtf16,
    #[error("disposable autostart value already exists")]
    DisposableValueAlreadyExists,
    #[error("{operation} failed with Windows error {code}")]
    Native { operation: &'static str, code: u32 },
    #[error("autostart registry value is malformed: {0}")]
    MalformedValue(String),
    #[error("autostart registry write did not read back exactly")]
    VerificationFailed,
    #[error("autostart registry value changed concurrently; refusing to overwrite it")]
    ConcurrentChange,
    #[error("autostart transaction failed: {primary}; rollback failed: {rollback}")]
    Transaction { primary: String, rollback: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutostartChange {
    before: Option<Vec<u16>>,
    after: Option<Vec<u16>>,
    enabled: bool,
}

impl AutostartChange {
    pub fn enabled(&self) -> bool {
        self.enabled
    }
}

pub struct WindowsAutostartRegistration {
    key: RegistryKey,
    value_name: Vec<u16>,
    command: Vec<u16>,
    cleanup_on_drop: bool,
}

impl WindowsAutostartRegistration {
    pub fn new(value_name: &str, executable: &Path) -> Result<Self, WindowsAutostartError> {
        Self::open(value_name, executable, false)
    }

    /// Opens a test-only value name whose contents are deleted when this handle drops.
    ///
    /// The constructor refuses an existing value so cleanup can never remove state it did not
    /// create. Production code should use [`Self::new`].
    pub fn new_disposable(
        value_name: &str,
        executable: &Path,
    ) -> Result<Self, WindowsAutostartError> {
        let mut registration = Self::open(value_name, executable, false)?;
        if registration.read_raw()?.is_some() {
            return Err(WindowsAutostartError::DisposableValueAlreadyExists);
        }
        registration.cleanup_on_drop = true;
        Ok(registration)
    }

    fn open(
        value_name: &str,
        executable: &Path,
        cleanup_on_drop: bool,
    ) -> Result<Self, WindowsAutostartError> {
        if value_name.is_empty() {
            return Err(WindowsAutostartError::EmptyValueName);
        }
        let value_name = nul_terminated(OsStr::new(value_name))
            .ok_or(WindowsAutostartError::ValueNameContainsNul)?;
        let executable: Vec<u16> = executable.as_os_str().encode_wide().collect();
        if executable.contains(&0) {
            return Err(WindowsAutostartError::ExecutableContainsNul);
        }
        let command = build_autostart_command_utf16(&executable);

        Ok(Self {
            key: RegistryKey::open_current_user_run()?,
            value_name,
            command,
            cleanup_on_drop,
        })
    }

    pub fn is_enabled(&self) -> Result<bool, WindowsAutostartError> {
        Ok(self.read_raw()?.as_ref() == Some(&self.command))
    }

    pub fn raw_command(&self) -> Result<Option<String>, WindowsAutostartError> {
        self.read_raw()?
            .map(|value| {
                String::from_utf16(&value).map_err(|_| WindowsAutostartError::InvalidCommandUtf16)
            })
            .transpose()
    }

    pub fn set_enabled(&self, enabled: bool) -> Result<AutostartChange, WindowsAutostartError> {
        let before = self.read_raw()?;
        let after = enabled.then(|| self.command.clone());
        if let Err(primary) = self.write_and_verify(after.as_deref()) {
            let rollback = self.restore_failed_write(before.as_deref(), after.as_deref());
            return match rollback {
                Ok(()) => Err(primary),
                Err(rollback) => Err(WindowsAutostartError::Transaction {
                    primary: primary.to_string(),
                    rollback: rollback.to_string(),
                }),
            };
        }
        Ok(AutostartChange {
            before,
            after,
            enabled,
        })
    }

    pub fn rollback(&self, change: &AutostartChange) -> Result<(), WindowsAutostartError> {
        if self.read_raw()? != change.after {
            return Err(WindowsAutostartError::ConcurrentChange);
        }
        self.write_and_verify(change.before.as_deref())
    }

    fn restore_failed_write(
        &self,
        before: Option<&[u16]>,
        attempted: Option<&[u16]>,
    ) -> Result<(), WindowsAutostartError> {
        let current = self.read_raw()?;
        if current.as_deref() == before {
            return Ok(());
        }
        if current.as_deref() != attempted {
            return Err(WindowsAutostartError::ConcurrentChange);
        }
        self.write_and_verify(before)
    }

    fn write_and_verify(&self, value: Option<&[u16]>) -> Result<(), WindowsAutostartError> {
        match value {
            Some(value) => self.write_raw(value)?,
            None => self.delete_raw()?,
        }
        if self.read_raw()?.as_deref() == value {
            Ok(())
        } else {
            Err(WindowsAutostartError::VerificationFailed)
        }
    }

    fn read_raw(&self) -> Result<Option<Vec<u16>>, WindowsAutostartError> {
        let mut bytes = vec![0_u8; MAX_REGISTRY_VALUE_BYTES];
        let mut byte_count = u32::try_from(bytes.len()).expect("registry value bound fits u32");
        let status = unsafe {
            RegGetValueW(
                self.key.raw,
                PCWSTR::null(),
                PCWSTR(self.value_name.as_ptr()),
                RRF_RT_REG_SZ,
                None,
                Some(bytes.as_mut_ptr().cast()),
                Some(&mut byte_count),
            )
        };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        ensure_success("read HKCU Run value", status)?;
        let byte_count = usize::try_from(byte_count).map_err(|_| {
            WindowsAutostartError::MalformedValue("byte count does not fit usize".into())
        })?;
        if byte_count > bytes.len() || byte_count < 2 || byte_count % 2 != 0 {
            return Err(WindowsAutostartError::MalformedValue(format!(
                "invalid UTF-16 byte count {byte_count}"
            )));
        }
        let mut value: Vec<u16> = bytes[..byte_count]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        if value.pop() != Some(0) {
            return Err(WindowsAutostartError::MalformedValue(
                "REG_SZ is not NUL terminated".into(),
            ));
        }
        Ok(Some(value))
    }

    fn write_raw(&self, value: &[u16]) -> Result<(), WindowsAutostartError> {
        let mut terminated = Vec::with_capacity(value.len().saturating_add(1));
        terminated.extend_from_slice(value);
        terminated.push(0);
        let bytes: Vec<u8> = terminated
            .iter()
            .flat_map(|unit| unit.to_le_bytes())
            .collect();
        if bytes.len() > MAX_REGISTRY_VALUE_BYTES {
            return Err(WindowsAutostartError::MalformedValue(format!(
                "value exceeds {MAX_REGISTRY_VALUE_BYTES} bytes"
            )));
        }
        let status = unsafe {
            RegSetValueExW(
                self.key.raw,
                PCWSTR(self.value_name.as_ptr()),
                None,
                REG_SZ,
                Some(&bytes),
            )
        };
        ensure_success("write HKCU Run value", status)
    }

    fn delete_raw(&self) -> Result<(), WindowsAutostartError> {
        let status = unsafe { RegDeleteValueW(self.key.raw, PCWSTR(self.value_name.as_ptr())) };
        if status == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            ensure_success("delete HKCU Run value", status)
        }
    }
}

impl Drop for WindowsAutostartRegistration {
    fn drop(&mut self) {
        if self.cleanup_on_drop {
            let _ = self.delete_raw();
        }
    }
}

pub fn build_autostart_command(executable: &Path) -> Result<String, WindowsAutostartError> {
    let executable: Vec<u16> = executable.as_os_str().encode_wide().collect();
    if executable.contains(&0) {
        return Err(WindowsAutostartError::ExecutableContainsNul);
    }
    String::from_utf16(&build_autostart_command_utf16(&executable))
        .map_err(|_| WindowsAutostartError::InvalidCommandUtf16)
}

fn build_autostart_command_utf16(executable: &[u16]) -> Vec<u16> {
    let mut command = Vec::with_capacity(executable.len().saturating_add(20));
    command.push(u16::from(b'"'));
    let mut slashes = 0_usize;
    for &unit in executable {
        if unit == u16::from(b'\\') {
            slashes = slashes.saturating_add(1);
            continue;
        }
        if unit == u16::from(b'"') {
            command.extend(std::iter::repeat_n(u16::from(b'\\'), slashes * 2 + 1));
        } else {
            command.extend(std::iter::repeat_n(u16::from(b'\\'), slashes));
        }
        slashes = 0;
        command.push(unit);
    }
    command.extend(std::iter::repeat_n(u16::from(b'\\'), slashes * 2));
    command.push(u16::from(b'"'));
    command.push(u16::from(b' '));
    command.extend(AUTOSTART_ARGUMENT.encode_utf16());
    command
}

fn nul_terminated(value: &OsStr) -> Option<Vec<u16>> {
    let mut value: Vec<u16> = value.encode_wide().collect();
    if value.contains(&0) {
        return None;
    }
    value.push(0);
    Some(value)
}

fn ensure_success(
    operation: &'static str,
    status: WIN32_ERROR,
) -> Result<(), WindowsAutostartError> {
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(WindowsAutostartError::Native {
            operation,
            code: status.0,
        })
    }
}

struct RegistryKey {
    raw: HKEY,
}

impl RegistryKey {
    fn open_current_user_run() -> Result<Self, WindowsAutostartError> {
        let path =
            nul_terminated(OsStr::new(RUN_KEY_PATH)).expect("static HKCU Run key path has no NUL");
        let mut raw = HKEY::default();
        let status = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(path.as_ptr()),
                None,
                KEY_QUERY_VALUE | KEY_SET_VALUE,
                &mut raw,
            )
        };
        ensure_success("open HKCU Run key", status)?;
        Ok(Self { raw })
    }
}

impl Drop for RegistryKey {
    fn drop(&mut self) {
        let _ = unsafe { RegCloseKey(self.raw) };
    }
}
