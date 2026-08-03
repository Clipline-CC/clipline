//! Safe file-list clipboard transfer with one audited Windows allocation owner.

use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::thread;
use std::time::Duration;

use thiserror::Error;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{CF_HDROP, CF_UNICODETEXT};
use windows::Win32::UI::Shell::DROPFILES;

const OPEN_ATTEMPTS: usize = 8;
const OPEN_RETRY_DELAY: Duration = Duration::from_millis(15);
const MAX_CLIPBOARD_TEXT_UTF16_UNITS: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WindowsClipboardError {
    #[error("clipboard file path contains an embedded NUL")]
    PathContainsNul,
    #[error("clipboard text contains an embedded NUL")]
    TextContainsNul,
    #[error("clipboard file-list payload is too large")]
    PayloadTooLarge,
    #[error("{operation}: {message}")]
    Native {
        operation: &'static str,
        message: String,
    },
}

/// Copies one file path to the Windows clipboard as a Unicode `CF_HDROP` list.
///
/// `owner_window` is the raw HWND value supplied by the frontend. Zero requests an unowned
/// clipboard open. The function retries only the bounded clipboard-open contention step.
pub fn copy_file_to_clipboard(
    path: &Path,
    owner_window: isize,
) -> Result<(), WindowsClipboardError> {
    let payload = dropfiles_payload(path)?;
    copy_payload_to_clipboard(&payload, CF_HDROP.0, owner_window)
}

/// Copies bounded Unicode text to the Windows clipboard as `CF_UNICODETEXT`.
///
/// `owner_window` follows [`copy_file_to_clipboard`]. Embedded NULs are
/// rejected because Windows treats the first one as the end of the payload.
pub fn copy_text_to_clipboard(
    text: &str,
    owner_window: isize,
) -> Result<(), WindowsClipboardError> {
    let payload = unicode_text_payload(text)?;
    copy_payload_to_clipboard(&payload, CF_UNICODETEXT.0, owner_window)
}

fn copy_payload_to_clipboard(
    payload: &[u8],
    format: u16,
    owner_window: isize,
) -> Result<(), WindowsClipboardError> {
    let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, payload.len()) }
        .map_err(|error| native_error("allocate clipboard memory", error))?;
    let mut transfer = ClipboardTransfer::new(handle);

    let memory = unsafe { GlobalLock(handle) };
    if memory.is_null() {
        return Err(native_error(
            "lock clipboard memory",
            windows::core::Error::from_thread(),
        ));
    }
    unsafe {
        std::ptr::copy_nonoverlapping(payload.as_ptr(), memory.cast::<u8>(), payload.len());
        // A zero return can mean that the final lock was released successfully. No later step
        // depends on the generated Result, so the ownership invariant is what matters here.
        let _ = GlobalUnlock(handle);
    }

    let owner = if owner_window == 0 {
        None
    } else {
        Some(HWND(owner_window as *mut _))
    };
    let mut last_open_error = None;
    for attempt in 0..OPEN_ATTEMPTS {
        match unsafe { OpenClipboard(owner) } {
            Ok(()) => {
                let _clipboard = OpenClipboardGuard;
                unsafe { EmptyClipboard() }
                    .map_err(|error| native_error("empty clipboard", error))?;
                let clipboard_handle = HANDLE(transfer.handle().0);
                unsafe { SetClipboardData(format.into(), Some(clipboard_handle)) }
                    .map_err(|error| native_error("set clipboard data", error))?;
                transfer.release();
                return Ok(());
            }
            Err(error) => {
                last_open_error = Some(error);
                if attempt + 1 < OPEN_ATTEMPTS {
                    thread::sleep(OPEN_RETRY_DELAY);
                }
            }
        }
    }

    Err(native_error(
        "open clipboard after 8 attempts",
        last_open_error.expect("the bounded clipboard-open loop always runs"),
    ))
}

fn native_error(operation: &'static str, error: windows::core::Error) -> WindowsClipboardError {
    WindowsClipboardError::Native {
        operation,
        message: error.to_string(),
    }
}

fn dropfiles_payload(path: &Path) -> Result<Vec<u8>, WindowsClipboardError> {
    let mut wide = shell_clipboard_path_wide(path);
    if wide.contains(&0) {
        return Err(WindowsClipboardError::PathContainsNul);
    }
    wide.extend([0, 0]);

    let header_len = size_of::<DROPFILES>();
    let path_bytes = wide
        .len()
        .checked_mul(size_of::<u16>())
        .ok_or(WindowsClipboardError::PayloadTooLarge)?;
    let byte_len = header_len
        .checked_add(path_bytes)
        .ok_or(WindowsClipboardError::PayloadTooLarge)?;
    let mut payload = vec![0_u8; byte_len];

    payload[0..4].copy_from_slice(
        &u32::try_from(header_len)
            .map_err(|_| WindowsClipboardError::PayloadTooLarge)?
            .to_le_bytes(),
    );
    // DROPFILES.pt and DROPFILES.fNC remain zero. fWide is the final BOOL in the packed header.
    payload[header_len - size_of::<i32>()..header_len].copy_from_slice(&1_i32.to_le_bytes());
    for (index, unit) in wide.into_iter().enumerate() {
        let offset = header_len + index * size_of::<u16>();
        payload[offset..offset + size_of::<u16>()].copy_from_slice(&unit.to_le_bytes());
    }
    Ok(payload)
}

fn unicode_text_payload(text: &str) -> Result<Vec<u8>, WindowsClipboardError> {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.contains(&0) {
        return Err(WindowsClipboardError::TextContainsNul);
    }
    if units.len() > MAX_CLIPBOARD_TEXT_UTF16_UNITS {
        return Err(WindowsClipboardError::PayloadTooLarge);
    }
    let unit_count = units
        .len()
        .checked_add(1)
        .ok_or(WindowsClipboardError::PayloadTooLarge)?;
    let byte_len = unit_count
        .checked_mul(size_of::<u16>())
        .ok_or(WindowsClipboardError::PayloadTooLarge)?;
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(byte_len)
        .map_err(|_| WindowsClipboardError::PayloadTooLarge)?;
    for unit in units.into_iter().chain(std::iter::once(0)) {
        payload.extend_from_slice(&unit.to_le_bytes());
    }
    Ok(payload)
}

fn shell_clipboard_path_wide(path: &Path) -> Vec<u16> {
    const BACKSLASH: u16 = b'\\' as u16;
    const QUESTION: u16 = b'?' as u16;
    const VERBATIM: [u16; 4] = [BACKSLASH, BACKSLASH, QUESTION, BACKSLASH];
    const VERBATIM_UNC: [u16; 8] = [
        BACKSLASH,
        BACKSLASH,
        QUESTION,
        BACKSLASH,
        b'U' as u16,
        b'N' as u16,
        b'C' as u16,
        BACKSLASH,
    ];

    let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.starts_with(&VERBATIM_UNC) {
        let mut plain = vec![BACKSLASH, BACKSLASH];
        plain.extend_from_slice(&wide[VERBATIM_UNC.len()..]);
        plain
    } else if wide.starts_with(&VERBATIM) {
        wide[VERBATIM.len()..].to_vec()
    } else {
        wide
    }
}

struct ClipboardTransfer {
    handle: HGLOBAL,
    transferred: bool,
}

impl ClipboardTransfer {
    fn new(handle: HGLOBAL) -> Self {
        Self {
            handle,
            transferred: false,
        }
    }

    fn handle(&self) -> HGLOBAL {
        self.handle
    }

    fn release(&mut self) {
        debug_assert!(!self.transferred, "clipboard allocation transferred twice");
        self.transferred = true;
    }
}

impl Drop for ClipboardTransfer {
    fn drop(&mut self) {
        if !self.transferred {
            // GlobalFree returns null on success; the generated Result is intentionally ignored.
            unsafe {
                let _ = GlobalFree(Some(self.handle));
            }
        }
    }
}

struct OpenClipboardGuard;

impl Drop for OpenClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::ffi::OsStr;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    #[test]
    fn dropfiles_payload_strips_verbatim_prefix_and_marks_unicode() {
        let path = Path::new(r"\\?\C:\Users\dain\Videos\Clipline\clip-snow.mp4");
        let payload = dropfiles_payload(path).unwrap();
        let p_files = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;

        assert_eq!(p_files, size_of::<DROPFILES>());
        assert_eq!(i32::from_le_bytes(payload[12..16].try_into().unwrap()), 0);
        assert_eq!(i32::from_le_bytes(payload[16..20].try_into().unwrap()), 1);
        let units: Vec<u16> = payload[p_files..]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes(pair.try_into().unwrap()))
            .collect();
        assert_eq!(&units[units.len() - 2..], &[0, 0]);
        assert_eq!(
            String::from_utf16(&units[..units.len() - 2]).unwrap(),
            r"C:\Users\dain\Videos\Clipline\clip-snow.mp4"
        );
    }

    #[test]
    fn clipboard_path_preserves_unc_and_rejects_embedded_nul() {
        let unc = Path::new(r"\\?\UNC\nas\clips\clip.mp4");
        assert_eq!(
            String::from_utf16(&shell_clipboard_path_wide(unc)).unwrap(),
            r"\\nas\clips\clip.mp4"
        );

        let path = Path::new(OsStr::new("clip\0injected.mp4"));
        assert_eq!(
            dropfiles_payload(path),
            Err(WindowsClipboardError::PathContainsNul)
        );
    }

    #[test]
    fn unicode_text_payload_is_nul_terminated_bounded_and_rejects_embedded_nul() {
        let payload = unicode_text_payload("https://clips.example/c/snowman-☃").unwrap();
        let units: Vec<u16> = payload
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes(pair.try_into().unwrap()))
            .collect();
        assert_eq!(units.last(), Some(&0));
        assert_eq!(
            String::from_utf16(&units[..units.len() - 1]).unwrap(),
            "https://clips.example/c/snowman-☃"
        );
        assert_eq!(
            unicode_text_payload("https://clips.example/c/bad\0suffix"),
            Err(WindowsClipboardError::TextContainsNul)
        );
        assert_eq!(
            unicode_text_payload(&"x".repeat(MAX_CLIPBOARD_TEXT_UTF16_UNITS + 1)),
            Err(WindowsClipboardError::PayloadTooLarge)
        );
    }

    #[test]
    fn transaction_retries_only_open_and_closes_after_transfer_failure_or_panic() {
        fn transaction<E>(
            attempts: usize,
            mut open: impl FnMut() -> Result<(), E>,
            mut close: impl FnMut(),
            mut transfer: impl FnMut() -> Result<(), E>,
            mut wait: impl FnMut(),
        ) -> Result<(), E> {
            struct CloseOnDrop<'a, F: FnMut()>(&'a mut F);
            impl<F: FnMut()> Drop for CloseOnDrop<'_, F> {
                fn drop(&mut self) {
                    (self.0)();
                }
            }

            let mut last = None;
            for attempt in 0..attempts.max(1) {
                match open() {
                    Ok(()) => {
                        let _close = CloseOnDrop(&mut close);
                        return transfer();
                    }
                    Err(error) => {
                        last = Some(error);
                        if attempt + 1 < attempts.max(1) {
                            wait();
                        }
                    }
                }
            }
            Err(last.expect("at least one attempt"))
        }

        let events = RefCell::new(Vec::new());
        let opens = Cell::new(0);
        let transfers = Cell::new(0);
        let result = transaction(
            OPEN_ATTEMPTS,
            || {
                events.borrow_mut().push("open");
                opens.set(opens.get() + 1);
                if opens.get() < OPEN_ATTEMPTS {
                    Err("busy")
                } else {
                    Ok(())
                }
            },
            || events.borrow_mut().push("close"),
            || {
                transfers.set(transfers.get() + 1);
                events.borrow_mut().push("transfer");
                Err("set")
            },
            || events.borrow_mut().push("wait"),
        );
        assert_eq!(result, Err("set"));
        assert_eq!(opens.get(), OPEN_ATTEMPTS);
        assert_eq!(transfers.get(), 1);
        assert_eq!(events.borrow().last(), Some(&"close"));

        let closes = Cell::new(0);
        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _ = transaction(
                1,
                || Ok::<(), ()>(()),
                || closes.set(closes.get() + 1),
                || panic!("transfer panic"),
                || {},
            );
        }));
        assert!(panic.is_err());
        assert_eq!(closes.get(), 1);
    }
}
