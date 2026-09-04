//! Clip library commands: inventory of the configured media folder for the UI and
//! a path-validated delete. The webview never touches the filesystem
//! directly — playback goes through the asset protocol.
//! Each domain lives in `library/<module>.rs`; this hub re-exports them so
//! existing `crate::library::*` paths keep resolving.

use std::collections::{hash_map::DefaultHasher, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::{self, Read, Write};
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::ptr;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex, OnceLock,
};
use std::time::{Duration, Instant};

use clipline_capture::{Codec, EncoderBackend};
use clipline_events::{is_review_event, ClipBookmark, ClipMarker, ClipMarkers, ClipPlay};
use clipline_mp4::{
    media_video_codecs_file, remux_with_mixed_audio_track_file,
    remux_with_selected_audio_tracks_file, trim_keyframe_aligned_file, MediaTrackCounts,
    MediaVideoCodec,
};
use clipline_storage::{
    remove_emptied_session_dir_after_clip, storage_status as read_storage_status,
};
use windows_sys::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::Ole::{CF_HDROP, CF_UNICODETEXT};
use windows_sys::Win32::UI::Shell::DROPFILES;

use tauri::{AppHandle, Manager, Runtime};

use crate::service::{clips_dir, default_clips_dir};
use crate::util;
use crate::windows::last_os_error;

#[path = "library/types.rs"]
mod types;
#[path = "library/scan.rs"]
mod scan;
#[path = "library/poster.rs"]
mod poster;
#[path = "library/delete.rs"]
mod delete;
#[path = "library/metadata.rs"]
mod metadata;
#[path = "library/rename.rs"]
mod rename;
#[path = "library/export_trim.rs"]
mod export_trim;
#[path = "library/audio_sidecars.rs"]
mod audio_sidecars;
#[path = "library/audio_publish.rs"]
mod audio_publish;
#[path = "library/audio_preview.rs"]
mod audio_preview;
#[path = "library/share.rs"]
mod share;
#[path = "library/status.rs"]
mod status;
#[path = "library/clipboard.rs"]
mod clipboard;
#[path = "library/compilation.rs"]
mod compilation;
#[path = "library/test_support.rs"]
#[cfg(test)]
mod test_support;
#[path = "library/naming.rs"]
mod naming;
#[path = "library/groups.rs"]
pub(crate) mod groups;

pub(crate) use audio_preview::*;
pub(crate) use audio_publish::*;
pub(crate) use audio_sidecars::*;
pub(crate) use clipboard::*;
pub(crate) use compilation::*;
pub(crate) use delete::*;
pub(crate) use export_trim::*;
pub(crate) use metadata::*;
pub(crate) use poster::*;
pub(crate) use rename::*;
pub(crate) use scan::*;
pub(crate) use share::*;
pub(crate) use status::*;
pub(crate) use types::*;

#[cfg(test)]
pub(crate) use test_support::*;

pub(crate) use clipline_capture::ffmpeg::suppress_console;
pub(crate) use naming::{
    inferred_clip_kind_for_path, is_reserved_windows_file_name, normalized_clip_file_name,
    normalized_clip_title,
};
