//! Safe generic-credential access with one audited Windows allocation owner.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::ptr;

use thiserror::Error;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::ERROR_NOT_FOUND;
use windows::Win32::Security::Credentials::{
    CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE,
    CRED_TYPE_GENERIC,
};
use zeroize::{Zeroize, Zeroizing};

use crate::secret::SecretString;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WindowsCredentialError {
    #[error("credential target contains an embedded NUL")]
    TargetContainsNul,
    #[error("credential username contains an embedded NUL")]
    UsernameContainsNul,
    #[error("{value_label} is too large")]
    ValueTooLarge { value_label: &'static str },
    #[error("{action} {value_label}: Windows error {code}")]
    Native {
        action: &'static str,
        value_label: &'static str,
        code: i32,
    },
    #[error("read {value_label}: Windows returned a null credential")]
    NullCredential { value_label: &'static str },
    #[error("read {value_label}: Windows returned a null nonempty credential blob")]
    NullBlob { value_label: &'static str },
    #[error("{value_label} is not valid UTF-8")]
    InvalidUtf8 { value_label: &'static str },
}

#[derive(Clone, Copy)]
pub struct CredentialStore {
    value_label: &'static str,
}

impl CredentialStore {
    pub const fn new(value_label: &'static str) -> Self {
        Self { value_label }
    }

    pub fn write(
        self,
        target: &str,
        username: &str,
        value: &str,
    ) -> Result<(), WindowsCredentialError> {
        let mut target_w =
            nul_terminated(OsStr::new(target)).ok_or(WindowsCredentialError::TargetContainsNul)?;
        let mut username_w = nul_terminated(OsStr::new(username))
            .ok_or(WindowsCredentialError::UsernameContainsNul)?;
        let mut blob = Zeroizing::new(value.as_bytes().to_vec());
        let blob_len =
            u32::try_from(blob.len()).map_err(|_| WindowsCredentialError::ValueTooLarge {
                value_label: self.value_label,
            })?;
        let credential = CREDENTIALW {
            Flags: Default::default(),
            Type: CRED_TYPE_GENERIC,
            TargetName: PWSTR(target_w.as_mut_ptr()),
            Comment: PWSTR::null(),
            LastWritten: Default::default(),
            CredentialBlobSize: blob_len,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            AttributeCount: 0,
            Attributes: ptr::null_mut(),
            TargetAlias: PWSTR::null(),
            UserName: PWSTR(username_w.as_mut_ptr()),
        };
        unsafe { CredWriteW(&credential, 0) }.map_err(|error| self.native_error("store", error))
    }

    pub fn read(self, target: &str) -> Result<String, WindowsCredentialError> {
        self.read_secret(target)
            .map(|secret| secret.expose_secret().to_owned())
    }

    /// Read a credential into move-only zeroizing UTF-8 ownership.
    pub fn read_secret(self, target: &str) -> Result<SecretString, WindowsCredentialError> {
        let target_w =
            nul_terminated(OsStr::new(target)).ok_or(WindowsCredentialError::TargetContainsNul)?;
        let mut raw: *mut CREDENTIALW = ptr::null_mut();
        unsafe { CredReadW(PCWSTR(target_w.as_ptr()), CRED_TYPE_GENERIC, None, &mut raw) }
            .map_err(|error| self.native_error("read", error))?;
        if raw.is_null() {
            return Err(WindowsCredentialError::NullCredential {
                value_label: self.value_label,
            });
        }
        let _owner = OwnedCredential(raw);
        let credential = unsafe { &*raw };
        unsafe {
            decode_credential_blob(
                credential.CredentialBlob,
                credential.CredentialBlobSize,
                self.value_label,
            )
        }
    }

    /// Read an optional credential without turning absence into an error.
    pub fn read_secret_if_present(
        self,
        target: &str,
    ) -> Result<Option<SecretString>, WindowsCredentialError> {
        match self.read_secret(target) {
            Ok(secret) => Ok(Some(secret)),
            Err(WindowsCredentialError::Native { code, .. })
                if code == ERROR_NOT_FOUND.to_hresult().0 =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    pub fn delete_if_present(self, target: &str) -> Result<(), WindowsCredentialError> {
        let target_w =
            nul_terminated(OsStr::new(target)).ok_or(WindowsCredentialError::TargetContainsNul)?;
        match unsafe { CredDeleteW(PCWSTR(target_w.as_ptr()), CRED_TYPE_GENERIC, None) } {
            Ok(()) => Ok(()),
            Err(error) if error.code() == ERROR_NOT_FOUND.to_hresult() => Ok(()),
            Err(error) => Err(self.native_error("delete", error)),
        }
    }

    fn native_error(
        self,
        action: &'static str,
        error: windows::core::Error,
    ) -> WindowsCredentialError {
        WindowsCredentialError::Native {
            action,
            value_label: self.value_label,
            code: error.code().0,
        }
    }
}

fn nul_terminated(value: &OsStr) -> Option<Vec<u16>> {
    let mut wide: Vec<u16> = value.encode_wide().collect();
    if wide.contains(&0) {
        return None;
    }
    wide.push(0);
    Some(wide)
}

struct OwnedCredential(*mut CREDENTIALW);

impl Drop for OwnedCredential {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let credential = &mut *self.0;
                if !credential.CredentialBlob.is_null() && credential.CredentialBlobSize != 0 {
                    std::slice::from_raw_parts_mut(
                        credential.CredentialBlob,
                        credential.CredentialBlobSize as usize,
                    )
                    .zeroize();
                }
                CredFree(self.0.cast());
            }
        }
    }
}

unsafe fn decode_credential_blob(
    blob: *const u8,
    blob_len: u32,
    value_label: &'static str,
) -> Result<SecretString, WindowsCredentialError> {
    let mut bytes = if blob_len == 0 {
        Zeroizing::new(Vec::new())
    } else {
        if blob.is_null() {
            return Err(WindowsCredentialError::NullBlob { value_label });
        }
        Zeroizing::new(unsafe { std::slice::from_raw_parts(blob, blob_len as usize) }.to_vec())
    };
    match String::from_utf8(std::mem::take(&mut *bytes)) {
        Ok(value) => Ok(SecretString::from_zeroizing(Zeroizing::new(value))),
        Err(error) => {
            let _invalid = Zeroizing::new(error.into_bytes());
            Err(WindowsCredentialError::InvalidUtf8 { value_label })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_blob_decoder_accepts_empty_and_valid_utf8() {
        assert_eq!(
            unsafe { decode_credential_blob(ptr::null(), 0, "test secret") }
                .unwrap()
                .expose_secret(),
            ""
        );
        let value = b"secret";
        assert_eq!(
            unsafe { decode_credential_blob(value.as_ptr(), value.len() as u32, "test secret") }
                .unwrap()
                .expose_secret(),
            "secret"
        );
    }

    #[test]
    fn credential_blob_decoder_rejects_invalid_memory_and_utf8_without_secret_bytes() {
        assert_eq!(
            unsafe { decode_credential_blob(ptr::null(), 1, "cloud token") }.unwrap_err(),
            WindowsCredentialError::NullBlob {
                value_label: "cloud token"
            }
        );
        let invalid = [0xff];
        assert_eq!(
            unsafe { decode_credential_blob(invalid.as_ptr(), 1, "cloud token") }.unwrap_err(),
            WindowsCredentialError::InvalidUtf8 {
                value_label: "cloud token"
            }
        );
    }

    #[test]
    fn credential_crud_round_trips_secret_bytes_and_preserves_labels() {
        struct Cleanup {
            store: CredentialStore,
            target: String,
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = self.store.delete_if_present(&self.target);
            }
        }

        let store = CredentialStore::new("Clipline disposable test secret");
        let target = format!(
            "Clipline shell credential test {} {}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let cleanup = Cleanup {
            store,
            target: target.clone(),
        };
        store.delete_if_present(&target).unwrap();
        let secret = "secret bytes: snowman \u{2603}";
        store.write(&target, "Clipline test", secret).unwrap();
        assert_eq!(store.read_secret(&target).unwrap().expose_secret(), secret);
        store.delete_if_present(&target).unwrap();
        assert!(store.read_secret_if_present(&target).unwrap().is_none());
        let error = store.read(&target).unwrap_err().to_string();
        assert!(error.contains("read Clipline disposable test secret"));
        assert!(!error.contains(secret));
        drop(cleanup);
    }
}
