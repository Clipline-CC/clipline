use super::*;

#[derive(serde::Deserialize)]
pub struct CopyClipToClipboardRequest {
    pub path: String,
    #[serde(default, rename = "audioTrackIds")]
    pub audio_track_ids: Option<Vec<String>>,
    #[serde(default)]
    pub original: bool,
}

#[tauri::command]
pub async fn copy_clip_to_clipboard(
    request: CopyClipToClipboardRequest,
    settings: tauri::State<'_, StorageSettings>,
    exports: tauri::State<'_, ClipboardExportState>,
    window: tauri::WebviewWindow,
) -> Result<(), String> {
    let target = validate_clip_path(&settings, &request.path)?;
    let audio_track_ids = request.audio_track_ids;
    let original = request.original;
    let job = exports.begin();
    let owner = window
        .hwnd()
        .map_err(|error| format!("get Clipline window handle: {error}"))?
        .0 as isize;
    tauri::async_runtime::spawn_blocking(move || {
        let share_path = clipboard_copy_path(&target, audio_track_ids.as_deref(), original, &job)?;
        job.ensure_active()?;
        copy_file_to_clipboard(&share_path, owner as HWND)
    })
    .await
    .map_err(|e| format!("copy clip task: {e}"))?
}

#[tauri::command]
pub async fn copy_text_to_clipboard(
    text: String,
    window: tauri::WebviewWindow,
) -> Result<(), String> {
    const MAX_CLIPBOARD_TEXT_BYTES: usize = 64 * 1024;
    if text.len() > MAX_CLIPBOARD_TEXT_BYTES {
        return Err("clipboard text exceeds 64 KiB".into());
    }
    if text.contains('\0') {
        return Err("clipboard text contains a null character".into());
    }
    let owner = window
        .hwnd()
        .map_err(|error| format!("get Clipline window handle: {error}"))?
        .0 as isize;
    tauri::async_runtime::spawn_blocking(move || {
        copy_text_to_clipboard_native(&text, owner as HWND)
    })
    .await
    .map_err(|error| format!("copy text task: {error}"))?
}

pub(crate) fn copy_file_to_clipboard(path: &Path, owner: HWND) -> Result<(), String> {
    let payload = dropfiles_payload(path);
    copy_payload_to_clipboard(&payload, CF_HDROP as u32, owner, false)
}

pub(crate) fn copy_text_to_clipboard_native(text: &str, owner: HWND) -> Result<(), String> {
    let payload = clipboard_text_payload(text);
    copy_payload_to_clipboard(&payload, CF_UNICODETEXT as u32, owner, true)
}

pub(crate) fn copy_payload_to_clipboard(
    payload: &[u8],
    format: u32,
    owner: HWND,
    empty_first: bool,
) -> Result<(), String> {
    let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, payload.len()) };
    if handle.is_null() {
        return Err(last_os_error("allocate clipboard memory"));
    }

    let mem = unsafe { GlobalLock(handle) };
    if mem.is_null() {
        let err = last_os_error("lock clipboard memory");
        unsafe {
            GlobalFree(handle);
        }
        return Err(err);
    }
    unsafe {
        ptr::copy_nonoverlapping(payload.as_ptr(), mem.cast::<u8>(), payload.len());
        GlobalUnlock(handle);
    }

    let mut transfer = ClipboardTransfer::new(handle);
    clipboard_transaction(
        8,
        || {
            if unsafe { OpenClipboard(owner) } == 0 {
                Err(last_os_error("open clipboard"))
            } else {
                Ok(())
            }
        },
        || unsafe {
            CloseClipboard();
        },
        || {
            if empty_first && unsafe { EmptyClipboard() } == 0 {
                return Err(last_os_error("empty clipboard"));
            }
            if unsafe { SetClipboardData(format, transfer.handle()) }.is_null() {
                Err(last_os_error("set clipboard data"))
            } else {
                transfer.release();
                Ok(())
            }
        },
        || std::thread::sleep(Duration::from_millis(15)),
    )
}

pub(crate) fn clipboard_transaction<E>(
    attempts: usize,
    mut open: impl FnMut() -> Result<(), E>,
    mut close: impl FnMut(),
    mut set: impl FnMut() -> Result<(), E>,
    mut wait: impl FnMut(),
) -> Result<(), E> {
    let mut last_error = None;
    for attempt in 0..attempts.max(1) {
        match open() {
            Ok(()) => {
                let result = set();
                close();
                return result;
            }
            Err(error) => {
                last_error = Some(error);
                if attempt + 1 < attempts.max(1) {
                    wait();
                }
            }
        }
    }
    Err(last_error.expect("at least one clipboard-open attempt runs"))
}

pub(crate) fn dropfiles_payload(path: &Path) -> Vec<u8> {
    let mut wide = shell_clipboard_path_wide(path);
    wide.extend([0, 0]);

    let header_len = size_of::<DROPFILES>();
    let byte_len = header_len + wide.len() * size_of::<u16>();
    let mut payload = vec![0u8; byte_len];
    let header = DROPFILES {
        pFiles: header_len as u32,
        pt: Default::default(),
        fNC: 0,
        fWide: 1,
    };
    unsafe {
        ptr::write_unaligned(payload.as_mut_ptr().cast::<DROPFILES>(), header);
        ptr::copy_nonoverlapping(
            wide.as_ptr().cast::<u8>(),
            payload.as_mut_ptr().add(header_len),
            wide.len() * size_of::<u16>(),
        );
    }
    payload
}

pub(crate) fn clipboard_text_payload(text: &str) -> Vec<u8> {
    crate::windows::wide_null(std::ffi::OsStr::new(text))
        .into_iter()
        .flat_map(u16::to_le_bytes)
        .collect()
}

pub(crate) fn shell_clipboard_path_wide(path: &Path) -> Vec<u16> {
    const BACKSLASH: u16 = b'\\' as u16;
    const QUESTION: u16 = b'?' as u16;
    const U: u16 = b'U' as u16;
    const N: u16 = b'N' as u16;
    const C: u16 = b'C' as u16;
    const VERBATIM: [u16; 4] = [BACKSLASH, BACKSLASH, QUESTION, BACKSLASH];
    const VERBATIM_UNC: [u16; 8] = [
        BACKSLASH, BACKSLASH, QUESTION, BACKSLASH, U, N, C, BACKSLASH,
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
}

impl ClipboardTransfer {
    fn new(handle: HGLOBAL) -> Self {
        Self { handle }
    }

    fn handle(&self) -> HANDLE {
        self.handle
    }

    fn release(&mut self) {
        self.handle = ptr::null_mut();
    }
}

impl Drop for ClipboardTransfer {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                GlobalFree(self.handle);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
        #[test]
        fn dropfiles_payload_strips_verbatim_prefix_and_marks_unicode() {
            let path = Path::new(r"\\?\C:\Users\dain\Videos\Clipline\clïp 雪.mp4");
            let payload = dropfiles_payload(path);
            let p_files = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;

            assert_eq!(p_files, size_of::<DROPFILES>());
            assert_eq!(i32::from_le_bytes(payload[12..16].try_into().unwrap()), 0);
            assert_eq!(i32::from_le_bytes(payload[16..20].try_into().unwrap()), 1);

            let path_units: Vec<u16> = payload[p_files..]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes(*pair))
                .collect();
            assert_eq!(&path_units[path_units.len() - 2..], &[0, 0]);
            let decoded = String::from_utf16(&path_units[..path_units.len() - 2]).unwrap();
            assert_eq!(decoded, r"C:\Users\dain\Videos\Clipline\clïp 雪.mp4");
        }
        #[test]
        fn clipboard_text_payload_is_null_terminated_utf16() {
            let payload = clipboard_text_payload("https://clipline.example/雪");
            let units: Vec<u16> = payload
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes(*pair))
                .collect();

            assert_eq!(units.last(), Some(&0));
            assert_eq!(
                String::from_utf16(&units[..units.len() - 1]).unwrap(),
                "https://clipline.example/雪"
            );
        }
        #[test]
        fn shell_clipboard_path_wide_converts_verbatim_unc_paths() {
            let path = Path::new(r"\\?\UNC\nas\clips\clïp 雪.mp4");
            let decoded = String::from_utf16(&shell_clipboard_path_wide(path)).unwrap();

            assert_eq!(decoded, r"\\nas\clips\clïp 雪.mp4");
        }
        #[test]
        fn clipboard_transaction_retries_open_and_closes_every_opened_path() {
            use std::cell::{Cell, RefCell};

            let events = RefCell::new(Vec::new());
            let opens = Cell::new(0_u32);
            let result = clipboard_transaction(
                3,
                || {
                    events.borrow_mut().push("open");
                    opens.set(opens.get() + 1);
                    if opens.get() < 3 {
                        Err("busy")
                    } else {
                        Ok(())
                    }
                },
                || events.borrow_mut().push("close"),
                || {
                    events.borrow_mut().push("set");
                    Ok(())
                },
                || events.borrow_mut().push("wait"),
            );
            assert_eq!(result, Ok(()));
            assert_eq!(
                events.into_inner(),
                vec!["open", "wait", "open", "wait", "open", "set", "close"]
            );

            let events = RefCell::new(Vec::new());
            let result = clipboard_transaction(
                1,
                || {
                    events.borrow_mut().push("open");
                    Ok::<(), &str>(())
                },
                || events.borrow_mut().push("close"),
                || {
                    events.borrow_mut().push("set");
                    Err("set")
                },
                || unreachable!(),
            );
            assert_eq!(result, Err("set"));
            assert_eq!(events.into_inner(), vec!["open", "set", "close"]);

            let closes = Cell::new(0);
            let result = clipboard_transaction(
                2,
                || Err::<(), _>("busy"),
                || closes.set(closes.get() + 1),
                || Ok(()),
                || {},
            );
            assert_eq!(result, Err("busy"));
            assert_eq!(
                closes.get(),
                0,
                "never close a clipboard that was not opened"
            );
        }
}
