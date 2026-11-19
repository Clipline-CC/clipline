//! Windows Application Control (Smart App Control / WDAC) blocks on FFmpeg.
//!
//! Smart App Control refuses unsigned binaries, and the managed FFmpeg build
//! is not code-signed. When it blocks a DLL that `ffmpeg.exe` imports, the
//! child dies in the loader and its exit code is the NTSTATUS. When it blocks
//! `ffmpeg.exe` itself, `CreateProcess` fails with the matching Win32 error.
//! Both are distinct from a missing or corrupt runtime: redownloading can't
//! help, so they get their own typed error and user-facing message.
//!
//! Pure classification over raw codes, tested on every CI OS.

use std::io;
use std::process::ExitStatus;

/// Every user-facing block message starts with this. It deliberately never
/// contains "ffmpeg is not available", so the UI's install-and-retry
/// affordances don't match it.
pub const BLOCKED_MESSAGE_PREFIX: &str = "Windows blocked FFmpeg";

/// NTSTATUS codes a blocked image produces as a child's exit code. These are
/// only the codes whose `ntstatus.h` (SDK 10.0.26100.0) message says a file
/// was blocked or failed signature checks. Policy-management failures
/// (rollback, invalid/unsigned policy, too many policies, unauthorized
/// supplemental) mean something else and stay generic.
const BLOCKED_NTSTATUS: &[(u32, &str)] = &[
    (0xC000_0428, "STATUS_INVALID_IMAGE_HASH"),
    (0xC0E9_0002, "STATUS_SYSTEM_INTEGRITY_POLICY_VIOLATION"),
    (0xC0E9_0007, "STATUS_SYSTEM_INTEGRITY_REPUTATION_MALICIOUS"),
    (0xC0E9_0008, "STATUS_SYSTEM_INTEGRITY_REPUTATION_PUA"),
    (
        0xC0E9_0009,
        "STATUS_SYSTEM_INTEGRITY_REPUTATION_DANGEROUS_EXT",
    ),
    (0xC0E9_000A, "STATUS_SYSTEM_INTEGRITY_REPUTATION_OFFLINE"),
    (
        0xC0E9_000B,
        "STATUS_SYSTEM_INTEGRITY_REPUTATION_UNFRIENDLY_FILE",
    ),
    (
        0xC0E9_000C,
        "STATUS_SYSTEM_INTEGRITY_REPUTATION_UNATTAINABLE",
    ),
    (
        0xC0E9_000D,
        "STATUS_SYSTEM_INTEGRITY_REPUTATION_EXPLICIT_DENY_FILE",
    ),
    (0xC0E9_000E, "STATUS_SYSTEM_INTEGRITY_WHQL_NOT_SATISFIED"),
];

/// Win32 errors `CreateProcess` returns when the executable itself is
/// blocked (`winerror.h`, same selection rule as above).
const BLOCKED_WIN32: &[(u32, &str)] = &[
    (577, "ERROR_INVALID_IMAGE_HASH"),
    (4551, "ERROR_SYSTEM_INTEGRITY_POLICY_VIOLATION"),
    (4556, "ERROR_SYSTEM_INTEGRITY_REPUTATION_MALICIOUS"),
    (4557, "ERROR_SYSTEM_INTEGRITY_REPUTATION_PUA"),
    (4558, "ERROR_SYSTEM_INTEGRITY_REPUTATION_DANGEROUS_EXT"),
    (4559, "ERROR_SYSTEM_INTEGRITY_REPUTATION_OFFLINE"),
    (4580, "ERROR_SYSTEM_INTEGRITY_REPUTATION_UNFRIENDLY_FILE"),
    (4581, "ERROR_SYSTEM_INTEGRITY_REPUTATION_UNATTAINABLE"),
    (4582, "ERROR_SYSTEM_INTEGRITY_REPUTATION_EXPLICIT_DENY_FILE"),
    (4583, "ERROR_SYSTEM_INTEGRITY_WHQL_NOT_SATISFIED"),
];

/// Which Windows code space a block was reported in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockSource {
    /// The child started, then the loader refused an image; the exit code is
    /// the NTSTATUS.
    ChildExit,
    /// `CreateProcess` refused the executable; a Win32 error code.
    Spawn,
}

/// Windows Application Control refused to load FFmpeg (or one of its DLLs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AppControlBlock {
    source: BlockSource,
    code: u32,
    symbol: &'static str,
}

impl AppControlBlock {
    pub fn source(&self) -> BlockSource {
        self.source
    }

    /// The raw code: an NTSTATUS for [`BlockSource::ChildExit`], a Win32
    /// error for [`BlockSource::Spawn`].
    pub fn code(&self) -> u32 {
        self.code
    }

    /// The SDK header name for [`Self::code`].
    pub fn symbol(&self) -> &'static str {
        self.symbol
    }

    /// `0xC0E90002` for NTSTATUS; decimal Win32 errors as Windows prints them.
    pub fn code_display(&self) -> String {
        match self.source {
            BlockSource::ChildExit => format!("0x{:08X}", self.code),
            BlockSource::Spawn => self.code.to_string(),
        }
    }
}

impl std::fmt::Display for AppControlBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{BLOCKED_MESSAGE_PREFIX} (Smart App Control or an App Control policy): {} {}",
            self.symbol,
            self.code_display()
        )
    }
}

impl std::error::Error for AppControlBlock {}

fn lookup(table: &[(u32, &'static str)], code: u32) -> Option<&'static str> {
    table
        .iter()
        .find(|(known, _)| *known == code)
        .map(|(_, symbol)| *symbol)
}

/// Classify an `ExitStatus::code()` value. On Windows that is the process exit
/// code (`u32`) reinterpreted as `i32`, so an NTSTATUS such as `0xC0E90002`
/// arrives negative. Reinterpreting the bits back recovers it. On Unix exit
/// codes are 0..=255 and never match.
pub fn block_from_exit_code(code: Option<i32>) -> Option<AppControlBlock> {
    let code = code? as u32;
    lookup(BLOCKED_NTSTATUS, code).map(|symbol| AppControlBlock {
        source: BlockSource::ChildExit,
        code,
        symbol,
    })
}

pub fn block_from_exit_status(status: &ExitStatus) -> Option<AppControlBlock> {
    block_from_exit_code(status.code())
}

/// Classify a raw Win32 error from a failed spawn.
pub fn block_from_win32_error(code: i32) -> Option<AppControlBlock> {
    let code = u32::try_from(code).ok()?;
    lookup(BLOCKED_WIN32, code).map(|symbol| AppControlBlock {
        source: BlockSource::Spawn,
        code,
        symbol,
    })
}

/// Recognize a block in a spawn `io::Error`. That covers both the raw OS
/// error from `CreateProcess` and an error that already carries an
/// [`AppControlBlock`] (see [`into_io_error`]).
pub fn block_from_io_error(error: &io::Error) -> Option<AppControlBlock> {
    if let Some(block) = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<AppControlBlock>())
    {
        return Some(*block);
    }
    if cfg!(windows) {
        error.raw_os_error().and_then(block_from_win32_error)
    } else {
        None
    }
}

/// Wrap a block as an `io::Error` whose `Display` is the block message, so
/// string-reporting callers surface it verbatim and typed callers can still
/// recover it with [`block_from_io_error`].
pub fn into_io_error(block: AppControlBlock) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, block)
}

/// How an FFmpeg child ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildExit {
    Success,
    Blocked(AppControlBlock),
    Failed { code: Option<i32> },
}

pub fn classify_exit_code(code: Option<i32>) -> ChildExit {
    if code == Some(0) {
        return ChildExit::Success;
    }
    match block_from_exit_code(code) {
        Some(block) => ChildExit::Blocked(block),
        None => ChildExit::Failed { code },
    }
}

pub fn classify_exit_status(status: &ExitStatus) -> ChildExit {
    if status.success() {
        return ChildExit::Success;
    }
    classify_exit_code(status.code())
}

/// True for any user-facing message produced from an [`AppControlBlock`],
/// even after callers prefix it with context.
pub fn is_blocked_message(message: &str) -> bool {
    message
        .to_ascii_lowercase()
        .contains(&BLOCKED_MESSAGE_PREFIX.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ntstatus(code: u32) -> Option<i32> {
        // Exactly what `ExitStatus::code()` yields on Windows.
        Some(code as i32)
    }

    #[test]
    fn policy_violation_exit_code_is_recognized_despite_negative_i32() {
        let raw = ntstatus(0xC0E9_0002);
        assert!(raw.unwrap() < 0, "an NTSTATUS error arrives negative");
        let block = block_from_exit_code(raw).expect("blocked");
        assert_eq!(block.code(), 0xC0E9_0002);
        assert_eq!(block.symbol(), "STATUS_SYSTEM_INTEGRITY_POLICY_VIOLATION");
        assert_eq!(block.source(), BlockSource::ChildExit);
        assert_eq!(block.code_display(), "0xC0E90002");
    }

    #[test]
    fn invalid_image_hash_and_reputation_blocks_are_recognized() {
        for code in [0xC000_0428, 0xC0E9_0007, 0xC0E9_000A, 0xC0E9_000E] {
            assert!(block_from_exit_code(ntstatus(code)).is_some(), "{code:#X}");
        }
    }

    #[test]
    fn policy_management_and_ordinary_failures_are_not_blocks() {
        for code in [
            0xC0E9_0001, // rollback detected
            0xC0E9_0003, // invalid policy
            0xC0E9_0004, // policy not signed
            0xC0E9_0005, // too many policies
            0xC0E9_0006, // supplemental not authorized
            0xC000_0135, // STATUS_DLL_NOT_FOUND
            0xC000_007B, // STATUS_INVALID_IMAGE_FORMAT
            0xC000_0005, // access violation
        ] {
            assert!(block_from_exit_code(ntstatus(code)).is_none(), "{code:#X}");
        }
        for code in [Some(0), Some(1), Some(2), Some(255), Some(-1), None] {
            assert!(block_from_exit_code(code).is_none(), "{code:?}");
        }
    }

    #[test]
    fn exit_classification_distinguishes_success_block_and_failure() {
        assert_eq!(classify_exit_code(Some(0)), ChildExit::Success);
        assert!(matches!(
            classify_exit_code(ntstatus(0xC0E9_0002)),
            ChildExit::Blocked(_)
        ));
        assert_eq!(
            classify_exit_code(Some(1)),
            ChildExit::Failed { code: Some(1) }
        );
        assert_eq!(classify_exit_code(None), ChildExit::Failed { code: None });
    }

    #[test]
    fn spawn_win32_errors_are_recognized() {
        let block = block_from_win32_error(4551).expect("policy violation");
        assert_eq!(block.symbol(), "ERROR_SYSTEM_INTEGRITY_POLICY_VIOLATION");
        assert_eq!(block.source(), BlockSource::Spawn);
        assert_eq!(block.code_display(), "4551");
        for code in [577, 4556, 4559, 4580, 4583] {
            assert!(block_from_win32_error(code).is_some(), "{code}");
        }
        for code in [0, 2, 5, 193, 4550, 4552, 4555, 4560, -1] {
            assert!(block_from_win32_error(code).is_none(), "{code}");
        }
    }

    #[test]
    fn wrapped_block_round_trips_through_io_error() {
        let block = block_from_exit_code(ntstatus(0xC0E9_0002)).unwrap();
        let error = into_io_error(block);
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(block_from_io_error(&error), Some(block));
        assert_eq!(error.to_string(), block.to_string());
        assert!(block_from_io_error(&io::Error::other("nope")).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn raw_spawn_os_error_is_recognized_on_windows() {
        let error = io::Error::from_raw_os_error(4551);
        assert_eq!(block_from_io_error(&error).map(|b| b.code()), Some(4551));
    }

    #[test]
    fn message_is_distinct_from_the_missing_runtime_message() {
        let block = block_from_exit_code(ntstatus(0xC0E9_0002)).unwrap();
        let message = block.to_string();
        assert!(message.starts_with(BLOCKED_MESSAGE_PREFIX));
        assert!(message.contains("0xC0E90002"));
        assert!(
            !message
                .to_ascii_lowercase()
                .contains("ffmpeg is not available")
        );
        assert!(is_blocked_message(&format!(
            "spawn ffmpeg poster: {message}"
        )));
        assert!(!is_blocked_message(
            "ffmpeg is not available for poster extraction"
        ));
    }
}
