//! The replay-buffer service: a dedicated recorder thread (ddoc §3 — the
//! pipeline is a synchronous pull loop on its own thread) talking to the
//! shell over channels. No Tauri types in here.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clipline_capture::ffmpeg;
use clipline_capture::ffmpeg_encoder::FfmpegVideoEncoder;
use clipline_capture::probe::{
    rank_encoders, EncoderApi, EncoderBackend, EncoderCandidate, EncoderCapability,
    EncoderPreference,
};
use clipline_capture::traits::{
    AudioSource, CaptureEngine, CaptureError, Encoder, Frame, FrameData,
};
use clipline_capture::windows::nv12::CropRect;
use clipline_capture::windows::wasapi::{
    enumerate_output_processes, process_loopback_available, AudioProcessInfo, WasapiChannelMode,
};
use clipline_capture::windows::{
    d3d11, find_window_by_title, mft_probe, window_from_raw_handle, DxgiDuplicationCapture,
    ID3D11Device, MftConfig, MftH264Encoder, SoftwareMftH264Encoder, WasapiLoopback, WgcCapture,
};
use clipline_capture::{
    even_dimensions, PipelineError, Recorder, RelativeClock, ReplayStorageConfig,
};
use clipline_events::{is_review_event, ClipAudioTrack, EventKind, MarkerLog, PlayerSummary};
use clipline_lol::LeagueQueue;
use clipline_storage::{
    clip_ownership_marker_path, enforce_quota_with_protection, ensure_clip_owned,
    recover_recording_files, remove_clip_ownership_marker, storage_status, StorageStatus,
};
use clipline_storage::{session_label, SessionTracker};

use crate::markers::PollerMsg;
use crate::util::{unix_now as unix_now_u64, unix_now_i64};

/// Re-exported so the app layer can name codecs without its own
/// clipline-capture import.
pub use clipline_capture::probe::Codec;

const LOW_REPLAY_CACHE_DISK_RESERVE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const REPLAY_SAVE_QUOTA_RESERVE_BYTES: u64 = 4 * 1024 * 1024;
const FULL_SESSION_QUOTA_RESERVE_BYTES: u64 = 64 * 1024 * 1024;
const REPLAY_CACHE_RUN_PREFIX: &str = "clipline-replay-cache-";
const REPLAY_CACHE_OWNER_FILE: &str = ".clipline-run.json";
const AMBIGUOUS_REPLAY_CACHE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
#[path = "service/media_root.rs"]
mod media_root;
pub enum Cmd {
    Save,
    /// Drop a user-placed bookmark on the recording timeline. The keypress
    /// instant travels with the command so command-queue latency cannot skew
    /// where the marker lands.
    Bookmark { pressed_at: Instant },
    StartFullSession,
    StopFullSession,
    Stop { announce: bool },
}

trait TimedFrameSource {
    fn next_frame_timeout(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError>;
}

impl TimedFrameSource for WgcCapture {
    fn next_frame_timeout(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError> {
        WgcCapture::next_frame_timeout(self, timeout)
    }
}

impl TimedFrameSource for DxgiDuplicationCapture {
    fn next_frame_timeout(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError> {
        DxgiDuplicationCapture::next_frame_timeout(self, timeout)
    }
}

/// The live screen-capture engine, chosen at recording start. WGC is the
/// default and the only per-window option; DXGI Desktop Duplication is the
/// opt-in borderless display/region backend (issue #42).
enum LiveBackend {
    Wgc(WgcCapture),
    Dxgi(DxgiDuplicationCapture),
}

impl LiveBackend {
    fn diagnostic_label(&self) -> &'static str {
        match self {
            Self::Wgc(_) => "windows_graphics_capture",
            Self::Dxgi(_) => "desktop_duplication",
        }
    }
}

impl TimedFrameSource for LiveBackend {
    fn next_frame_timeout(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError> {
        match self {
            LiveBackend::Wgc(cap) => cap.next_frame_timeout(timeout),
            LiveBackend::Dxgi(cap) => cap.next_frame_timeout(timeout),
        }
    }
}

/// WGC can go quiet when the captured image is unchanged. Keep the encoder
/// moving at the configured cadence by reusing the latest texture during
/// idle gaps, so GOP/keyframe spacing follows wall-clock time.
struct CadencedCapture<C> {
    inner: C,
    frame_interval: Duration,
    frame_interval_s: f64,
    last_data: Option<FrameData>,
    last_emit_pts_s: Option<f64>,
    next_pts_s: Option<f64>,
    last_emit_wall: Instant,
    retry_deadline: Option<Instant>,
}

impl<C> CadencedCapture<C> {
    fn new(inner: C, fps: u32, seed: Frame) -> Self {
        let frame_interval_s = 1.0 / fps.max(1) as f64;
        let seed_pts_s = seed.pts_s;
        Self {
            inner,
            frame_interval: Duration::from_secs_f64(frame_interval_s),
            frame_interval_s,
            last_data: Some(seed.data),
            last_emit_pts_s: Some(seed_pts_s),
            next_pts_s: Some(seed_pts_s + frame_interval_s),
            last_emit_wall: Instant::now(),
            retry_deadline: None,
        }
    }

    fn remember(&mut self, frame: &Frame) {
        let now = Instant::now();
        self.last_emit_wall = self
            .last_emit_pts_s
            .map(|last| frame.pts_s - last)
            .filter(|delta| delta.is_finite() && *delta >= 0.0)
            .and_then(|delta| {
                self.last_emit_wall
                    .checked_add(Duration::from_secs_f64(delta))
            })
            .map(|anchored| anchored.min(now))
            .unwrap_or(now);
        self.last_data = Some(frame.data.clone());
        self.last_emit_pts_s = Some(frame.pts_s);
        self.next_pts_s = Some(frame.pts_s + self.frame_interval_s);
    }
}

impl<C: TimedFrameSource> CaptureEngine for CadencedCapture<C> {
    fn next_frame(&mut self) -> Result<Option<Frame>, CaptureError> {
        let now = Instant::now();
        let wall_remaining = self
            .frame_interval
            .saturating_sub(now.saturating_duration_since(self.last_emit_wall));
        let retry_deadline = self.retry_deadline.take();
        let timeout = retry_deadline
            .map(|deadline| deadline.saturating_duration_since(now).min(wall_remaining))
            .unwrap_or(wall_remaining);
        match self.inner.next_frame_timeout(timeout) {
            Ok(Some(mut frame)) => {
                if let Some(next_pts_s) = self.next_pts_s {
                    if frame.pts_s < next_pts_s {
                        // A timeout duplicate already filled this cadence slot. Keep the
                        // newest texture, but yield to the service loop before reading again
                        // so stop/save commands remain responsive while a stale queue drains.
                        let pts_remaining = Duration::from_secs_f64(
                            (next_pts_s - frame.pts_s)
                                .min(self.frame_interval_s)
                                .max(0.0),
                        );
                        let now = Instant::now();
                        let wall_remaining = self
                            .frame_interval
                            .saturating_sub(now.saturating_duration_since(self.last_emit_wall));
                        let retry_after = pts_remaining.min(wall_remaining);
                        self.last_data = Some(frame.data);
                        self.retry_deadline = Some(now + retry_after);
                        return Err(CaptureError::Timeout(retry_after));
                    }
                }
                if let Some(last) = self.last_emit_pts_s {
                    frame.pts_s = frame.pts_s.max(last + 1e-4);
                }
                self.remember(&frame);
                Ok(Some(frame))
            }
            Ok(None) => Ok(None),
            Err(CaptureError::Timeout(_)) => {
                let Some(data) = self.last_data.clone() else {
                    return Err(CaptureError::Timeout(self.frame_interval));
                };
                let now = Instant::now();
                let elapsed = now.saturating_duration_since(self.last_emit_wall);
                if elapsed < self.frame_interval {
                    // A capture backend may report a timeout before the duration it was
                    // asked to wait. Do not pay out a full video cadence slot until that
                    // slot's wall-clock deadline has actually arrived.
                    let wall_remaining = self.frame_interval - elapsed;
                    if retry_deadline.is_some_and(|deadline| deadline > now) {
                        self.retry_deadline = retry_deadline;
                    }
                    let retry_after = retry_deadline
                        .map(|deadline| deadline.saturating_duration_since(now))
                        .map_or(wall_remaining, |remaining| remaining.min(wall_remaining));
                    return Err(CaptureError::Timeout(retry_after));
                }
                let elapsed_intervals =
                    (elapsed.as_secs_f64() / self.frame_interval_s).floor() as u64;
                let intervals = elapsed_intervals.max(1);
                let skipped = intervals - 1;
                let min_pts = self.last_emit_pts_s.map(|last| last + 1e-4).unwrap_or(0.0);
                let pts_s = (self.next_pts_s.unwrap_or(min_pts)
                    + skipped as f64 * self.frame_interval_s)
                    .max(min_pts);
                self.last_emit_pts_s = Some(pts_s);
                self.next_pts_s = Some(pts_s + self.frame_interval_s);
                self.last_emit_wall +=
                    Duration::from_secs_f64(intervals as f64 * self.frame_interval_s);
                Ok(Some(Frame { pts_s, data }))
            }
            Err(e) => Err(e),
        }
    }
}

type LiveCapture = CadencedCapture<LiveBackend>;
type LiveRecorder = Recorder<LiveCapture, Box<dyn Encoder>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureRegion {
    pub display_id: Option<String>,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureSource {
    PrimaryMonitor,
    WindowTitle(String),
    WindowHandle { hwnd: isize, title: String },
    DisplayRegion(CaptureRegion),
}

/// Which screen-capture backend to use for display/region capture (issue #42).
/// `Auto` and `Wgc` both use Windows Graphics Capture today; `Wgc` is the
/// persisted force-WGC escape hatch that survives any future change to `Auto`.
/// `DesktopDuplication` uses DXGI Desktop Duplication, which has no Windows 10
/// privacy border but is display/region only (never per-window) and silently
/// falls back to WGC when it can't initialize (multi-GPU, rotated display, etc).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureBackend {
    #[default]
    Auto,
    Wgc,
    DesktopDuplication,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioChannelMode {
    #[default]
    Mono,
    Stereo,
}

/// The user's encoder choice. `Auto` prefers H.264 for playback compatibility while
/// respecting backend merit order within a codec; the explicit variants force a
/// (backend, codec) pair (still falling back through Auto if it can't open).
/// Legacy saved values (`auto`, `nvenc_h264`, `amf_h264`, `quick_sync_h264`)
/// still deserialize.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoEncoder {
    #[default]
    Auto,
    NvencH264,
    NvencHevc,
    NvencAv1,
    AmfH264,
    AmfHevc,
    AmfAv1,
    QuickSyncH264,
    QuickSyncHevc,
    QuickSyncAv1,
    SvtAv1,
}

impl VideoEncoder {
    fn preference(self) -> EncoderPreference {
        let (backend, codec) = match self {
            Self::Auto => return EncoderPreference::Auto,
            Self::NvencH264 => (EncoderBackend::Nvenc, Codec::H264),
            Self::NvencHevc => (EncoderBackend::Nvenc, Codec::Hevc),
            Self::NvencAv1 => (EncoderBackend::Nvenc, Codec::Av1),
            Self::AmfH264 => (EncoderBackend::Amf, Codec::H264),
            Self::AmfHevc => (EncoderBackend::Amf, Codec::Hevc),
            Self::AmfAv1 => (EncoderBackend::Amf, Codec::Av1),
            Self::QuickSyncH264 => (EncoderBackend::QuickSync, Codec::H264),
            Self::QuickSyncHevc => (EncoderBackend::QuickSync, Codec::Hevc),
            Self::QuickSyncAv1 => (EncoderBackend::QuickSync, Codec::Av1),
            Self::SvtAv1 => (EncoderBackend::SvtAv1, Codec::Av1),
        };
        EncoderPreference::Explicit { backend, codec }
    }

    /// The settings/serde id (snake_case). Kept in lockstep with the
    /// `serde(rename_all = "snake_case")` derive by a test.
    fn id(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::NvencH264 => "nvenc_h264",
            Self::NvencHevc => "nvenc_hevc",
            Self::NvencAv1 => "nvenc_av1",
            Self::AmfH264 => "amf_h264",
            Self::AmfHevc => "amf_hevc",
            Self::AmfAv1 => "amf_av1",
            Self::QuickSyncH264 => "quick_sync_h264",
            Self::QuickSyncHevc => "quick_sync_hevc",
            Self::QuickSyncAv1 => "quick_sync_av1",
            Self::SvtAv1 => "svt_av1",
        }
    }

    /// The explicit variant for a (backend, codec) pair, if Clipline exposes
    /// it as a user choice. `None` for combinations with no settings id
    /// (e.g. `MfSoftware`, or SvtAv1 paired with a non-AV1 codec).
    fn from_parts(backend: EncoderBackend, codec: Codec) -> Option<Self> {
        Some(match (backend, codec) {
            (EncoderBackend::Nvenc, Codec::H264) => Self::NvencH264,
            (EncoderBackend::Nvenc, Codec::Hevc) => Self::NvencHevc,
            (EncoderBackend::Nvenc, Codec::Av1) => Self::NvencAv1,
            (EncoderBackend::Amf, Codec::H264) => Self::AmfH264,
            (EncoderBackend::Amf, Codec::Hevc) => Self::AmfHevc,
            (EncoderBackend::Amf, Codec::Av1) => Self::AmfAv1,
            (EncoderBackend::QuickSync, Codec::H264) => Self::QuickSyncH264,
            (EncoderBackend::QuickSync, Codec::Hevc) => Self::QuickSyncHevc,
            (EncoderBackend::QuickSync, Codec::Av1) => Self::QuickSyncAv1,
            (EncoderBackend::SvtAv1, Codec::Av1) => Self::SvtAv1,
            _ => return None,
        })
    }
}

/// The settings id string for a codec, matching the frontend's decode-probe
/// keys ("h264"/"hevc"/"av1").
pub fn codec_id(codec: Codec) -> &'static str {
    match codec {
        Codec::Av1 => "av1",
        Codec::Hevc => "hevc",
        Codec::H264 => "h264",
    }
}

/// One selectable encoder for the Settings dropdown.
#[derive(serde::Serialize)]
pub struct EncoderOption {
    /// VideoEncoder settings id (e.g. "amf_hevc").
    pub id: String,
    /// Human label (e.g. "AMD AMF · HEVC").
    pub name: String,
    /// Codec key the frontend matches against its decode-capability probe.
    pub codec: String,
}

/// The encoders this machine can actually use, as Settings options. Dedupes
/// the same (backend, codec) offered by both MFT and FFmpeg, ordered by the
/// ddoc merit/preference order.
pub fn available_encoder_options() -> Vec<EncoderOption> {
    let mut seen = std::collections::BTreeSet::new();
    let mut options = Vec::new();
    let capabilities = encoder_capabilities();
    for cap in &capabilities {
        for &codec in &cap.codecs {
            let Some(encoder) = VideoEncoder::from_parts(cap.backend, codec) else {
                continue;
            };
            if !seen.insert(encoder.id()) {
                continue;
            }
            let candidate = EncoderCandidate {
                api: cap.api,
                backend: cap.backend,
                codec,
            };
            options.push(EncoderOption {
                id: encoder.id().to_string(),
                name: encoder_label(candidate),
                codec: codec_id(codec).to_string(),
            });
        }
    }
    options
}

/// A short, human-readable label for the active encoder, shown in the
/// sidebar status (e.g. "AMD AMF · H.264" or "Software · AV1").
pub fn encoder_label(candidate: EncoderCandidate) -> String {
    let backend = match candidate.backend {
        EncoderBackend::Nvenc => "NVIDIA NVENC",
        EncoderBackend::Amf => "AMD AMF",
        EncoderBackend::QuickSync => "Intel Quick Sync",
        EncoderBackend::SvtAv1 => "Software",
        EncoderBackend::MfSoftware => "Software",
    };
    let codec = match candidate.codec {
        Codec::Av1 => "AV1",
        Codec::Hevc => "HEVC",
        Codec::H264 => "H.264",
    };
    format!("{backend} · {codec}")
}

#[derive(Clone, Debug, PartialEq)]
pub struct AudioOptions {
    pub output_enabled: bool,
    pub output_device_id: Option<String>,
    pub output_volume: f64,
    pub split_output_by_process: bool,
    pub mic_enabled: bool,
    pub mic_device_id: Option<String>,
    pub mic_volume: f64,
    pub mic_channels: AudioChannelMode,
}

impl Default for AudioOptions {
    fn default() -> Self {
        Self {
            output_enabled: true,
            output_device_id: None,
            output_volume: 1.0,
            split_output_by_process: false,
            mic_enabled: false,
            mic_device_id: None,
            mic_volume: 1.0,
            mic_channels: AudioChannelMode::Mono,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ReplayStorageOptions {
    #[default]
    Memory,
    Disk {
        dir: PathBuf,
        quota_bytes: u64,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RecordingMode {
    FullSession,
    #[default]
    ReplaysOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputResolutionBounds {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum OutputResolution {
    #[default]
    #[serde(rename = "source")]
    Source,
    #[serde(rename = "1440p")]
    P1440,
    #[serde(rename = "1080p")]
    P1080,
    #[serde(rename = "720p")]
    P720,
    #[serde(rename = "480p")]
    P480,
}

impl OutputResolution {
    fn bounds(self) -> Option<(u32, u32)> {
        match self {
            Self::Source => None,
            Self::P1440 => Some((2560, 1440)),
            Self::P1080 => Some((1920, 1080)),
            Self::P720 => Some((1280, 720)),
            Self::P480 => Some((854, 480)),
        }
    }
}

#[derive(Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    MediaRootResolved {
        path: String,
        fell_back: bool,
    },
    Status {
        recording: bool,
        /// True when recording is armed but the games-only policy is waiting
        /// for an enabled game before starting capture.
        #[serde(default)]
        waiting_for_game: bool,
        segments: usize,
        buffered_s: f64,
        buffered_mb: f64,
        /// True while a full-session writer is active in addition to the replay ring.
        #[serde(default)]
        full_session: bool,
        /// Active encoder label (e.g. "AMD AMF · H.264"); empty when stopped.
        #[serde(default)]
        encoder: String,
        /// Actual capture backend after any automatic selection or fallback.
        #[serde(default)]
        capture_backend: String,
    },
    Saved {
        path: String,
        seconds: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recording_start_unix: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recording_end_unix: Option<i64>,
        markers: usize,
        #[serde(default)]
        full_session: bool,
        storage_total_bytes: u64,
        storage_quota_bytes: Option<u64>,
        storage_over_quota: bool,
    },
    StorageQuotaFull {
        total_bytes: u64,
        quota_bytes: u64,
        required_bytes: u64,
    },
    /// A user-placed bookmark landed on the recording timeline, `t_s` seconds
    /// from the start of this recorder run — the marker log's own origin, not
    /// an offset in any clip. `session_t_s` is that bookmark's position inside
    /// the full session being written, which is the only offset a user can
    /// already see; a replay bookmark has none, because where it falls depends
    /// on a save window that has not been chosen yet.
    BookmarkAdded {
        t_s: f64,
        session_t_s: Option<f64>,
    },
    /// Saved-media tree changed outside a normal save (auto-delete).
    LibraryChanged,
    Error {
        message: String,
    },
}

/// The game a recording run is attributed to (plugin or custom), recorded
/// alongside saved clips so the library can show its icon.
#[derive(Clone, Debug)]
pub struct ActiveGame {
    pub identity: crate::game_identity::GameIdentity,
    pub name: String,
    pub exe_path: Option<PathBuf>,
}

pub struct ServiceOptions {
    pub capture_source: CaptureSource,
    /// Screen-capture backend preference for display/region capture.
    pub capture_backend: CaptureBackend,
    /// Active built-in or custom game identity for policy and clip attribution.
    pub active_game: Option<ActiveGame>,
    /// Root folder for saved media.
    pub media_dir: PathBuf,
    /// Whether this run should recover leftover `.mp4.recording` files.
    /// Internal recorder restarts disable this to avoid stealing the previous
    /// recorder thread's active full-session temp file while it is shutting down.
    pub recover_abandoned_recordings: bool,
    /// Override the League Live Client endpoint (mock servers).
    pub lol_url: Option<String>,
    /// Save Replay trailing window (s).
    pub replay_window_s: f64,
    /// Ring budget in bytes.
    pub buffer_bytes: usize,
    /// Where the rolling replay buffer stores encoded GOP segments.
    pub replay_storage: ReplayStorageOptions,
    /// Saved-media quota. None disables quota blocking.
    pub disk_quota_bytes: Option<u64>,
    /// When the disk quota is full, delete the oldest saved clips to make room
    /// before stopping recording. Off by default.
    pub auto_delete_when_over_quota: bool,
    pub recording_mode: RecordingMode,
    pub fps: u32,
    pub bitrate_bps: u32,
    pub video_encoder: VideoEncoder,
    pub output_resolution: OutputResolution,
    pub output_resolution_bounds: Option<OutputResolutionBounds>,
    /// Codecs the in-app review player can decode. `Auto` is restricted to
    /// these so we never record a clip the user can't play back. The
    /// frontend reports the real set (canPlayType); H.264 is always safe.
    pub decodable_codecs: Vec<Codec>,
    pub audio: AudioOptions,
}

pub const DEFAULT_DISK_QUOTA_BYTES: u64 = 10 * 1024 * 1024 * 1024;

fn quota_would_be_exceeded(
    total_bytes: u64,
    quota_bytes: Option<u64>,
    required_bytes: u64,
) -> bool {
    quota_bytes.is_some_and(|quota| {
        total_bytes > quota || required_bytes > quota.saturating_sub(total_bytes)
    })
}

fn storage_quota_event_for_usage(
    total_bytes: u64,
    quota_bytes: Option<u64>,
    required_bytes: u64,
) -> Option<Event> {
    quota_would_be_exceeded(total_bytes, quota_bytes, required_bytes).then(|| {
        Event::StorageQuotaFull {
            total_bytes,
            quota_bytes: quota_bytes.expect("a quota is present when capacity is exceeded"),
            required_bytes,
        }
    })
}

fn storage_status_or_warn(clips_dir: &Path, quota_bytes: Option<u64>) -> Option<StorageStatus> {
    match storage_status(clips_dir, quota_bytes) {
        Ok(status) => Some(status),
        Err(error) => {
            tracing::warn!(
                event = "storage_quota_inspection_failed",
                path = ?clips_dir,
                error = %error,
            );
            None
        }
    }
}

/// Deletes oldest managed clips until `required_bytes` fit under the quota.
/// Active recordings and upload sources are never removed.
fn make_room_for_quota(
    events: &Sender<Event>,
    clips_dir: &Path,
    quota_bytes: Option<u64>,
    required_bytes: u64,
    protect: Option<&Path>,
) -> Option<(StorageStatus, usize)> {
    let Some(quota) = quota_bytes else {
        return storage_status_or_warn(clips_dir, None).map(|status| (status, 0));
    };
    let before_bytes = storage_status_or_warn(clips_dir, quota_bytes).map(|status| status.total_bytes);
    let target = quota.saturating_sub(required_bytes);
    let mut deleted_clips = match enforce_quota_with_protection(
        clips_dir,
        Some(target),
        protect,
        crate::cloud_upload::is_active_upload_source,
    ) {
        Ok(report) => report.deleted_clips,
        Err(error) => {
            tracing::warn!(
                event = "storage_quota_auto_delete_failed",
                path = ?clips_dir,
                error = %error,
            );
            0
        }
    };
    let status = storage_status_or_warn(clips_dir, quota_bytes)?;
    // A collector can remove an MP4 and then fail on a later sidecar/scan.
    // Compare the post-error inventory so LibraryChanged still fires.
    if deleted_clips == 0
        && before_bytes.is_some_and(|before| status.total_bytes < before)
    {
        deleted_clips = 1;
    }
    if deleted_clips > 0 {
        let _ = events.send(Event::LibraryChanged);
    }
    Some((status, deleted_clips))
}

fn storage_quota_full_event(
    events: &Sender<Event>,
    clips_dir: &Path,
    quota_bytes: Option<u64>,
    required_bytes: u64,
    auto_delete: bool,
) -> Option<Event> {
    let mut status = storage_status_or_warn(clips_dir, quota_bytes)?;
    if !quota_would_be_exceeded(status.total_bytes, quota_bytes, required_bytes) {
        return None;
    }
    if auto_delete {
        if let Some((cleaned, _)) =
            make_room_for_quota(events, clips_dir, quota_bytes, required_bytes, None)
        {
            status = cleaned;
        }
        if !quota_would_be_exceeded(status.total_bytes, quota_bytes, required_bytes) {
            return None;
        }
    }
    storage_quota_event_for_usage(status.total_bytes, quota_bytes, required_bytes)
}

impl Default for ServiceOptions {
    fn default() -> Self {
        Self {
            capture_source: CaptureSource::PrimaryMonitor,
            capture_backend: CaptureBackend::Auto,
            active_game: None,
            media_dir: default_clips_dir(),
            recover_abandoned_recordings: true,
            lol_url: None,
            replay_window_s: 60.0,
            // 60 s at 12 Mbps with 2x encoder-overshoot allowance.
            buffer_bytes: 180_000_000,
            replay_storage: ReplayStorageOptions::Memory,
            disk_quota_bytes: Some(DEFAULT_DISK_QUOTA_BYTES),
            auto_delete_when_over_quota: false,
            recording_mode: RecordingMode::ReplaysOnly,
            fps: 60,
            bitrate_bps: 12_000_000,
            video_encoder: VideoEncoder::Auto,
            output_resolution: OutputResolution::Source,
            output_resolution_bounds: None,
            decodable_codecs: vec![Codec::H264],
            audio: AudioOptions::default(),
        }
    }
}

pub fn spawn(opts: ServiceOptions) -> (Sender<Cmd>, Receiver<Event>) {
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (event_tx, event_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("clipline-recorder".into())
        .spawn(move || {
            if let Err(e) = run(opts, cmd_rx, &event_tx) {
                let _ = event_tx.send(Event::Error { message: e });
                send_stopped(&event_tx);
            }
        })
        .expect("spawn recorder thread");
    (cmd_tx, event_rx)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkerSourceKind {
    Plugin,
    LegacyLeaguePoller,
}

#[derive(Default)]
struct PlayerSummaryState {
    in_match: bool,
    active_replay: Option<PlayerSummary>,
    full_session: Option<PlayerSummary>,
}

impl PlayerSummaryState {
    fn match_started(&mut self) {
        self.in_match = true;
        self.active_replay = None;
        self.full_session = None;
    }

    fn update(&mut self, summary: PlayerSummary) {
        if self.in_match {
            self.active_replay = Some(summary.clone());
        }
        if self.in_match || self.full_session.is_some() {
            self.full_session = Some(summary);
        }
    }

    fn match_ended(&mut self) {
        self.in_match = false;
        self.active_replay = None;
    }

    fn active_replay_summary(&self) -> Option<&PlayerSummary> {
        self.active_replay.as_ref()
    }

    fn full_session_summary(&self) -> Option<&PlayerSummary> {
        self.active_replay.as_ref().or(self.full_session.as_ref())
    }
}

fn marker_source_kind(opts: &ServiceOptions) -> MarkerSourceKind {
    let plugin_id = opts
        .active_game
        .as_ref()
        .and_then(|game| game.identity.plugin_id());
    if crate::game_plugins::has_event_source(plugin_id) {
        MarkerSourceKind::Plugin
    } else {
        MarkerSourceKind::LegacyLeaguePoller
    }
}

fn spawn_marker_source(opts: &ServiceOptions, recording_t0: Instant) -> Receiver<PollerMsg> {
    let context = crate::game_plugins::GameEventSourceContext {
        lol_url: opts.lol_url.clone(),
        recording_t0,
        league_game_executable: opts
            .active_game
            .as_ref()
            .filter(|game| {
                game.identity.plugin_id() == Some(crate::game_plugins::LEAGUE_OF_LEGENDS_ID)
            })
            .and_then(|game| game.exe_path.clone()),
    };
    match marker_source_kind(opts) {
        MarkerSourceKind::Plugin => {
            let plugin_id = opts
                .active_game
                .as_ref()
                .and_then(|game| game.identity.plugin_id());
            crate::game_plugins::spawn_event_source(plugin_id, context)
                .expect("marker source kind checked plugin event source")
        }
        MarkerSourceKind::LegacyLeaguePoller => crate::markers::spawn(
            context.lol_url,
            context.recording_t0,
            context.league_game_executable,
        ),
    }
}

/// First-frame wait: the frame that fixes the capture size. Matches WGC's own
/// 5 s budget; an idle desktop can legitimately take this long to update.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(5);

/// Build the capture engine and pull its first frame (which fixes the capture
/// size). DXGI Desktop Duplication is attempted only when the user explicitly
/// selected it for a display/region source; any DXGI failure at construction
/// or on the first frame is logged as a diagnostic and silently falls back to
/// WGC, so recording always starts (the user chose silent fallback over a
/// warning).
fn open_screen_capture(
    device: &ID3D11Device,
    clock: RelativeClock,
    source: &CaptureSource,
    backend: CaptureBackend,
    events: &Sender<Event>,
) -> Result<(LiveBackend, Frame), String> {
    if backend == CaptureBackend::DesktopDuplication
        && matches!(
            source,
            CaptureSource::PrimaryMonitor | CaptureSource::DisplayRegion(_)
        )
    {
        match open_dxgi(device, clock, source, events) {
            Ok(pair) => return Ok(pair),
            Err(e) => tracing::warn!(
                event = "desktop_duplication_unavailable",
                error = %e,
                fallback = "windows_graphics_capture"
            ),
        }
    }

    let init = |e: &dyn std::fmt::Display| format!("init: {e}");
    let mut cap = open_wgc(device, clock, source, events)?;
    let first = cap
        .next_frame_timeout(FIRST_FRAME_TIMEOUT)
        .map_err(|e| init(&e))?
        .ok_or("capture ended before the first frame")?;
    Ok((LiveBackend::Wgc(cap), first))
}

/// DXGI Desktop Duplication for a display/region source (never per-window). The
/// monitor handle is resolved inside the capture crate / via `display`, both of
/// which use the `windows`-crate `HMONITOR` the constructors expect.
fn open_dxgi(
    device: &ID3D11Device,
    clock: RelativeClock,
    source: &CaptureSource,
    events: &Sender<Event>,
) -> Result<(LiveBackend, Frame), String> {
    let mut cap = match source {
        CaptureSource::PrimaryMonitor => {
            DxgiDuplicationCapture::primary_monitor_on(device.clone(), clock)
                .map_err(|e| e.to_string())?
        }
        CaptureSource::DisplayRegion(region) => {
            let (display, recovered) =
                clipline_capture::windows::display::display_handle_by_id_or_primary(
                    region.display_id.as_deref(),
                )
                .map_err(|e| e.to_string())?;
            let (crop, crop_recovered) =
                crop_for_region_or_full_display(region, &display.info, recovered)?;
            warn_capture_display_recovery(events, region, &display.info, recovered, crop_recovered);
            DxgiDuplicationCapture::for_monitor_region_on(
                device.clone(),
                display.handle,
                clock,
                crop,
            )
            .map_err(|e| e.to_string())?
        }
        // Window sources never reach here (guarded by open_screen_capture).
        _ => return Err("Desktop Duplication cannot capture a single window".into()),
    };
    let first = cap
        .next_frame_timeout(FIRST_FRAME_TIMEOUT)
        .map_err(|e| e.to_string())?
        .ok_or("Desktop Duplication ended before the first frame")?;
    Ok((LiveBackend::Dxgi(cap), first))
}

/// Windows Graphics Capture for any source (the default, and the only
/// per-window option).
fn open_wgc(
    device: &ID3D11Device,
    clock: RelativeClock,
    source: &CaptureSource,
    events: &Sender<Event>,
) -> Result<WgcCapture, String> {
    let init = |e: &dyn std::fmt::Display| format!("init: {e}");
    match source {
        CaptureSource::WindowTitle(needle) => {
            let hwnd = find_window_by_title(needle)
                .ok_or_else(|| format!("no visible window matching {needle:?}"))?;
            WgcCapture::for_window_client_on(device.clone(), hwnd, clock).map_err(|e| init(&e))
        }
        CaptureSource::WindowHandle { hwnd, title } => {
            let hwnd = window_from_raw_handle(*hwnd)
                .ok_or_else(|| format!("game window {title:?} is no longer available"))?;
            WgcCapture::for_window_client_on(device.clone(), hwnd, clock).map_err(|e| init(&e))
        }
        CaptureSource::PrimaryMonitor => {
            WgcCapture::primary_monitor_on(device.clone(), clock).map_err(|e| init(&e))
        }
        CaptureSource::DisplayRegion(region) => {
            let (display, recovered) =
                clipline_capture::windows::display::display_handle_by_id_or_primary(
                    region.display_id.as_deref(),
                )
                .map_err(|e| init(&e))?;
            let (crop, crop_recovered) =
                crop_for_region_or_full_display(region, &display.info, recovered)?;
            warn_capture_display_recovery(events, region, &display.info, recovered, crop_recovered);
            WgcCapture::for_monitor_region_on(device.clone(), display.handle, clock, crop)
                .map_err(|e| init(&e))
        }
    }
}

fn run(opts: ServiceOptions, cmd_rx: Receiver<Cmd>, events: &Sender<Event>) -> Result<(), String> {
    let (clips_dir, fell_back) = clips_dir_resolved(&opts.media_dir, default_clips_dir)?;
    let _ = events.send(Event::MediaRootResolved {
        path: clips_dir.display().to_string(),
        fell_back,
    });
    if fell_back {
        warn_user(
            events,
            format!(
                "media folder {:?} is unavailable; saving to {:?} instead",
                opts.media_dir, clips_dir
            ),
        );
    }
    if is_within_temp(&clips_dir, &std::env::temp_dir()) {
        warn_user(
            events,
            format!(
                "saving recordings to a temporary folder {clips_dir:?} that the system may delete; choose a Media folder in Settings"
            ),
        );
    }
    if opts.recover_abandoned_recordings {
        recover_abandoned_recordings(&clips_dir, events);
    }
    let startup_reserve = if opts.recording_mode == RecordingMode::FullSession {
        FULL_SESSION_QUOTA_RESERVE_BYTES
    } else {
        1
    };
    if let Some(event) = storage_quota_full_event(
        events,
        &clips_dir,
        opts.disk_quota_bytes,
        startup_reserve,
        opts.auto_delete_when_over_quota,
    ) {
        let _ = events.send(event);
        send_stopped(events);
        return Ok(());
    }
    let mut saved_media_baseline_bytes =
        storage_status_or_warn(&clips_dir, opts.disk_quota_bytes).map(|status| status.total_bytes);

    let init = |e: &dyn std::fmt::Display| format!("init: {e}");
    let (device, _ctx) = d3d11::create_device().map_err(|e| init(&e))?;
    let clock = WgcCapture::new_clock().map_err(|e| init(&e))?;
    // The wall-clock twin of the capture clock origin (both are QPC under
    // the hood; sampled together they describe one timeline — ddoc §5).
    let recording_t0 = Instant::now();
    let marker_rx = spawn_marker_source(&opts, recording_t0);
    let mut marker_log = MarkerLog::new();
    let mut player_summary = PlayerSummaryState::default();
    let mut league_queue: Option<LeagueQueue> = None;
    // Build the capture engine — DXGI Desktop Duplication when the user opted
    // in for a display/region source, else WGC — and pull the first frame,
    // which fixes the capture size. A DXGI failure (multi-GPU, rotated display,
    // secure desktop on the first frame, …) silently falls back to WGC.
    let (cap, first) = open_screen_capture(
        &device,
        clock,
        &opts.capture_source,
        opts.capture_backend,
        events,
    )?;
    let capture_backend_status = cap.diagnostic_label();
    // Output resolution caps scale down while preserving the captured aspect ratio.
    let FrameData::Gpu(tex) = &first.data else {
        return Err("expected a GPU frame".into());
    };
    let (in_w, in_h) = d3d11::texture_size(tex);
    let (enc_w, enc_h) = output_dimensions_with_bounds(
        in_w,
        in_h,
        opts.output_resolution,
        opts.output_resolution_bounds,
    );

    let (encoder, active) = build_encoder(&device, &opts, in_w, in_h, enc_w, enc_h, events)?;
    let encoder_status = encoder_label(active);
    // `encoder_label` intentionally shows only backend and codec, so an MFT and
    // an FFmpeg path render identically ("AMD AMF · H.264"). Log the API too:
    // the two have very different memory and readback behaviour, and telling
    // them apart from a support bundle was otherwise guesswork.
    tracing::info!(
        event = "encoder_selected",
        api = ?active.api,
        backend = ?active.backend,
        codec = ?active.codec,
        label = %encoder_status,
    );

    let mut prepared_replay = prepare_replay_storage(&opts)?;
    let replay_cache_dir = prepared_replay.run_dir.clone();
    let replay_storage = match &opts.replay_storage {
        ReplayStorageOptions::Memory => ReplayStorageConfig::Memory {
            max_bytes: opts.buffer_bytes,
            retention_s: opts.replay_window_s,
        },
        ReplayStorageOptions::Disk { .. } => ReplayStorageConfig::Disk {
            max_bytes: prepared_replay.max_bytes,
            retention_s: opts.replay_window_s,
            dir: replay_cache_dir
                .clone()
                .ok_or_else(|| "disk replay cache was not prepared".to_string())?,
        },
    };
    let cap = CadencedCapture::new(cap, opts.fps, first);
    let mut rec = Recorder::new_with_replay_storage(cap, encoder, replay_storage)
        .map_err(|e| format!("replay cache: {e}"))?;
    prepared_replay.disarm();
    let audio_tracks = audio_sources_from_options(clock, &opts.audio, events);
    let audio_track_metadata: Vec<ClipAudioTrack> = audio_tracks
        .iter()
        .map(|(_, track)| track.clone())
        .collect();
    for (audio, _) in audio_tracks {
        rec = rec.with_audio(audio);
    }
    // Saves land in a session folder: one per recorder run, with a dedicated
    // folder per detected match. Folders are created lazily at save time.
    let mut session = SessionTracker::new(local_session_label(false));
    let mut last_status = Instant::now();
    let mut full_session = begin_full_session_recording(
        &mut rec,
        &clips_dir,
        session.current(),
        opts.recording_mode,
        opts.active_game.as_ref(),
        events,
    );
    send_recording_status(
        events,
        &rec,
        &full_session,
        &encoder_status,
        capture_backend_status,
    );

    loop {
        match rec.step_with_frame(|_frame| {}) {
            Ok(true) => {}
            Ok(false) => break,
            // Idle screen: WGC delivers nothing — keep serving commands.
            Err(PipelineError::Capture(CaptureError::Timeout(_))) => {}
            Err(e) => {
                let primary = format!("recording: {e}");
                return Err(finalize_runtime_failure(primary, || {
                    shutdown_recorder(
                        &mut rec,
                        &mut full_session,
                        RecorderFinishContext {
                            marker_log: &marker_log,
                            player_summary: player_summary.full_session_summary(),
                            audio_tracks: &audio_track_metadata,
                            clips_dir: &clips_dir,
                            opts: &opts,
                            events,
                        },
                    )
                }));
            }
        }

        while let Ok(msg) = marker_rx.try_recv() {
            match msg {
                PollerMsg::Event(event) => {
                    // GameEnd means the match is over even while the Live
                    // Client API lingers; stop attributing saves to it.
                    if event.kind == EventKind::GameEnd {
                        player_summary.match_ended();
                        session.match_ended();
                        league_queue = None;
                    }
                    if is_review_event(&event) {
                        marker_log.push(event);
                    }
                }
                PollerMsg::PlayerSummary(summary) => player_summary.update(summary),
                PollerMsg::Queue(queue) => {
                    league_queue = Some(queue);
                    let replay_session_dir = clips_dir.join(session.current());
                    if replay_session_dir.is_dir() {
                        write_session_game_meta(
                            &replay_session_dir,
                            opts.active_game.as_ref(),
                            league_queue.as_ref(),
                        );
                    }
                    if let Some(full_session_dir) = full_session
                        .as_ref()
                        .and_then(|recording| recording.final_path.parent())
                    {
                        if full_session_dir != replay_session_dir {
                            write_session_game_meta(
                                full_session_dir,
                                opts.active_game.as_ref(),
                                league_queue.as_ref(),
                            );
                        }
                    }
                }
                PollerMsg::MatchStarted => {
                    league_queue = None;
                    player_summary.match_started();
                    session.match_started(local_session_label(true));
                }
                PollerMsg::MatchEnded => {
                    league_queue = None;
                    player_summary.match_ended();
                    session.match_ended();
                }
                PollerMsg::Heartbeat => {}
            }
        }

        if last_status.elapsed() >= Duration::from_secs(1) {
            last_status = Instant::now();
            if let Some(recording) = full_session.as_ref() {
                let check = full_session_quota_check(
                    events,
                    &clips_dir,
                    recording,
                    saved_media_baseline_bytes,
                    opts.disk_quota_bytes,
                    FULL_SESSION_QUOTA_RESERVE_BYTES,
                    opts.auto_delete_when_over_quota,
                );
                if let Some(baseline) = check.new_baseline_bytes {
                    saved_media_baseline_bytes = Some(baseline);
                }
                if let Some(event) = check.event {
                    let _ = events.send(event);
                    let _ = shutdown_recorder(
                        &mut rec,
                        &mut full_session,
                        RecorderFinishContext {
                            marker_log: &marker_log,
                            player_summary: player_summary.full_session_summary(),
                            audio_tracks: &audio_track_metadata,
                            clips_dir: &clips_dir,
                            opts: &opts,
                            events,
                        },
                    );
                    send_stopped(events);
                    return Ok(());
                }
            }
            if full_session.is_none() {
                if let Some((oldest_media_s, _)) =
                    rec.save_window_bounds(opts.replay_window_s, None)
                {
                    marker_log.retain_from_recording_offset(oldest_media_s);
                }
            }
            send_recording_status(
                events,
                &rec,
                &full_session,
                &encoder_status,
                capture_backend_status,
            );
            if replay_cache_dir.is_some() {
                if let Err(primary) = ensure_replay_cache_free_space(&opts) {
                    return Err(finalize_runtime_failure(primary, || {
                        shutdown_recorder(
                            &mut rec,
                            &mut full_session,
                            RecorderFinishContext {
                                marker_log: &marker_log,
                                player_summary: player_summary.full_session_summary(),
                                audio_tracks: &audio_track_metadata,
                                clips_dir: &clips_dir,
                                opts: &opts,
                                events,
                            },
                        )
                    }));
                }
            }
        }

        loop {
            match cmd_rx.try_recv() {
                Ok(Cmd::Save) => {
                    let replay_payload_bytes =
                        u64::try_from(rec.save_window_bytes(opts.replay_window_s, None))
                            .unwrap_or(u64::MAX);
                    if replay_payload_bytes > 0 {
                        let required_bytes =
                            replay_payload_bytes.saturating_add(REPLAY_SAVE_QUOTA_RESERVE_BYTES);
                        if let Some(event) = storage_quota_full_event(
                            events,
                            &clips_dir,
                            opts.disk_quota_bytes,
                            required_bytes,
                            opts.auto_delete_when_over_quota,
                        ) {
                            let _ = events.send(event);
                            let _ = shutdown_recorder(
                                &mut rec,
                                &mut full_session,
                                RecorderFinishContext {
                                    marker_log: &marker_log,
                                    player_summary: player_summary.full_session_summary(),
                                    audio_tracks: &audio_track_metadata,
                                    clips_dir: &clips_dir,
                                    opts: &opts,
                                    events,
                                },
                            );
                            send_stopped(events);
                            return Ok(());
                        }
                    }
                    let session_dir = clips_dir.join(session.current());
                    if let Err(e) = std::fs::create_dir_all(&session_dir) {
                        let _ = events.send(Event::Error {
                            message: format!("create session folder {session_dir:?}: {e}"),
                        });
                        continue;
                    }
                    write_session_game_meta(
                        &session_dir,
                        opts.active_game.as_ref(),
                        league_queue.as_ref(),
                    );
                    let path = unique_media_path(&session_dir, "clip");
                    match save(&rec, &path, opts.replay_window_s) {
                        Ok((end, seconds)) => {
                            // Markers and match summary ride along as a
                            // sidecar (ddoc §5) when either is available.
                            let markers = write_marker_sidecar(
                                events,
                                &marker_log,
                                &path,
                                end - seconds,
                                end,
                                player_summary.active_replay_summary(),
                                &audio_track_metadata,
                            );
                            let status = emit_saved_clip(
                                events,
                                &clips_dir,
                                &path,
                                seconds,
                                SavedClipMeta {
                                    markers,
                                    full_session: false,
                                    recording_start_unix: None,
                                    recording_end_unix: None,
                                },
                                &opts,
                            );
                            if status.clip_count > 0 {
                                match full_session.as_ref() {
                                    Some(recording) => {
                                        if let Ok(metadata) =
                                            std::fs::metadata(&recording.temp_path)
                                        {
                                            saved_media_baseline_bytes = Some(
                                                status.total_bytes.saturating_sub(metadata.len()),
                                            );
                                        }
                                    }
                                    None => {
                                        saved_media_baseline_bytes = Some(status.total_bytes);
                                    }
                                }
                            }
                            if status.is_over_quota() {
                                if let Some(event) = storage_quota_full_event(
                                    events,
                                    &clips_dir,
                                    opts.disk_quota_bytes,
                                    0,
                                    opts.auto_delete_when_over_quota,
                                ) {
                                    let _ = events.send(event);
                                }
                                let _ = shutdown_recorder(
                                    &mut rec,
                                    &mut full_session,
                                    RecorderFinishContext {
                                        marker_log: &marker_log,
                                        player_summary: player_summary.full_session_summary(),
                                        audio_tracks: &audio_track_metadata,
                                        clips_dir: &clips_dir,
                                        opts: &opts,
                                        events,
                                    },
                                );
                                send_stopped(events);
                                return Ok(());
                            }
                        }
                        Err(e) => {
                            let _ = events.send(Event::Error { message: e });
                            let _ = std::fs::remove_file(&path);
                        }
                    }
                }
                Ok(Cmd::Bookmark { pressed_at }) => {
                    // Anchored on `recording_t0`, the same origin every game
                    // marker offset uses, so both land on one timeline.
                    let t_s = pressed_at
                        .saturating_duration_since(recording_t0)
                        .as_secs_f64();
                    marker_log.push_bookmark(t_s);
                    // Re-based the way the sidecar will re-base it, so the
                    // confirmation names the offset review will show.
                    let session_t_s = rec
                        .full_session_start_s()
                        .map(|start_s| (t_s - start_s).max(0.0));
                    let _ = events.send(Event::BookmarkAdded { t_s, session_t_s });
                }
                Ok(Cmd::StartFullSession) => {
                    if full_session.is_none() {
                        if let Some(event) = storage_quota_full_event(
                            events,
                            &clips_dir,
                            opts.disk_quota_bytes,
                            FULL_SESSION_QUOTA_RESERVE_BYTES,
                            opts.auto_delete_when_over_quota,
                        ) {
                            let _ = events.send(event);
                            let _ = shutdown_recorder(
                                &mut rec,
                                &mut full_session,
                                RecorderFinishContext {
                                    marker_log: &marker_log,
                                    player_summary: player_summary.full_session_summary(),
                                    audio_tracks: &audio_track_metadata,
                                    clips_dir: &clips_dir,
                                    opts: &opts,
                                    events,
                                },
                            );
                            send_stopped(events);
                            return Ok(());
                        }
                        full_session = begin_full_session_recording(
                            &mut rec,
                            &clips_dir,
                            session.current(),
                            RecordingMode::FullSession,
                            opts.active_game.as_ref(),
                            events,
                        );
                        if let Some(recording_dir) = full_session
                            .as_ref()
                            .and_then(|recording| recording.final_path.parent())
                        {
                            write_session_game_meta(
                                recording_dir,
                                opts.active_game.as_ref(),
                                league_queue.as_ref(),
                            );
                        }
                    }
                    send_recording_status(
                        events,
                        &rec,
                        &full_session,
                        &encoder_status,
                        capture_backend_status,
                    );
                }
                Ok(Cmd::StopFullSession) => {
                    if let Some(status) = finish_full_session_recording(
                        &mut rec,
                        &mut full_session,
                        &RecorderFinishContext {
                            marker_log: &marker_log,
                            player_summary: player_summary.full_session_summary(),
                            audio_tracks: &audio_track_metadata,
                            clips_dir: &clips_dir,
                            opts: &opts,
                            events,
                        },
                    ) {
                        saved_media_baseline_bytes = Some(status.total_bytes);
                    }
                    send_recording_status(
                        events,
                        &rec,
                        &full_session,
                        &encoder_status,
                        capture_backend_status,
                    );
                }
                Ok(Cmd::Stop { announce }) => {
                    let _ = shutdown_recorder(
                        &mut rec,
                        &mut full_session,
                        RecorderFinishContext {
                            marker_log: &marker_log,
                            player_summary: player_summary.full_session_summary(),
                            audio_tracks: &audio_track_metadata,
                            clips_dir: &clips_dir,
                            opts: &opts,
                            events,
                        },
                    );
                    if announce {
                        send_stopped(events);
                    }
                    return Ok(());
                }
                Err(TryRecvError::Disconnected) => {
                    let _ = shutdown_recorder(
                        &mut rec,
                        &mut full_session,
                        RecorderFinishContext {
                            marker_log: &marker_log,
                            player_summary: player_summary.full_session_summary(),
                            audio_tracks: &audio_track_metadata,
                            clips_dir: &clips_dir,
                            opts: &opts,
                            events,
                        },
                    );
                    send_stopped(events);
                    return Ok(());
                }
                Err(TryRecvError::Empty) => break,
            }
        }
    }
    if let Some(err) = shutdown_recorder(
        &mut rec,
        &mut full_session,
        RecorderFinishContext {
            marker_log: &marker_log,
            player_summary: player_summary.full_session_summary(),
            audio_tracks: &audio_track_metadata,
            clips_dir: &clips_dir,
            opts: &opts,
            events,
        },
    ) {
        return Err(err);
    }
    send_stopped(events);
    Ok(())
}

fn audio_sources_from_options(
    clock: RelativeClock,
    options: &AudioOptions,
    events: &Sender<Event>,
) -> Vec<(Box<dyn AudioSource>, ClipAudioTrack)> {
    let mic_channels = match options.mic_channels {
        AudioChannelMode::Mono => WasapiChannelMode::Mono,
        AudioChannelMode::Stereo => WasapiChannelMode::Stereo,
    };

    let mut sources = Vec::<(Box<dyn AudioSource>, ClipAudioTrack)>::new();
    if options.output_enabled {
        add_output_audio_sources(clock, options, events, &mut sources);
    }
    if options.mic_enabled {
        match WasapiLoopback::start_microphone(
            clock,
            options.mic_device_id.as_deref(),
            options.mic_volume,
            mic_channels,
        ) {
            Ok(audio) => {
                let index = sources.len() as u32;
                sources.push((
                    Box::new(audio),
                    audio_track("microphone", index, "Microphone", "microphone"),
                ));
            }
            Err(e) => {
                warn_user(events, format!("microphone unavailable; continuing: {e}"));
            }
        }
    }
    sources
}

fn add_output_audio_sources(
    clock: RelativeClock,
    options: &AudioOptions,
    events: &Sender<Event>,
    sources: &mut Vec<(Box<dyn AudioSource>, ClipAudioTrack)>,
) {
    let mut process_tracks = Vec::new();
    let mut process_loopback_failed = false;
    let mut process_loopback_error = None::<String>;
    if options.split_output_by_process && process_loopback_available() {
        match enumerate_output_processes(options.output_device_id.as_deref()) {
            Ok(processes) => {
                for process in split_output_process_candidates(processes, std::process::id()) {
                    match WasapiLoopback::start_process_output(
                        clock,
                        process.pid,
                        options.output_volume,
                    ) {
                        Ok(audio) => process_tracks.push((process, audio)),
                        Err(e) if e.is_timeout() => {
                            process_loopback_failed = true;
                            process_loopback_error.get_or_insert_with(|| e.to_string());
                            break;
                        }
                        Err(e) => {
                            process_loopback_failed = true;
                            process_loopback_error
                                .get_or_insert_with(|| format!("{}: {e}", process.label));
                        }
                    }
                }
            }
            Err(e) => {
                process_loopback_failed = true;
                process_loopback_error = Some(e.to_string());
            }
        }
    }

    if process_loopback_failed {
        let detail = process_loopback_error
            .map(|error| format!(": {error}"))
            .unwrap_or_default();
        warn_user(
            events,
            format!("some app audio tracks unavailable; adding mixed output fallback{detail}"),
        );
    }

    let process_track_count = process_tracks.len();
    add_mixed_output_audio_source(clock, options, events, sources);

    if process_track_count > 0 {
        for (process, audio) in process_tracks {
            let index = sources.len() as u32;
            let id = format!("process:{}", process.pid);
            sources.push((
                Box::new(audio),
                audio_track(&id, index, &process.label, "process_output"),
            ));
        }
    }
}

fn split_output_process_candidates(
    processes: Vec<AudioProcessInfo>,
    own_pid: u32,
) -> Vec<AudioProcessInfo> {
    // Split process tracks should not include Clipline's own notification
    // sounds. The mixed Output Audio safety track remains raw speaker loopback.
    processes
        .into_iter()
        .filter(|process| process.pid != own_pid)
        .collect()
}

fn add_mixed_output_audio_source(
    clock: RelativeClock,
    options: &AudioOptions,
    events: &Sender<Event>,
    sources: &mut Vec<(Box<dyn AudioSource>, ClipAudioTrack)>,
) {
    match WasapiLoopback::start_output(
        clock,
        options.output_device_id.as_deref(),
        options.output_volume,
    ) {
        Ok(audio) => {
            let index = sources.len() as u32;
            sources.push((
                Box::new(audio),
                audio_track("output", index, "Output Audio", "output"),
            ));
        }
        Err(e) => {
            warn_user(events, format!("output audio unavailable; continuing: {e}"));
        }
    }
}

fn audio_track(id: &str, track_index: u32, label: &str, kind: &str) -> ClipAudioTrack {
    ClipAudioTrack {
        id: id.to_string(),
        track_index,
        label: label.to_string(),
        kind: Some(kind.to_string()),
    }
}

fn warn_user(events: &Sender<Event>, message: String) {
    let _ = events.send(Event::Error { message });
}

/// Process-static MFT capabilities (hardware probe is stable for the process).
fn mft_capabilities_cached() -> &'static [EncoderCapability] {
    use std::sync::OnceLock;
    static MFT_CAPS: OnceLock<Vec<EncoderCapability>> = OnceLock::new();
    MFT_CAPS.get_or_init(|| mft_probe::enumerate().unwrap_or_default())
}

#[derive(Debug, Clone)]
struct FfmpegCapabilitySlot {
    identity: String,
    caps: Vec<EncoderCapability>,
}

fn ffmpeg_caps_slot() -> &'static std::sync::Mutex<Option<FfmpegCapabilitySlot>> {
    use std::sync::{Mutex, OnceLock};
    static FFMPEG_CAPS: OnceLock<Mutex<Option<FfmpegCapabilitySlot>>> = OnceLock::new();
    FFMPEG_CAPS.get_or_init(|| Mutex::new(None))
}

fn ffmpeg_capability_slot(
    existing: Option<&FfmpegCapabilitySlot>,
    identity: &str,
    probe: impl FnOnce() -> Vec<EncoderCapability>,
) -> FfmpegCapabilitySlot {
    if let Some(existing) = existing {
        if existing.identity == identity {
            return existing.clone();
        }
    }
    FfmpegCapabilitySlot {
        identity: identity.to_string(),
        caps: probe(),
    }
}

/// Identity for the replaceable FFmpeg half: managed provenance, external path, or missing.
pub fn ffmpeg_capability_identity(
    managed: Option<&crate::ffmpeg_runtime::ManagedRuntimeInfo>,
    locate_path: Option<&std::path::Path>,
) -> String {
    if let Some(info) = managed {
        return format!("managed:{}:{}", info.dir.display(), info.manifest_sha256);
    }
    if let Some(path) = locate_path {
        return format!("external:{}", path.display());
    }
    "missing".to_string()
}

fn current_ffmpeg_capability_identity() -> String {
    let local = std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from);
    let managed_dir = local
        .as_ref()
        .map(|path| crate::ffmpeg_install::managed_root(path));
    let locate = ffmpeg::locate();
    let status =
        crate::ffmpeg_install::runtime_status_for_dirs(managed_dir.as_deref(), locate.as_deref())
            .unwrap_or_else(|_| crate::ffmpeg_runtime::FfmpegRuntimeStatus {
                kind: crate::ffmpeg_runtime::FfmpegDiscoveryKind::Missing,
                managed: None,
                locate_path: locate.clone(),
            });
    ffmpeg_capability_identity(status.managed.as_ref(), status.locate_path.as_deref())
}

fn ffmpeg_capabilities_cached() -> Vec<EncoderCapability> {
    let identity = current_ffmpeg_capability_identity();
    let slot_lock = ffmpeg_caps_slot();
    let mut guard = match slot_lock.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let next = ffmpeg_capability_slot(guard.as_ref(), &identity, ffmpeg::probe);
    *guard = Some(next.clone());
    next.caps
}

/// Drop the FFmpeg capability slot and re-probe against the current runtime identity.
pub fn refresh_ffmpeg_encoder_capabilities() -> Vec<EncoderOption> {
    {
        let slot_lock = ffmpeg_caps_slot();
        let mut guard = match slot_lock.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard = None;
    }
    available_encoder_options()
}

/// Combined MFT (process-static) + FFmpeg (replaceable/versioned) capabilities.
fn encoder_capabilities() -> Vec<EncoderCapability> {
    let mut caps = mft_capabilities_cached().to_vec();
    caps.extend(ffmpeg_capabilities_cached());
    caps
}

#[cfg(test)]
fn output_dimensions(in_w: u32, in_h: u32, resolution: OutputResolution) -> (u32, u32) {
    output_dimensions_with_bounds(in_w, in_h, resolution, None)
}

fn output_dimensions_with_bounds(
    in_w: u32,
    in_h: u32,
    resolution: OutputResolution,
    bounds: Option<OutputResolutionBounds>,
) -> (u32, u32) {
    let max_box = bounds
        .map(|bounds| (bounds.width, bounds.height))
        .or_else(|| resolution.bounds())
        .unwrap_or((2560, u32::MAX));
    let scale = (max_box.0 as f64 / in_w.max(1) as f64)
        .min(max_box.1 as f64 / in_h.max(1) as f64)
        .min(1.0);
    even_dimensions(
        (in_w as f64 * scale).round() as u32,
        (in_h as f64 * scale).round() as u32,
    )
}

/// Build the recorder's video encoder by walking the ranked candidate list
/// until one opens. Returns the boxed encoder and the candidate that won so
/// the caller can report it. Warns the user once if an explicit choice could
/// not be honored and Auto fallback was used instead.
#[allow(clippy::too_many_arguments)]
fn build_encoder(
    device: &ID3D11Device,
    opts: &ServiceOptions,
    in_w: u32,
    in_h: u32,
    enc_w: u32,
    enc_h: u32,
    events: &Sender<Event>,
) -> Result<(Box<dyn Encoder>, EncoderCandidate), String> {
    let preference = opts.video_encoder.preference();
    let capabilities = encoder_capabilities();
    let candidates = rank_encoders(&capabilities, &opts.decodable_codecs, preference);
    if candidates.is_empty() {
        return Err("init: no usable video encoder found on this system".into());
    }

    let explicit_target = match preference {
        EncoderPreference::Explicit { backend, codec } => Some((backend, codec)),
        EncoderPreference::Auto => None,
    };
    let ffmpeg_path = ffmpeg::locate();
    let mut last_err = String::new();
    for candidate in &candidates {
        match open_candidate(
            *candidate,
            device,
            opts,
            in_w,
            in_h,
            enc_w,
            enc_h,
            &ffmpeg_path,
        ) {
            Ok(encoder) => {
                // If the user forced a specific encoder/codec and we ended up
                // on a different one — whether the choice failed to open or
                // was never offered (so it isn't even in `candidates`) — tell
                // them we downgraded.
                if let Some((backend, codec)) = explicit_target {
                    if candidate.backend != backend || candidate.codec != codec {
                        let reason = if last_err.is_empty() {
                            "not available on this system".to_string()
                        } else {
                            last_err.clone()
                        };
                        warn_user(
                            events,
                            format!(
                                "{:?} encoder unavailable ({reason}); using {} instead",
                                opts.video_encoder,
                                encoder_label(*candidate)
                            ),
                        );
                    }
                }
                return Ok((encoder, *candidate));
            }
            Err(e) => last_err = e,
        }
    }
    Err(format!(
        "init: no video encoder could be opened: {last_err}"
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FfmpegConversionPath {
    Gpu,
    Cpu,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MftEncoderPath {
    Hardware,
    Software,
}

fn mft_encoder_path(backend: EncoderBackend) -> MftEncoderPath {
    if backend == EncoderBackend::MfSoftware {
        MftEncoderPath::Software
    } else {
        MftEncoderPath::Hardware
    }
}

fn ffmpeg_conversion_path(backend: EncoderBackend) -> FfmpegConversionPath {
    if backend == EncoderBackend::MfSoftware {
        FfmpegConversionPath::Cpu
    } else {
        FfmpegConversionPath::Gpu
    }
}

/// Construct one candidate encoder. MFT uses the zero-copy GPU H.264 path;
/// FFmpeg hardware backends convert BGRA→NV12 on the GPU, while `MfSoftware`
/// uses readback and CPU conversion so it works without a video processor.
#[allow(clippy::too_many_arguments)]
fn open_candidate(
    candidate: EncoderCandidate,
    device: &ID3D11Device,
    opts: &ServiceOptions,
    in_w: u32,
    in_h: u32,
    enc_w: u32,
    enc_h: u32,
    ffmpeg_path: &Option<PathBuf>,
) -> Result<Box<dyn Encoder>, String> {
    match candidate.api {
        EncoderApi::Mft => {
            let cfg = MftConfig {
                width: enc_w,
                height: enc_h,
                fps: opts.fps,
                bitrate_bps: opts.bitrate_bps,
                encoder_backend: Some(candidate.backend),
            };
            match mft_encoder_path(candidate.backend) {
                MftEncoderPath::Software => SoftwareMftH264Encoder::new(device, in_w, in_h, cfg)
                    .map(|encoder| Box::new(encoder) as Box<dyn Encoder>)
                    .map_err(|error| error.to_string()),
                MftEncoderPath::Hardware => MftH264Encoder::new(device, in_w, in_h, cfg)
                    .map(|encoder| Box::new(encoder) as Box<dyn Encoder>)
                    .map_err(|error| error.to_string()),
            }
        }
        EncoderApi::Ffmpeg => {
            let ffmpeg = ffmpeg_path
                .as_deref()
                .ok_or_else(|| "ffmpeg not located".to_string())?;
            let encoder = match ffmpeg_conversion_path(candidate.backend) {
                FfmpegConversionPath::Gpu => FfmpegVideoEncoder::new_on(
                    device,
                    ffmpeg,
                    candidate.backend,
                    candidate.codec,
                    in_w,
                    in_h,
                    None,
                    enc_w,
                    enc_h,
                    opts.fps,
                    opts.bitrate_bps,
                ),
                FfmpegConversionPath::Cpu => FfmpegVideoEncoder::new_cpu_on(
                    device,
                    ffmpeg,
                    candidate.backend,
                    candidate.codec,
                    in_w,
                    in_h,
                    None,
                    enc_w,
                    enc_h,
                    opts.fps,
                    opts.bitrate_bps,
                ),
            };
            encoder
                .map(|e| Box::new(e) as Box<dyn Encoder>)
                .map_err(|e| e.to_string())
        }
    }
}

struct PreparedReplayStorage {
    run_dir: Option<PathBuf>,
    max_bytes: usize,
    armed: bool,
}

impl PreparedReplayStorage {
    fn memory(max_bytes: usize) -> Self {
        Self {
            run_dir: None,
            max_bytes,
            armed: false,
        }
    }

    fn disk(run_dir: PathBuf, max_bytes: usize) -> Self {
        Self {
            run_dir: Some(run_dir),
            max_bytes,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PreparedReplayStorage {
    fn drop(&mut self) {
        if self.armed {
            if let Some(run_dir) = &self.run_dir {
                let _ = std::fs::remove_dir_all(run_dir);
            }
        }
    }
}

#[derive(serde::Deserialize, serde::Serialize)]
struct ReplayCacheOwner {
    process_instance_id: String,
    created_at_unix: u64,
}

fn prepare_replay_storage(opts: &ServiceOptions) -> Result<PreparedReplayStorage, String> {
    match &opts.replay_storage {
        ReplayStorageOptions::Memory => Ok(PreparedReplayStorage::memory(opts.buffer_bytes)),
        ReplayStorageOptions::Disk { dir, quota_bytes } => {
            if *quota_bytes < 256 * 1024 * 1024 {
                return Err("replay cache quota is too small".into());
            }
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("create replay cache folder {dir:?}: {e}"))?;
            ensure_replay_cache_free_space(opts)?;
            let now = SystemTime::now();
            let preserved_bytes =
                sweep_replay_cache_runs(dir, now, crate::windows::process_instance_id)?;
            let available_quota = quota_bytes.saturating_sub(preserved_bytes);
            if available_quota == 0 {
                return Err(format!(
                    "replay cache quota is already consumed by active or protected runs ({preserved_bytes} bytes)"
                ));
            }
            let current_process_instance_id =
                crate::windows::process_instance_id(std::process::id())?;
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let run_dir = (0u32..1024)
                .find_map(|attempt| {
                    let candidate = dir.join(format!(
                        "{REPLAY_CACHE_RUN_PREFIX}{stamp}-{}-{attempt}",
                        std::process::id()
                    ));
                    match std::fs::create_dir(&candidate) {
                        Ok(()) => Some(Ok(candidate)),
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => None,
                        Err(e) => Some(Err(format!(
                            "create replay cache run folder {candidate:?}: {e}"
                        ))),
                    }
                })
                .unwrap_or_else(|| {
                    Err("create replay cache run folder: too many collisions".into())
                })?;
            let owner = ReplayCacheOwner {
                process_instance_id: current_process_instance_id,
                created_at_unix: now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
            };
            if let Err(error) = write_replay_cache_owner(&run_dir, &owner) {
                let _ = std::fs::remove_dir_all(&run_dir);
                return Err(error);
            }
            Ok(PreparedReplayStorage::disk(
                run_dir,
                usize::try_from(available_quota).unwrap_or(usize::MAX),
            ))
        }
    }
}

fn write_replay_cache_owner(run_dir: &Path, owner: &ReplayCacheOwner) -> Result<(), String> {
    let path = run_dir.join(REPLAY_CACHE_OWNER_FILE);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| format!("create replay cache ownership record {path:?}: {e}"))?;
    serde_json::to_writer(&mut file, owner)
        .map_err(|e| format!("write replay cache ownership record {path:?}: {e}"))?;
    file.sync_all()
        .map_err(|e| format!("flush replay cache ownership record {path:?}: {e}"))
}

fn sweep_replay_cache_runs(
    root: &Path,
    now: SystemTime,
    mut process_instance_id: impl FnMut(u32) -> Result<String, String>,
) -> Result<u64, String> {
    let entries =
        std::fs::read_dir(root).map_err(|e| format!("scan replay cache folder {root:?}: {e}"))?;
    let mut preserved_bytes = 0u64;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !is_replay_cache_run_name(name) {
            continue;
        }
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_dir() || is_link_or_reparse_point(&metadata) {
            continue;
        }

        let owner = std::fs::read(path.join(REPLAY_CACHE_OWNER_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<ReplayCacheOwner>(&bytes).ok());
        let definitively_stale = owner
            .as_ref()
            .and_then(|owner| {
                replay_cache_owner_pid(&owner.process_instance_id).map(|pid| (owner, pid))
            })
            .and_then(|(owner, pid)| {
                process_instance_id(pid)
                    .ok()
                    .map(|current| current != owner.process_instance_id)
            });
        let old_enough = metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= AMBIGUOUS_REPLAY_CACHE_MAX_AGE);
        let should_remove = definitively_stale.unwrap_or(old_enough);

        if should_remove && std::fs::remove_dir_all(&path).is_ok() {
            continue;
        }
        preserved_bytes = preserved_bytes.saturating_add(replay_cache_run_size(&path));
    }
    Ok(preserved_bytes)
}

fn is_replay_cache_run_name(name: &str) -> bool {
    let Some(suffix) = name.strip_prefix(REPLAY_CACHE_RUN_PREFIX) else {
        return false;
    };
    let mut parts = suffix.split('-');
    let valid = (0..3).all(|_| {
        parts
            .next()
            .is_some_and(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
    });
    valid && parts.next().is_none()
}

fn replay_cache_owner_pid(process_instance_id: &str) -> Option<u32> {
    let (pid, creation_time) = process_instance_id.split_once(':')?;
    if creation_time.is_empty() || !creation_time.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    pid.parse().ok()
}

fn replay_cache_run_size(path: &Path) -> u64 {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if is_link_or_reparse_point(&metadata) {
        return 0;
    }
    if metadata.is_file() {
        return metadata.len();
    }
    if !metadata.is_dir() {
        return 0;
    }
    std::fs::read_dir(path)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| replay_cache_run_size(&entry.path()))
                .fold(0u64, u64::saturating_add)
        })
        .unwrap_or(0)
}

fn is_link_or_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

fn ensure_replay_cache_free_space(opts: &ServiceOptions) -> Result<(), String> {
    let ReplayStorageOptions::Disk { dir, .. } = &opts.replay_storage else {
        return Ok(());
    };
    let free = available_space_bytes(dir)?;
    if free < LOW_REPLAY_CACHE_DISK_RESERVE_BYTES {
        return Err(format!(
            "replay cache disk is low: {} MiB free, need at least 2048 MiB",
            free / (1024 * 1024)
        ));
    }
    Ok(())
}

fn available_space_bytes(path: &Path) -> Result<u64, String> {
    crate::windows::available_space_bytes(path, &format!("could not read free space for {path:?}"))
}

fn send_stopped(events: &Sender<Event>) {
    let _ = events.send(Event::Status {
        recording: false,
        waiting_for_game: false,
        segments: 0,
        buffered_s: 0.0,
        buffered_mb: 0.0,
        full_session: false,
        encoder: String::new(),
        capture_backend: String::new(),
    });
}

fn send_recording_status(
    events: &Sender<Event>,
    rec: &LiveRecorder,
    full_session: &Option<FullSessionRecording>,
    encoder_status: &str,
    capture_backend_status: &str,
) {
    let _ = events.send(Event::Status {
        recording: true,
        waiting_for_game: false,
        segments: rec.ring_len(),
        buffered_s: rec.buffered_span_s(),
        buffered_mb: rec.ring_bytes() as f64 / (1024.0 * 1024.0),
        full_session: full_session.is_some(),
        encoder: encoder_status.to_string(),
        capture_backend: capture_backend_status.to_string(),
    });
}

fn recover_abandoned_recordings(clips_dir: &Path, events: &Sender<Event>) {
    static RECOVERED_THIS_PROCESS: AtomicBool = AtomicBool::new(false);
    if RECOVERED_THIS_PROCESS.swap(true, Ordering::AcqRel) {
        return;
    }
    match recover_recording_files(clips_dir) {
        Ok(report) => {
            if !report.recovered.is_empty() {
                warn_user(
                    events,
                    format!(
                        "recovered {} unfinished full-session recording(s)",
                        report.recovered.len()
                    ),
                );
            }
            if report.deleted_empty > 0 {
                warn_user(
                    events,
                    format!(
                        "cleaned up {} empty unfinished full-session recording(s)",
                        report.deleted_empty
                    ),
                );
            }
        }
        Err(e) => warn_user(events, format!("recover unfinished recordings: {e}")),
    }
}

struct RecorderFinishContext<'a> {
    marker_log: &'a MarkerLog,
    player_summary: Option<&'a PlayerSummary>,
    audio_tracks: &'a [ClipAudioTrack],
    clips_dir: &'a Path,
    opts: &'a ServiceOptions,
    events: &'a Sender<Event>,
}

fn shutdown_recorder(
    rec: &mut LiveRecorder,
    full_session: &mut Option<FullSessionRecording>,
    ctx: RecorderFinishContext<'_>,
) -> Option<String> {
    match rec.finish_stream() {
        Ok(()) => {
            let _ = finish_full_session_recording(rec, full_session, &ctx);
            None
        }
        Err(e) => {
            let message = format!("finish: {e}");
            warn_user(ctx.events, message.clone());
            preserve_full_session_recording(
                rec,
                full_session,
                ctx.events,
                "full session could not finish cleanly and was kept for recovery",
            );
            Some(message)
        }
    }
}

fn finalize_runtime_failure(primary: String, finalize: impl FnOnce() -> Option<String>) -> String {
    match finalize() {
        Some(finish) => format!("{primary}; additionally, {finish}"),
        None => primary,
    }
}

/// Sidecar that records which game a session folder belongs to, so the
/// library can show its icon. Written once per folder; custom-game clips have
/// no markers, so this is their only game link.
const SESSION_META_FILE: &str = "clipline-session.json";

fn write_session_game_meta(
    session_dir: &Path,
    active_game: Option<&ActiveGame>,
    league_queue: Option<&LeagueQueue>,
) {
    let Some(game) = active_game else { return };
    let meta_path = session_dir.join(SESSION_META_FILE);
    if meta_path.exists() && league_queue.is_none() {
        return;
    }
    let mut doc = serde_json::json!({ "id": game.identity.id(), "name": game.name });
    if let Some(queue) = league_queue {
        doc["queue"] = serde_json::json!(queue);
    }
    match serde_json::to_string(&doc) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&meta_path, json) {
                tracing::warn!(event = "session_game_metadata_write_failed", error = %e);
            }
        }
        Err(e) => tracing::warn!(event = "session_game_metadata_serialize_failed", error = %e),
    }
}

fn begin_full_session_recording(
    rec: &mut LiveRecorder,
    clips_dir: &Path,
    session_label: &str,
    mode: RecordingMode,
    active_game: Option<&ActiveGame>,
    events: &Sender<Event>,
) -> Option<FullSessionRecording> {
    if mode != RecordingMode::FullSession {
        return None;
    }

    let session_dir = clips_dir.join(session_label);
    if let Err(e) = std::fs::create_dir_all(&session_dir) {
        warn_user(
            events,
            format!("full-session recording unavailable; create {session_dir:?}: {e}"),
        );
        return None;
    }
    write_session_game_meta(&session_dir, active_game, None);
    let stamp = unix_now_u64();
    let (final_path, temp_path, file) =
        match reserve_full_session_path_at(&session_dir, "session", stamp) {
            Ok(reservation) => reservation,
            Err(e) => {
                warn_user(
                    events,
                    format!(
                        "full-session recording unavailable; reserve path in {session_dir:?}: {e}"
                    ),
                );
                return None;
            }
        };
    if let Err(e) = rec.start_full_session(file) {
        handle_full_session_finish_error(
            &temp_path,
            events,
            &format!("full-session recording unavailable; start writer: {e}"),
        );
        return None;
    }
    Some(FullSessionRecording {
        final_path,
        temp_path,
        wall_start_unix: unix_now_i64(),
    })
}

fn finish_full_session_recording(
    rec: &mut LiveRecorder,
    recording: &mut Option<FullSessionRecording>,
    ctx: &RecorderFinishContext<'_>,
) -> Option<StorageStatus> {
    let recording = recording.take()?;
    match rec.finish_full_session() {
        Ok(Some(summary)) if summary.duration_s.is_finite() && summary.duration_s <= 0.0 => {
            handle_full_session_finish_error(
                &recording.temp_path,
                ctx.events,
                "full session ended before any footage was written",
            );
            None
        }
        Ok(Some(summary)) => {
            let seconds = if summary.duration_s.is_finite() {
                summary.duration_s
            } else {
                warn_user(
                    ctx.events,
                    "full session duration was invalid; keeping the recording with an unknown duration"
                        .into(),
                );
                0.0
            };
            if !rename_finalized_session(&recording, ctx.events) {
                return None;
            }
            let markers = write_marker_sidecar(
                ctx.events,
                ctx.marker_log,
                &recording.final_path,
                summary.start_s,
                summary.end_s,
                ctx.player_summary,
                ctx.audio_tracks,
            );
            Some(emit_saved_clip(
                ctx.events,
                ctx.clips_dir,
                &recording.final_path,
                seconds,
                SavedClipMeta {
                    markers,
                    full_session: true,
                    recording_start_unix: Some(recording.wall_start_unix),
                    recording_end_unix: Some(unix_now_i64()),
                },
                ctx.opts,
            ))
        }
        Ok(None) => {
            handle_full_session_finish_error(
                &recording.temp_path,
                ctx.events,
                "full session ended before any footage was written",
            );
            None
        }
        Err(error) => {
            handle_full_session_finish_error(&recording.temp_path, ctx.events, &error.to_string());
            None
        }
    }
}

fn handle_full_session_finish_error(temp_path: &Path, events: &Sender<Event>, error: &str) {
    match std::fs::metadata(temp_path) {
        Ok(metadata) if metadata.is_file() && metadata.len() == 0 => {
            remove_discarded_clip(temp_path);
            warn_user(events, format!("finish full session: {error}"));
        }
        Ok(_) => warn_user(
            events,
            format!("finish full session: {error}; recoverable recording kept at {temp_path:?}"),
        ),
        Err(metadata_error) if metadata_error.kind() == std::io::ErrorKind::NotFound => {
            let _ = remove_clip_ownership_marker(temp_path);
            warn_user(events, format!("finish full session: {error}"));
        }
        Err(metadata_error) => warn_user(
            events,
            format!(
                "finish full session: {error}; could not inspect {temp_path:?} ({metadata_error}), so it was kept for recovery"
            ),
        ),
    }
}

fn rename_finalized_session(recording: &FullSessionRecording, events: &Sender<Event>) -> bool {
    match std::fs::rename(&recording.temp_path, &recording.final_path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && recording.final_path.is_file() => {
            true
        }
        Err(error) => {
            let recovery = if recording.temp_path.is_file() {
                format!("; recoverable recording kept at {:?}", recording.temp_path)
            } else {
                String::new()
            };
            warn_user(
                events,
                format!(
                    "finalize full session {:?} -> {:?}: {error}{recovery}",
                    recording.temp_path, recording.final_path,
                ),
            );
            false
        }
    }
}

fn preserve_full_session_recording(
    rec: &mut LiveRecorder,
    recording: &mut Option<FullSessionRecording>,
    events: &Sender<Event>,
    reason: &str,
) {
    let Some(recording) = recording.take() else {
        return;
    };
    if let Err(e) = rec.finish_full_session() {
        warn_user(events, format!("stop full-session writer: {e}"));
    }
    handle_full_session_finish_error(&recording.temp_path, events, reason);
}

fn remove_discarded_clip(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = remove_clip_ownership_marker(path);
}

struct FullSessionRecording {
    final_path: PathBuf,
    temp_path: PathBuf,
    wall_start_unix: i64,
}

struct FullSessionQuotaCheck {
    event: Option<Event>,
    /// Present when auto-delete ran and the cached library baseline should move.
    new_baseline_bytes: Option<u64>,
}

fn full_session_quota_check(
    events: &Sender<Event>,
    clips_dir: &Path,
    recording: &FullSessionRecording,
    saved_media_baseline_bytes: Option<u64>,
    quota_bytes: Option<u64>,
    required_bytes: u64,
    auto_delete: bool,
) -> FullSessionQuotaCheck {
    let Some(baseline) = saved_media_baseline_bytes else {
        return FullSessionQuotaCheck {
            event: None,
            new_baseline_bytes: None,
        };
    };
    let active_bytes = match std::fs::metadata(&recording.temp_path) {
        Ok(metadata) => metadata.len(),
        Err(error) => {
            tracing::warn!(
                event = "full_session_quota_inspection_failed",
                path = ?recording.temp_path,
                error = %error,
            );
            return FullSessionQuotaCheck {
                event: None,
                new_baseline_bytes: None,
            };
        }
    };
    let total_bytes = baseline.saturating_add(active_bytes);
    if !quota_would_be_exceeded(total_bytes, quota_bytes, required_bytes) {
        return FullSessionQuotaCheck {
            event: None,
            new_baseline_bytes: None,
        };
    }
    if auto_delete {
        // Inventory after cleanup includes the active recording, which
        // enforce_quota never deletes.
        if let Some((cleaned, _)) =
            make_room_for_quota(events, clips_dir, quota_bytes, required_bytes, None)
        {
            let new_baseline_bytes = Some(cleaned.total_bytes.saturating_sub(active_bytes));
            if !quota_would_be_exceeded(cleaned.total_bytes, quota_bytes, required_bytes) {
                return FullSessionQuotaCheck {
                    event: None,
                    new_baseline_bytes,
                };
            }
            return FullSessionQuotaCheck {
                event: storage_quota_event_for_usage(
                    cleaned.total_bytes,
                    quota_bytes,
                    required_bytes,
                ),
                new_baseline_bytes,
            };
        }
    }
    FullSessionQuotaCheck {
        event: storage_quota_event_for_usage(total_bytes, quota_bytes, required_bytes),
        new_baseline_bytes: None,
    }
}

fn unique_media_path(session_dir: &Path, prefix: &str) -> PathBuf {
    unique_media_path_at(session_dir, prefix, unix_now_u64())
}

fn unique_media_path_at(session_dir: &Path, prefix: &str, stamp: u64) -> PathBuf {
    for attempt in 0u32..1024 {
        let name = if attempt == 0 {
            format!("{prefix}_{stamp}.mp4")
        } else {
            format!("{prefix}_{stamp}_{attempt}.mp4")
        };
        let candidate = session_dir.join(name);
        let marker_exists =
            clip_ownership_marker_path(&candidate).is_ok_and(|marker| marker.exists());
        if !candidate.exists() && !marker_exists {
            return candidate;
        }
    }
    let fallback = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    session_dir.join(format!("{prefix}_{fallback}.mp4"))
}

fn reserve_full_session_path_at(
    session_dir: &Path,
    prefix: &str,
    stamp: u64,
) -> std::io::Result<(PathBuf, PathBuf, std::fs::File)> {
    reserve_full_session_path_at_with(session_dir, prefix, stamp, |_, temp_path| {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(temp_path)
    })
}

fn reserve_full_session_path_at_with<F>(
    session_dir: &Path,
    prefix: &str,
    stamp: u64,
    mut reserve_temp: F,
) -> std::io::Result<(PathBuf, PathBuf, std::fs::File)>
where
    F: FnMut(&Path, &Path) -> std::io::Result<std::fs::File>,
{
    for attempt in 0u32..1024 {
        let name = if attempt == 0 {
            format!("{prefix}_{stamp}.mp4")
        } else {
            format!("{prefix}_{stamp}_{attempt}.mp4")
        };
        let final_path = session_dir.join(name);
        if final_path.try_exists()? || clip_ownership_marker_path(&final_path)?.try_exists()? {
            continue;
        }
        let temp_path = final_path.with_extension("mp4.recording");
        match reserve_temp(&final_path, &temp_path) {
            Ok(file) => match final_path.try_exists() {
                Ok(false) => match ensure_clip_owned(&temp_path) {
                    Ok(true) => match final_path.try_exists() {
                        Ok(false) => return Ok((final_path, temp_path, file)),
                        Ok(true) => {
                            drop(file);
                            remove_discarded_clip(&temp_path);
                            continue;
                        }
                        Err(check_error) => {
                            drop(file);
                            remove_discarded_clip(&temp_path);
                            return Err(check_error);
                        }
                    },
                    Ok(false) => {
                        drop(file);
                        std::fs::remove_file(&temp_path)?;
                        continue;
                    }
                    Err(marker_error) => {
                        drop(file);
                        let _ = std::fs::remove_file(&temp_path);
                        return Err(marker_error);
                    }
                },
                Ok(true) => {
                    drop(file);
                    std::fs::remove_file(&temp_path)?;
                    continue;
                }
                Err(check_error) => {
                    drop(file);
                    if let Err(cleanup_error) = std::fs::remove_file(&temp_path) {
                        return Err(std::io::Error::new(
                            check_error.kind(),
                            format!(
                                "inspect reserved final path {final_path:?}: {check_error}; \
                                 remove reservation {temp_path:?}: {cleanup_error}"
                            ),
                        ));
                    }
                    return Err(check_error);
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("no free {prefix}_{stamp} full-session path after 1024 attempts"),
    ))
}

fn write_marker_sidecar(
    events: &Sender<Event>,
    marker_log: &MarkerLog,
    path: &Path,
    start_s: f64,
    end_s: f64,
    player_summary: Option<&PlayerSummary>,
    audio_tracks: &[ClipAudioTrack],
) -> usize {
    let mut clip = marker_log.clip_markers(start_s, end_s);
    clip.markers.retain(|m| is_review_event(&m.event));
    clip.player_summary = player_summary.cloned();
    clip.audio_tracks = audio_tracks.to_vec();
    // User-placed bookmarks are markers to the review timeline, so they count
    // toward the clip's marker total and keep the sidecar worth writing.
    let markers = clip.markers.len() + clip.bookmarks.len();
    if markers == 0
        && clip.player_summary.is_none()
        && clip.audio_tracks.is_empty()
        && clip.plays.is_empty()
    {
        return 0;
    }
    match serde_json::to_string_pretty(&clip) {
        Ok(json) => {
            if let Err(e) = std::fs::write(path.with_extension("markers.json"), json) {
                warn_user(events, format!("write marker sidecar for {path:?}: {e}"));
            }
        }
        Err(e) => warn_user(
            events,
            format!("serialize marker sidecar for {path:?}: {e}"),
        ),
    }
    markers
}

struct SavedClipMeta {
    markers: usize,
    full_session: bool,
    recording_start_unix: Option<i64>,
    recording_end_unix: Option<i64>,
}

fn emit_saved_clip(
    events: &Sender<Event>,
    clips_dir: &Path,
    path: &Path,
    seconds: f64,
    meta: SavedClipMeta,
    opts: &ServiceOptions,
) -> StorageStatus {
    let fallback_status = || StorageStatus {
        clip_count: 0,
        total_bytes: 0,
        quota_bytes: opts.disk_quota_bytes,
    };
    let status = if opts.auto_delete_when_over_quota {
        match enforce_quota_with_protection(
            clips_dir,
            opts.disk_quota_bytes,
            Some(path),
            crate::cloud_upload::is_active_upload_source,
        ) {
            Ok(report) => report.status,
            Err(e) => {
                warn_user(events, format!("storage cleanup: {e}"));
                match storage_status(clips_dir, opts.disk_quota_bytes) {
                    Ok(status) => status,
                    Err(status_error) => {
                        warn_user(events, format!("storage status: {status_error}"));
                        fallback_status()
                    }
                }
            }
        }
    } else {
        match storage_status(clips_dir, opts.disk_quota_bytes) {
            Ok(status) => status,
            Err(e) => {
                warn_user(events, format!("storage status: {e}"));
                fallback_status()
            }
        }
    };

    let _ = events.send(Event::Saved {
        path: path.display().to_string(),
        seconds,
        recording_start_unix: meta.recording_start_unix,
        recording_end_unix: meta.recording_end_unix,
        markers: meta.markers,
        full_session: meta.full_session,
        storage_total_bytes: status.total_bytes,
        storage_quota_bytes: status.quota_bytes,
        storage_over_quota: status.is_over_quota(),
    });
    status
}

fn save(
    rec: &Recorder<impl CaptureEngine, impl Encoder>,
    path: &Path,
    window_s: f64,
) -> Result<(f64, f64), String> {
    let marker_created =
        ensure_clip_owned(path).map_err(|e| format!("mark Clipline-owned clip {path:?}: {e}"))?;
    let saved_from = rec
        .save_window_bounds(window_s, None)
        .map(|(start, _)| start);
    let result = (|| {
        let file = std::fs::File::create(path).map_err(|e| format!("create {path:?}: {e}"))?;
        let (_, end) = rec
            .save_replay(file, window_s, None)
            .map_err(|e| format!("save: {e}"))?;
        Ok((end, end - saved_from.unwrap_or(end)))
    })();
    if result.is_err() && marker_created {
        let _ = remove_clip_ownership_marker(path);
    }
    result
}

fn crop_for_region(
    region: &CaptureRegion,
    display: &clipline_capture::windows::display::DisplayInfo,
) -> Result<CropRect, String> {
    if region.width < 2 || region.height < 2 {
        return Err("capture region must be at least 2x2 pixels".into());
    }
    let local_x = region.x - display.x;
    let local_y = region.y - display.y;
    if local_x < 0
        || local_y < 0
        || local_x as i64 + region.width as i64 > display.width as i64
        || local_y as i64 + region.height as i64 > display.height as i64
    {
        return Err(format!(
            "capture region must fit inside {} ({}x{} at {}, {})",
            display.name, display.width, display.height, display.x, display.y
        ));
    }
    Ok(CropRect {
        x: local_x as u32,
        y: local_y as u32,
        width: region.width,
        height: region.height,
    })
}

fn crop_for_region_or_full_display(
    region: &CaptureRegion,
    display: &clipline_capture::windows::display::DisplayInfo,
    recovered_display: bool,
) -> Result<(CropRect, bool), String> {
    if region.width < 2 || region.height < 2 {
        return Err("capture region must be at least 2x2 pixels".into());
    }
    if !recovered_display {
        if let Some(crop) = rebased_full_display_crop(region, display) {
            return Ok((crop, false));
        }
        if let Ok(crop) = crop_for_region(region, display) {
            return Ok((crop, false));
        }
        if let Some(crop) = clamped_region_crop(region, display)? {
            return Ok((crop, true));
        }
    }
    Ok((
        CropRect {
            x: 0,
            y: 0,
            width: display.width,
            height: display.height,
        },
        true,
    ))
}

fn rebased_full_display_crop(
    region: &CaptureRegion,
    display: &clipline_capture::windows::display::DisplayInfo,
) -> Option<CropRect> {
    if region.width == display.width && region.height == display.height {
        Some(CropRect {
            x: 0,
            y: 0,
            width: display.width,
            height: display.height,
        })
    } else {
        None
    }
}

fn clamped_region_crop(
    region: &CaptureRegion,
    display: &clipline_capture::windows::display::DisplayInfo,
) -> Result<Option<CropRect>, String> {
    if region.width < 2 || region.height < 2 {
        return Err("capture region must be at least 2x2 pixels".into());
    }
    let region_left = region.x as i64;
    let region_top = region.y as i64;
    let region_right = region_left + region.width as i64;
    let region_bottom = region_top + region.height as i64;
    let display_left = display.x as i64;
    let display_top = display.y as i64;
    let display_right = display_left + display.width as i64;
    let display_bottom = display_top + display.height as i64;

    let left = region_left.max(display_left);
    let top = region_top.max(display_top);
    let right = region_right.min(display_right);
    let bottom = region_bottom.min(display_bottom);
    let width = right - left;
    let height = bottom - top;
    if width < 2 || height < 2 {
        return Ok(None);
    }
    Ok(Some(CropRect {
        x: (left - display_left) as u32,
        y: (top - display_top) as u32,
        width: width as u32,
        height: height as u32,
    }))
}

fn capture_display_recovery_warning(
    region: &CaptureRegion,
    display: &clipline_capture::windows::display::DisplayInfo,
    recovered_display: bool,
    recovered_crop: bool,
) -> Option<String> {
    if !recovered_display && !recovered_crop {
        return None;
    }
    let configured = region
        .display_id
        .as_deref()
        .unwrap_or("the configured display");
    let fallback = if recovered_display {
        format!("using full display {}", display.name)
    } else {
        format!("using the visible part of the region on {}", display.name)
    };
    Some(format!(
        "capture target {configured} is no longer available or no longer fits; {fallback}. Open Settings and save your capture source to update it."
    ))
}

fn warn_capture_display_recovery(
    events: &Sender<Event>,
    region: &CaptureRegion,
    display: &clipline_capture::windows::display::DisplayInfo,
    recovered_display: bool,
    recovered_crop: bool,
) {
    if let Some(message) =
        capture_display_recovery_warning(region, display, recovered_display, recovered_crop)
    {
        tracing::warn!(event = "capture_display_recovered", message = %message);
        warn_user(events, message);
    }
}

/// Session label from the local wall clock (folder names should match what
/// the user's file explorer shows, not UTC).
fn local_session_label(league_match: bool) -> String {
    use chrono::{Datelike, Local, Timelike};
    let now = Local::now();
    session_label(
        now.year(),
        now.month(),
        now.day(),
        now.hour(),
        now.minute(),
        league_match,
    )
}

pub(crate) fn default_clips_dir() -> PathBuf {
    media_root::default_clips_dir()
}

pub(crate) fn clips_dir(media_dir: &Path) -> Result<PathBuf, String> {
    media_root::clips_dir(media_dir)
}

/// Resolve the directory clips are actually written to. The configured folder
/// is used when it can reserve and durably write a new file; otherwise
/// `fallback` is, so an unplugged external drive degrades to the default folder
/// instead of killing recording and emptying the library. The bool is true when
/// the fallback was taken, so callers with a UI channel can warn the user.
pub(crate) fn clips_dir_resolved(
    media_dir: &Path,
    fallback: impl FnOnce() -> PathBuf,
) -> Result<(PathBuf, bool), String> {
    media_root::clips_dir_resolved_with_probe(media_dir, fallback, probe_writable_directory)
}

#[cfg(test)]
fn clips_dir_resolved_with_probe(
    media_dir: &Path,
    fallback: impl FnOnce() -> PathBuf,
    mut probe: impl FnMut(&Path) -> std::io::Result<()>,
) -> Result<(PathBuf, bool), String> {
    media_root::clips_dir_resolved_with_probe(media_dir, fallback, &mut probe)
}

pub(crate) fn prepare_writable_media_directory(dir: &Path) -> Result<(), String> {
    prepare_writable_directory_with(dir, probe_writable_directory)
        .map_err(|error| format!("media folder {} is not writable: {error}", dir.display()))
}

fn prepare_writable_directory_with(
    dir: &Path,
    probe: impl FnMut(&Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    media_root::prepare_writable_directory_with(dir, probe)
}

fn probe_writable_directory(dir: &Path) -> std::io::Result<()> {
    media_root::probe_writable_directory(dir)
}

/// Whether `dir` lives under the system temp root. Both paths are canonicalized
/// when they exist so a symlinked or short-name temp root still matches.
fn is_within_temp(dir: &Path, temp_dir: &Path) -> bool {
    media_root::is_within_temp(dir, temp_dir)
}

#[cfg(test)]
mod tests {
    use super::{
        ffmpeg_capability_identity, ffmpeg_capability_slot, full_session_quota_check,
        quota_would_be_exceeded, storage_quota_full_event, Event, FullSessionRecording,
    };
    use clipline_capture::{Codec, EncoderApi, EncoderBackend, EncoderCapability};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn saved_media_quota_blocks_only_when_the_requested_write_would_cross_it() {
        assert!(!quota_would_be_exceeded(90, Some(100), 10));
        assert!(quota_would_be_exceeded(90, Some(100), 11));
        assert!(quota_would_be_exceeded(u64::MAX, Some(u64::MAX), 1));
        assert!(!quota_would_be_exceeded(u64::MAX, None, u64::MAX));
    }

    #[test]
    fn unreadable_quota_status_skips_the_check_instead_of_stopping_recording() {
        let dir = clipline_test_utils::TestDir::new("clipline-service", "quota-inspection-error");
        let file = dir.path().join("not-a-media-directory");
        std::fs::write(&file, b"file").unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        assert!(storage_quota_full_event(&tx, &file, Some(100), 1, false).is_none());
        assert!(storage_quota_full_event(&tx, &file, Some(100), 1, true).is_none());
    }

    #[test]
    fn auto_delete_makes_room_before_emitting_quota_full() {
        let dir = clipline_test_utils::TestDir::new("clipline-service", "quota-auto-delete");
        let old = dir.path().join("old.mp4");
        let keep = dir.path().join("keep.mp4");
        std::fs::write(&old, [0; 80]).unwrap();
        clipline_storage::ensure_clip_owned(&old).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&keep, [0; 10]).unwrap();
        clipline_storage::ensure_clip_owned(&keep).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        assert!(
            storage_quota_full_event(&tx, dir.path(), Some(100), 30, false).is_some(),
            "without auto-delete the requested write must still be blocked"
        );
        assert!(
            storage_quota_full_event(&tx, dir.path(), Some(100), 30, true).is_none(),
            "auto-delete should free the oldest clip and allow the write"
        );
        assert!(!old.exists());
        assert!(keep.exists());
        assert!(
            matches!(rx.try_recv(), Ok(Event::LibraryChanged)),
            "auto-delete must ask the UI to refresh after removing clips"
        );
    }

    #[test]
    fn full_session_quota_uses_cached_library_bytes_plus_active_file() {
        let dir = clipline_test_utils::TestDir::new("clipline-service", "quota-active-file");
        let temp_path = dir.path().join("session.mp4.recording");
        std::fs::write(&temp_path, [0; 15]).unwrap();
        let recording = FullSessionRecording {
            final_path: dir.path().join("session.mp4"),
            temp_path,
            wall_start_unix: 0,
        };

        let (tx, _rx) = std::sync::mpsc::channel();
        let check = full_session_quota_check(
            &tx,
            dir.path(),
            &recording,
            Some(80),
            Some(100),
            6,
            false,
        );
        assert!(matches!(
            check.event,
            Some(Event::StorageQuotaFull {
                total_bytes: 95,
                quota_bytes: 100,
                required_bytes: 6,
            })
        ));
        assert_eq!(check.new_baseline_bytes, None);
    }

    #[test]
    fn full_session_auto_delete_updates_baseline_and_refreshes_library() {
        let dir = clipline_test_utils::TestDir::new("clipline-service", "quota-full-session-gc");
        let old = dir.path().join("old.mp4");
        std::fs::write(&old, [0; 80]).unwrap();
        clipline_storage::ensure_clip_owned(&old).unwrap();
        let temp_path = dir.path().join("session.mp4.recording");
        std::fs::write(&temp_path, [0; 15]).unwrap();
        // Production reservations mark the active recording owned so inventory
        // counts it (and refuses to delete it) during auto-delete.
        clipline_storage::ensure_clip_owned(&temp_path).unwrap();
        let recording = FullSessionRecording {
            final_path: dir.path().join("session.mp4"),
            temp_path,
            wall_start_unix: 0,
        };

        let (tx, rx) = std::sync::mpsc::channel();
        // Baseline is only the saved library; active recording is added each tick.
        let check = full_session_quota_check(
            &tx,
            dir.path(),
            &recording,
            Some(80),
            Some(100),
            6,
            true,
        );
        assert!(check.event.is_none(), "cleanup should make room for the reserve");
        assert!(!old.exists());
        assert!(matches!(rx.try_recv(), Ok(Event::LibraryChanged)));
        let baseline = check.new_baseline_bytes.expect("baseline must refresh");
        assert_eq!(baseline, 0);
    }

    #[test]
    fn ffmpeg_capability_slot_reuses_same_identity() {
        let probes = AtomicUsize::new(0);
        let first = ffmpeg_capability_slot(None, "managed:x:abc", || {
            probes.fetch_add(1, Ordering::SeqCst);
            vec![EncoderCapability {
                api: EncoderApi::Ffmpeg,
                backend: EncoderBackend::SvtAv1,
                codecs: vec![Codec::Av1],
            }]
        });
        let second = ffmpeg_capability_slot(Some(&first), "managed:x:abc", || {
            probes.fetch_add(1, Ordering::SeqCst);
            panic!("should not reprobe");
        });
        assert_eq!(first.identity, second.identity);
        assert_eq!(first.caps.len(), second.caps.len());
        assert_eq!(probes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn ffmpeg_capability_slot_reprobes_when_identity_changes() {
        let probes = AtomicUsize::new(0);
        let first = ffmpeg_capability_slot(None, "missing", || {
            probes.fetch_add(1, Ordering::SeqCst);
            Vec::new()
        });
        let second = ffmpeg_capability_slot(Some(&first), "external:C:/ffmpeg.exe", || {
            probes.fetch_add(1, Ordering::SeqCst);
            vec![EncoderCapability {
                api: EncoderApi::Ffmpeg,
                backend: EncoderBackend::Nvenc,
                codecs: vec![Codec::H264],
            }]
        });
        assert_ne!(first.identity, second.identity);
        assert_eq!(second.caps.len(), 1);
        assert_eq!(probes.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn ffmpeg_capability_identity_prefers_managed_over_external() {
        let managed = crate::ffmpeg_runtime::ManagedRuntimeInfo {
            dir: std::path::PathBuf::from("C:/managed"),
            ffmpeg_exe: std::path::PathBuf::from("C:/managed/ffmpeg.exe"),
            release_tag: "tag".into(),
            archive_sha256: "aa".into(),
            manifest_sha256: "bb".into(),
        };
        assert_eq!(
            ffmpeg_capability_identity(
                Some(&managed),
                Some(std::path::Path::new("C:/ext/ffmpeg.exe"))
            ),
            "managed:C:/managed:bb"
        );
        assert_eq!(
            ffmpeg_capability_identity(None, Some(std::path::Path::new("C:/ext/ffmpeg.exe"))),
            "external:C:/ext/ffmpeg.exe"
        );
        assert_eq!(ffmpeg_capability_identity(None, None), "missing");
    }

    use super::*;
    use clipline_capture::{MockCapture, MockEncoder};
    use clipline_test_utils::TestDir;
    use std::collections::VecDeque;

    struct TimeoutSource;

    impl TimedFrameSource for TimeoutSource {
        fn next_frame_timeout(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError> {
            std::thread::sleep(timeout);
            Err(CaptureError::Timeout(timeout))
        }
    }

    struct ScriptedTimedSource {
        outcomes: VecDeque<Result<Option<Frame>, CaptureError>>,
        requested_timeouts: Vec<Duration>,
    }

    impl TimedFrameSource for ScriptedTimedSource {
        fn next_frame_timeout(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError> {
            self.requested_timeouts.push(timeout);
            let outcome = self
                .outcomes
                .pop_front()
                .expect("scripted timed source exhausted");
            if matches!(outcome, Err(CaptureError::Timeout(_))) {
                std::thread::sleep(timeout);
            }
            outcome
        }
    }

    struct DelayedFrameSource {
        frame: Option<Frame>,
        delay: Duration,
        requested_timeouts: Vec<Duration>,
    }

    impl TimedFrameSource for DelayedFrameSource {
        fn next_frame_timeout(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError> {
            self.requested_timeouts.push(timeout);
            if let Some(frame) = self.frame.take() {
                std::thread::sleep(self.delay);
                Ok(Some(frame))
            } else {
                std::thread::sleep(timeout);
                Err(CaptureError::Timeout(timeout))
            }
        }
    }

    struct BlockingTimeoutSource {
        requested_timeouts: Vec<Duration>,
    }

    impl TimedFrameSource for BlockingTimeoutSource {
        fn next_frame_timeout(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError> {
            self.requested_timeouts.push(timeout);
            std::thread::sleep(timeout);
            Err(CaptureError::Timeout(timeout))
        }
    }

    struct PrematureTimeoutSource {
        delay: Duration,
    }

    impl TimedFrameSource for PrematureTimeoutSource {
        fn next_frame_timeout(
            &mut self,
            _timeout: Duration,
        ) -> Result<Option<Frame>, CaptureError> {
            std::thread::sleep(self.delay);
            Err(CaptureError::Timeout(self.delay))
        }
    }

    #[test]
    fn video_encoder_id_matches_serde_serialization() {
        // The Settings dropdown sends EncoderOption.id; settings.rs maps it
        // back through VideoEncoder's snake_case serde. id() must stay in
        // lockstep with that derive, including the new codec variants.
        for enc in [
            VideoEncoder::Auto,
            VideoEncoder::NvencH264,
            VideoEncoder::NvencHevc,
            VideoEncoder::NvencAv1,
            VideoEncoder::AmfH264,
            VideoEncoder::AmfHevc,
            VideoEncoder::AmfAv1,
            VideoEncoder::QuickSyncH264,
            VideoEncoder::QuickSyncHevc,
            VideoEncoder::QuickSyncAv1,
            VideoEncoder::SvtAv1,
        ] {
            let serialized = serde_json::to_string(&enc).unwrap();
            assert_eq!(serialized, format!("\"{}\"", enc.id()));
        }
    }

    #[test]
    fn from_parts_round_trips_through_preference() {
        // Every explicit option maps back to the same (backend, codec).
        for (backend, codec) in [
            (EncoderBackend::Amf, Codec::Hevc),
            (EncoderBackend::Nvenc, Codec::Av1),
            (EncoderBackend::SvtAv1, Codec::Av1),
        ] {
            let enc = VideoEncoder::from_parts(backend, codec).unwrap();
            assert_eq!(
                enc.preference(),
                EncoderPreference::Explicit { backend, codec }
            );
        }
        assert!(VideoEncoder::from_parts(EncoderBackend::MfSoftware, Codec::H264).is_none());
        assert!(VideoEncoder::from_parts(EncoderBackend::SvtAv1, Codec::H264).is_none());
    }

    #[test]
    fn native_media_foundation_software_uses_synchronous_mft() {
        assert_eq!(
            mft_encoder_path(EncoderBackend::MfSoftware),
            MftEncoderPath::Software
        );
        assert_eq!(
            mft_encoder_path(EncoderBackend::Amf),
            MftEncoderPath::Hardware
        );
    }

    #[test]
    fn ffmpeg_media_foundation_software_uses_cpu_frame_conversion() {
        assert_eq!(
            ffmpeg_conversion_path(EncoderBackend::MfSoftware),
            FfmpegConversionPath::Cpu
        );
        assert_eq!(
            ffmpeg_conversion_path(EncoderBackend::Nvenc),
            FfmpegConversionPath::Gpu
        );
        assert_eq!(
            ffmpeg_conversion_path(EncoderBackend::SvtAv1),
            FfmpegConversionPath::Gpu
        );
    }

    #[test]
    fn output_dimensions_scale_down_to_selected_resolution() {
        assert_eq!(
            output_dimensions(2560, 1440, OutputResolution::Source),
            (2560, 1440)
        );
        assert_eq!(
            output_dimensions(2560, 1440, OutputResolution::P1080),
            (1920, 1080)
        );
        assert_eq!(
            output_dimensions(2560, 1440, OutputResolution::P720),
            (1280, 720)
        );
    }

    #[test]
    fn output_dimensions_preserve_aspect_and_never_upscale() {
        assert_eq!(
            output_dimensions(1600, 1000, OutputResolution::P1080),
            (1600, 1000)
        );
        assert_eq!(
            output_dimensions(5120, 1440, OutputResolution::P1080),
            (1920, 540)
        );
        assert_eq!(
            output_dimensions(5120, 1440, OutputResolution::Source),
            (2560, 720)
        );
    }

    #[test]
    fn missing_display_region_falls_back_to_full_current_display_crop() {
        let region = CaptureRegion {
            display_id: Some(r"\\.\DISPLAY-GHOST".into()),
            x: 1920,
            y: 0,
            width: 2560,
            height: 1440,
        };
        let display = clipline_capture::windows::display::DisplayInfo {
            id: r"\\.\DISPLAY1".into(),
            name: "DISPLAY1".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            is_primary: true,
        };

        let (crop, recovered) = crop_for_region_or_full_display(&region, &display, true).unwrap();

        assert!(recovered);
        assert_eq!(
            crop,
            CropRect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080
            }
        );
    }

    #[test]
    fn recovered_capture_display_builds_user_visible_warning() {
        let region = CaptureRegion {
            display_id: Some(r"\\.\DISPLAY-GHOST".into()),
            x: 1920,
            y: 0,
            width: 2560,
            height: 1440,
        };
        let display = clipline_capture::windows::display::DisplayInfo {
            id: r"\\.\DISPLAY1".into(),
            name: "DISPLAY1".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            is_primary: true,
        };

        let message = capture_display_recovery_warning(&region, &display, true, false)
            .expect("recovery warning");

        assert!(message.contains(r"\\.\DISPLAY-GHOST"), "{message}");
        assert!(message.contains("DISPLAY1"), "{message}");
        assert!(message.contains("Settings"), "{message}");
    }

    #[test]
    fn out_of_bounds_region_clamps_to_visible_display_crop() {
        let region = CaptureRegion {
            display_id: Some(r"\\.\DISPLAY1".into()),
            x: 1000,
            y: 500,
            width: 1000,
            height: 800,
        };
        let display = clipline_capture::windows::display::DisplayInfo {
            id: r"\\.\DISPLAY1".into(),
            name: "DISPLAY1".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            is_primary: true,
        };

        let (crop, recovered) = crop_for_region_or_full_display(&region, &display, false).unwrap();

        assert!(recovered);
        assert_eq!(
            crop,
            CropRect {
                x: 1000,
                y: 500,
                width: 920,
                height: 580
            }
        );
    }

    #[test]
    fn full_display_region_survives_virtual_origin_change() {
        let region = CaptureRegion {
            display_id: Some(r"\\.\DISPLAY1".into()),
            x: 1280,
            y: 0,
            width: 2560,
            height: 1440,
        };
        let display = clipline_capture::windows::display::DisplayInfo {
            id: r"\\.\DISPLAY1".into(),
            name: "DISPLAY1".into(),
            x: 0,
            y: 0,
            width: 2560,
            height: 1440,
            is_primary: true,
        };

        let (crop, recovered) = crop_for_region_or_full_display(&region, &display, false).unwrap();

        assert!(
            !recovered,
            "a full-display selection should rebase without a settings warning"
        );
        assert_eq!(
            crop,
            CropRect {
                x: 0,
                y: 0,
                width: 2560,
                height: 1440
            }
        );
    }

    #[test]
    fn marker_source_falls_back_to_league_poller_without_active_plugin() {
        let opts = ServiceOptions::default();

        assert_eq!(
            marker_source_kind(&opts),
            MarkerSourceKind::LegacyLeaguePoller
        );
    }

    #[test]
    fn marker_source_uses_active_plugin_event_source_when_available() {
        let opts = ServiceOptions {
            active_game: Some(ActiveGame {
                identity: crate::game_identity::GameIdentity::built_in_plugin(
                    crate::game_plugins::LEAGUE_OF_LEGENDS_ID,
                )
                .unwrap(),
                name: "League of Legends".into(),
                exe_path: None,
            }),
            ..ServiceOptions::default()
        };

        assert_eq!(marker_source_kind(&opts), MarkerSourceKind::Plugin);
    }

    #[test]
    fn session_game_metadata_merges_a_late_league_queue() {
        let dir = TestDir::new("clipline-service", "league-session-queue");
        let game = ActiveGame {
            identity: crate::game_identity::GameIdentity::built_in_plugin(
                crate::game_plugins::LEAGUE_OF_LEGENDS_ID,
            )
            .unwrap(),
            name: "League of Legends".into(),
            exe_path: None,
        };
        let queue = clipline_lol::LeagueQueue::from_id(420);

        write_session_game_meta(dir.path(), Some(&game), None);
        write_session_game_meta(dir.path(), Some(&game), Some(&queue));

        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join(SESSION_META_FILE)).unwrap())
                .unwrap();
        assert_eq!(value["id"], crate::game_plugins::LEAGUE_OF_LEGENDS_ID);
        assert_eq!(value["name"], "League of Legends");
        assert_eq!(value["queue"]["id"], 420);
        assert_eq!(value["queue"]["category"], "ranked-solo-duo");
        assert_eq!(value["queue"]["label"], "Ranked Solo/Duo");
    }

    #[test]
    fn custom_identity_cannot_enable_a_built_in_marker_source() {
        let opts = ServiceOptions {
            active_game: Some(ActiveGame {
                identity: crate::game_identity::GameIdentity::custom(
                    crate::game_plugins::LEAGUE_OF_LEGENDS_ID,
                ),
                name: "Community game".into(),
                exe_path: None,
            }),
            ..ServiceOptions::default()
        };

        assert_eq!(
            marker_source_kind(&opts),
            MarkerSourceKind::LegacyLeaguePoller
        );
    }

    #[test]
    fn split_output_candidates_exclude_clipline_process() {
        let own_pid = 42;
        let processes = vec![
            clipline_capture::windows::wasapi::AudioProcessInfo {
                pid: own_pid,
                label: "clipline-app".into(),
                process_name: Some("clipline-app".into()),
                process_path: Some(r"C:\Clipline\clipline-app.exe".into()),
            },
            clipline_capture::windows::wasapi::AudioProcessInfo {
                pid: 99,
                label: "Game".into(),
                process_name: Some("Game".into()),
                process_path: Some(r"C:\Games\Game.exe".into()),
            },
        ];

        let candidates = split_output_process_candidates(processes, own_pid);

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].label, "Game");
    }

    fn player_summary(champion_name: &str, kills: u32, deaths: u32, assists: u32) -> PlayerSummary {
        PlayerSummary {
            champion_name: champion_name.into(),
            kills,
            deaths,
            assists,
            creep_score: None,
            game_time_s: None,
            player_name: String::new(),
            team: String::new(),
            participants: Vec::new(),
            summoner_spells: Vec::new(),
            items: Vec::new(),
        }
    }

    fn review_event(
        kind: EventKind,
        actor: &str,
        victim: Option<&str>,
        offset_s: f64,
        involves_local_player: bool,
    ) -> clipline_events::GameEvent {
        clipline_events::GameEvent {
            game_id: clipline_events::GameId::LeagueOfLegends,
            kind,
            actor: actor.into(),
            victim: victim.map(String::from),
            assisters: Vec::new(),
            subtype: None,
            game_time_s: offset_s,
            recording_offset_s: Some(offset_s),
            importance: 7,
            involves_local_player,
        }
    }

    #[test]
    fn player_summary_state_stops_replay_attribution_after_match_end() {
        let mut state = PlayerSummaryState::default();
        let mid_match = player_summary("Nautilus", 3, 4, 22);
        let final_match = player_summary("Nautilus", 3, 4, 23);

        state.match_started();
        state.update(mid_match.clone());
        assert_eq!(state.active_replay_summary(), Some(&mid_match));
        assert_eq!(state.full_session_summary(), Some(&mid_match));

        state.match_ended();
        assert_eq!(state.active_replay_summary(), None);
        assert_eq!(state.full_session_summary(), Some(&mid_match));

        state.update(final_match.clone());
        assert_eq!(state.active_replay_summary(), None);
        assert_eq!(state.full_session_summary(), Some(&final_match));

        state.match_started();
        assert_eq!(state.active_replay_summary(), None);
        assert_eq!(state.full_session_summary(), None);
    }

    #[test]
    fn write_marker_sidecar_keeps_player_summary_without_markers() {
        let dir = TestDir::new("clipline-service", "sidecar-summary");
        let path = dir.path().join("clip.mp4");
        let (tx, _rx) = std::sync::mpsc::channel();
        let summary = PlayerSummary {
            champion_name: "Nautilus".into(),
            kills: 3,
            deaths: 4,
            assists: 23,
            creep_score: Some(187),
            game_time_s: Some(1800),
            player_name: String::new(),
            team: String::new(),
            participants: Vec::new(),
            summoner_spells: Vec::new(),
            items: Vec::new(),
        };

        let count = write_marker_sidecar(
            &tx,
            &MarkerLog::new(),
            &path,
            0.0,
            10.0,
            Some(&summary),
            &[],
        );

        assert_eq!(count, 0);
        let json = std::fs::read_to_string(path.with_extension("markers.json")).unwrap();
        let sidecar: clipline_events::ClipMarkers = serde_json::from_str(&json).unwrap();
        assert!(sidecar.markers.is_empty());
        assert_eq!(sidecar.player_summary, Some(summary));
    }

    #[test]
    fn write_marker_sidecar_writes_a_bookmark_only_clip() {
        let dir = TestDir::new("clipline-service", "sidecar-bookmarks");
        let path = dir.path().join("clip.mp4");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut marker_log = MarkerLog::new();
        marker_log.push_bookmark(4.0);
        marker_log.push_bookmark(9.0);
        // Outside the clip window, so it must not reach the sidecar.
        marker_log.push_bookmark(30.0);

        let count = write_marker_sidecar(&tx, &marker_log, &path, 0.0, 10.0, None, &[]);

        assert_eq!(count, 2, "bookmarks count toward the clip's markers");
        let json = std::fs::read_to_string(path.with_extension("markers.json")).unwrap();
        let sidecar: clipline_events::ClipMarkers = serde_json::from_str(&json).unwrap();
        assert!(sidecar.markers.is_empty());
        assert_eq!(
            sidecar
                .bookmarks
                .iter()
                .map(|bookmark| bookmark.t_s)
                .collect::<Vec<_>>(),
            [4.0, 9.0],
            "bookmarks survive the review-event filter"
        );
    }

    #[test]
    fn write_marker_sidecar_keeps_audio_tracks_without_markers() {
        let dir = TestDir::new("clipline-service", "sidecar-audio-tracks");
        let path = dir.path().join("clip.mp4");
        let (tx, _rx) = std::sync::mpsc::channel();
        let tracks = vec![audio_track("output", 0, "Output Audio", "output")];

        let count = write_marker_sidecar(&tx, &MarkerLog::new(), &path, 0.0, 10.0, None, &tracks);

        assert_eq!(count, 0);
        let json = std::fs::read_to_string(path.with_extension("markers.json")).unwrap();
        let sidecar: clipline_events::ClipMarkers = serde_json::from_str(&json).unwrap();
        assert!(sidecar.markers.is_empty());
        assert_eq!(sidecar.audio_tracks, tracks);
    }

    #[test]
    fn write_marker_sidecar_keeps_review_events_for_match_event_filters() {
        let dir = TestDir::new("clipline-service", "sidecar-review-events");
        let path = dir.path().join("clip.mp4");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut log = MarkerLog::new();
        log.push(review_event(
            EventKind::ChampionKill,
            "Enemy Mid",
            Some("Ally Top"),
            12.0,
            false,
        ));
        log.push(review_event(
            EventKind::ChampionAssist,
            "Dain",
            Some("Enemy Mid"),
            14.0,
            true,
        ));
        log.push(review_event(
            EventKind::HeraldKill,
            "Ally Jungle",
            None,
            16.0,
            false,
        ));
        log.push(review_event(
            EventKind::MinionsSpawning,
            "",
            None,
            18.0,
            false,
        ));

        let count = write_marker_sidecar(&tx, &log, &path, 10.0, 20.0, None, &[]);

        assert_eq!(count, 3);
        let json = std::fs::read_to_string(path.with_extension("markers.json")).unwrap();
        let sidecar: clipline_events::ClipMarkers = serde_json::from_str(&json).unwrap();
        let kinds: Vec<_> = sidecar
            .markers
            .iter()
            .map(|marker| marker.event.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![
                EventKind::ChampionKill,
                EventKind::ChampionAssist,
                EventKind::HeraldKill,
            ]
        );
        assert_eq!(sidecar.markers[0].event.actor, "Enemy Mid");
        assert!(!sidecar.markers[0].event.involves_local_player);
        assert!((sidecar.markers[0].t_s - 2.0).abs() < 1e-9);
    }

    #[test]
    fn cadenced_capture_takes_ownership_of_seed_payload_without_clone() {
        let payload = vec![7, 8, 9, 10];
        let original = payload.as_ptr();
        let seed = Frame {
            pts_s: 1.0,
            data: FrameData::Cpu(payload),
        };

        let cap = CadencedCapture::new(TimeoutSource, 60, seed);

        let Some(FrameData::Cpu(retained)) = cap.last_data.as_ref() else {
            panic!("CPU seed must remain a CPU frame");
        };
        assert_eq!(
            retained.as_ptr(),
            original,
            "constructor must move the seed allocation instead of cloning it"
        );
    }

    #[test]
    fn cadenced_capture_duplicates_seed_on_idle_timeout() {
        let seed = Frame {
            pts_s: 1.0,
            data: FrameData::Cpu(vec![7, 8, 9]),
        };
        let mut cap = CadencedCapture::new(TimeoutSource, 60, seed);

        let first = cap
            .next_frame()
            .expect("duplicate frame")
            .expect("capture still open");
        let second = cap
            .next_frame()
            .expect("duplicate frame")
            .expect("capture still open");

        assert!((first.pts_s - (1.0 + 1.0 / 60.0)).abs() < 1e-9);
        assert!((second.pts_s - (1.0 + 2.0 / 60.0)).abs() < 1e-9);
        assert!(matches!(first.data, FrameData::Cpu(ref data) if data == &[7, 8, 9]));
        assert!(matches!(second.data, FrameData::Cpu(ref data) if data == &[7, 8, 9]));
    }

    #[test]
    fn cadenced_capture_premature_timeouts_cannot_inflate_pts_past_wall_time() {
        let fps = 60;
        let frame_interval_s = 1.0 / fps as f64;
        let seed = Frame {
            pts_s: 1.0,
            data: FrameData::Cpu(vec![7, 8, 9]),
        };
        let seed_pts_s = seed.pts_s;
        let mut cap = CadencedCapture::new(
            PrematureTimeoutSource {
                delay: Duration::from_millis(1),
            },
            fps,
            seed,
        );
        let started = Instant::now();
        let mut last_pts_s = seed_pts_s;

        for _ in 0..120 {
            match cap.next_frame() {
                Ok(Some(frame)) => last_pts_s = frame.pts_s,
                Err(CaptureError::Timeout(_)) => {}
                other => panic!("unexpected capture result: {other:?}"),
            }
        }

        let wall_elapsed_s = started.elapsed().as_secs_f64();
        let pts_elapsed_s = last_pts_s - seed_pts_s;
        assert!(
            pts_elapsed_s <= wall_elapsed_s + frame_interval_s,
            "premature timeouts inflated PTS: pts={pts_elapsed_s:.6}s wall={wall_elapsed_s:.6}s"
        );
    }

    #[test]
    fn cadenced_capture_propagates_target_closure_instead_of_duplicating() {
        let seed = Frame {
            pts_s: 1.0,
            data: FrameData::Cpu(vec![7, 8, 9]),
        };
        let source = ScriptedTimedSource {
            outcomes: VecDeque::from([Ok(None)]),
            requested_timeouts: Vec::new(),
        };
        let mut capture = CadencedCapture::new(source, 60, seed);

        assert!(capture
            .next_frame()
            .expect("closed source is not an error")
            .is_none());
    }

    #[test]
    fn cadenced_capture_suppresses_stale_real_frame_after_timeout_duplicate() {
        let fps = 60;
        let interval_s = 1.0 / fps as f64;
        let seed = Frame {
            pts_s: 1.0,
            data: FrameData::Cpu(vec![1]),
        };
        let stale_pts_s = 1.0 + interval_s + 0.00005;
        let scheduled_pts_s = 1.0 + 2.0 * interval_s;
        let source = ScriptedTimedSource {
            outcomes: VecDeque::from([
                Err(CaptureError::Timeout(Duration::ZERO)),
                Ok(Some(Frame {
                    pts_s: stale_pts_s,
                    data: FrameData::Cpu(vec![2]),
                })),
                Ok(Some(Frame {
                    pts_s: scheduled_pts_s,
                    data: FrameData::Cpu(vec![3]),
                })),
            ]),
            requested_timeouts: Vec::new(),
        };
        let mut cap = CadencedCapture::new(source, fps, seed);

        let duplicate = cap.next_frame().unwrap().unwrap();
        let skipped = cap.next_frame();

        assert!((duplicate.pts_s - (1.0 + interval_s)).abs() < 1e-9);
        let skipped_for = match skipped {
            Err(CaptureError::Timeout(duration)) => duration,
            other => panic!("expected bounded stale-frame timeout, got {other:?}"),
        };
        assert_eq!(cap.inner.requested_timeouts.len(), 2);

        let next = cap.next_frame().unwrap().unwrap();

        assert!((next.pts_s - scheduled_pts_s).abs() < 1e-9);
        assert!(matches!(next.data, FrameData::Cpu(ref data) if data == &[3]));
        assert_eq!(cap.inner.requested_timeouts.len(), 3);
        assert!(cap.inner.requested_timeouts[0] <= cap.frame_interval);
        assert!(cap.inner.requested_timeouts[1] <= cap.frame_interval);
        assert!(cap.inner.requested_timeouts[2] <= skipped_for);
        let remaining_s = skipped_for.as_secs_f64();
        let pts_remaining_s = scheduled_pts_s - stale_pts_s;
        assert!(remaining_s <= pts_remaining_s + 1e-9);
        assert!(
            remaining_s >= pts_remaining_s - 0.005,
            "stale retry lost its deadline: remaining={remaining_s:.6}s expected={pts_remaining_s:.6}s"
        );
    }

    #[test]
    fn cadenced_capture_timeout_uses_latest_suppressed_frame_data() {
        let fps = 60;
        let interval_s = 1.0 / fps as f64;
        let seed = Frame {
            pts_s: 1.0,
            data: FrameData::Cpu(vec![1]),
        };
        let source = ScriptedTimedSource {
            outcomes: VecDeque::from([
                Err(CaptureError::Timeout(Duration::ZERO)),
                Ok(Some(Frame {
                    pts_s: 1.0 + interval_s + 0.00005,
                    data: FrameData::Cpu(vec![2]),
                })),
                Err(CaptureError::Timeout(Duration::ZERO)),
            ]),
            requested_timeouts: Vec::new(),
        };
        let mut cap = CadencedCapture::new(source, fps, seed);

        let first = cap.next_frame().unwrap().unwrap();
        let skipped = cap.next_frame();
        let second = cap.next_frame().unwrap().unwrap();

        assert!(matches!(first.data, FrameData::Cpu(ref data) if data == &[1]));
        assert!(matches!(skipped, Err(CaptureError::Timeout(_))));
        assert!((second.pts_s - (1.0 + 2.0 * interval_s)).abs() < 1e-9);
        assert!(matches!(second.data, FrameData::Cpu(ref data) if data == &[2]));
    }

    #[test]
    fn cadenced_capture_stale_retry_keeps_the_original_wait_deadline() {
        let fps = 60;
        let interval_s = 1.0 / fps as f64;
        let seed = Frame {
            pts_s: 1.0,
            data: FrameData::Cpu(vec![1]),
        };
        let source = DelayedFrameSource {
            frame: Some(Frame {
                pts_s: 1.0 + interval_s / 2.0,
                data: FrameData::Cpu(vec![2]),
            }),
            delay: Duration::from_millis(30),
            requested_timeouts: Vec::new(),
        };
        let mut cap = CadencedCapture::new(source, fps, seed);

        assert!(matches!(cap.next_frame(), Err(CaptureError::Timeout(_))));
        let duplicate = cap.next_frame().unwrap().unwrap();

        assert!((duplicate.pts_s - (1.0 + interval_s)).abs() < 1e-9);
        assert!(matches!(duplicate.data, FrameData::Cpu(ref data) if data == &[2]));
        assert!(
            cap.inner.requested_timeouts[1] <= Duration::from_millis(1),
            "retry restarted the cadence wait: {:?}",
            cap.inner.requested_timeouts
        );
    }

    #[test]
    fn cadenced_capture_counts_encoder_work_against_the_next_deadline() {
        let fps = 60;
        let interval_s = 1.0 / fps as f64;
        let seed = Frame {
            pts_s: 1.0,
            data: FrameData::Cpu(vec![1]),
        };
        let source = BlockingTimeoutSource {
            requested_timeouts: Vec::new(),
        };
        let mut cap = CadencedCapture::new(source, fps, seed);

        let first = cap.next_frame().unwrap().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let second = cap.next_frame().unwrap().unwrap();

        assert!(
            cap.inner.requested_timeouts[1] <= Duration::from_millis(1),
            "encoder work restarted the cadence wait: {:?}",
            cap.inner.requested_timeouts
        );
        assert!(
            second.pts_s - first.pts_s >= 3.0 * interval_s - 1e-9,
            "missed wall-clock slots were not reflected in PTS: first={}, second={}",
            first.pts_s,
            second.pts_s
        );
    }

    #[test]
    fn cadenced_capture_counts_delayed_real_frame_delivery() {
        let fps = 60;
        let interval_s = 1.0 / fps as f64;
        let seed = Frame {
            pts_s: 1.0,
            data: FrameData::Cpu(vec![1]),
        };
        let source = DelayedFrameSource {
            frame: Some(Frame {
                pts_s: 1.0 + interval_s,
                data: FrameData::Cpu(vec![2]),
            }),
            delay: Duration::from_millis(30),
            requested_timeouts: Vec::new(),
        };
        let mut cap = CadencedCapture::new(source, fps, seed);

        let real = cap.next_frame().unwrap().unwrap();
        let duplicate = cap.next_frame().unwrap().unwrap();

        assert!(
            cap.inner.requested_timeouts[1] <= Duration::from_millis(5),
            "late real-frame delivery restarted the cadence wait: {:?}",
            cap.inner.requested_timeouts
        );
        assert!((duplicate.pts_s - (real.pts_s + interval_s)).abs() < 1e-9);
    }

    #[test]
    fn manual_replay_save_does_not_shrink_after_previous_save() {
        let dir = TestDir::new("clipline-service", "manual-save-window");
        let mut rec = Recorder::new(
            MockCapture::new(120, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        );
        rec.run_to_end().unwrap();

        let first_path = dir.path().join("clip_first.mp4");
        let second_path = dir.path().join("clip_second.mp4");
        let (first_end, first_seconds) = save(&rec, &first_path, 2.0).unwrap();
        let (second_end, second_seconds) = save(&rec, &second_path, 2.0).unwrap();

        assert!((first_end - 4.0).abs() < 1e-6);
        assert!((second_end - 4.0).abs() < 1e-6);
        assert!((first_seconds - 2.0).abs() < 1e-6);
        assert!((second_seconds - 2.0).abs() < 1e-6);
        assert_eq!(
            std::fs::read(first_path.with_extension("clipline.json")).unwrap(),
            b"{}"
        );
        assert_eq!(
            std::fs::read(second_path.with_extension("clipline.json")).unwrap(),
            b"{}"
        );
    }

    #[test]
    fn failed_replay_save_removes_only_a_new_ownership_marker() {
        let dir = TestDir::new("clipline-service", "failed-save-marker-cleanup");
        let mut rec = Recorder::new(
            MockCapture::new(1, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        );
        rec.run_to_end().unwrap();

        let newly_marked = dir.path().join("new.mp4");
        std::fs::create_dir(&newly_marked).unwrap();
        assert!(save(&rec, &newly_marked, 1.0).is_err());
        assert!(!newly_marked.with_extension("clipline.json").exists());

        let already_marked = dir.path().join("existing.mp4");
        std::fs::create_dir(&already_marked).unwrap();
        ensure_clip_owned(&already_marked).unwrap();
        assert!(save(&rec, &already_marked, 1.0).is_err());
        assert_eq!(
            std::fs::read(already_marked.with_extension("clipline.json")).unwrap(),
            b"{}"
        );
    }

    #[test]
    fn clips_dir_uses_configured_root_when_creatable() {
        let dir = TestDir::new("clipline-service", "configured-root");
        let configured = dir.path().join("media");

        let (resolved, fell_back) =
            clips_dir_resolved(&configured, || panic!("must not fall back")).unwrap();

        assert!(!fell_back);
        assert_eq!(resolved, configured);
        assert!(configured.is_dir());
    }

    #[test]
    fn clips_dir_falls_back_when_configured_root_is_unusable() {
        let dir = TestDir::new("clipline-service", "unusable-root");
        // A directory cannot be created under a regular file, so this stands in
        // for an unreachable root (e.g. an unplugged drive).
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, b"x").unwrap();
        let unusable = blocker.join("clipline");
        let fallback = dir.path().join("fallback");

        let (resolved, fell_back) = clips_dir_resolved(&unusable, || fallback.clone()).unwrap();

        assert!(fell_back);
        assert_eq!(resolved, fallback);
        assert!(fallback.is_dir());
    }

    #[test]
    fn clips_dir_falls_back_when_existing_configured_root_is_not_writable() {
        let dir = TestDir::new("clipline-service", "unwritable-existing-root");
        let configured = dir.path().join("configured");
        let fallback = dir.path().join("fallback");
        std::fs::create_dir_all(&configured).unwrap();
        let mut probed = Vec::new();

        let (resolved, fell_back) = clips_dir_resolved_with_probe(
            &configured,
            || fallback.clone(),
            |candidate| {
                probed.push(candidate.to_path_buf());
                if candidate == configured {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "injected ACL denial",
                    ))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap();

        assert!(fell_back);
        assert_eq!(resolved, fallback);
        assert_eq!(probed, [configured, fallback]);
    }

    #[test]
    fn writable_directory_probe_leaves_no_probe_file() {
        let dir = TestDir::new("clipline-service", "writable-root-probe");
        let media = dir.path().join("media");
        std::fs::create_dir_all(&media).unwrap();
        std::fs::write(media.join("existing.txt"), b"keep").unwrap();

        probe_writable_directory(&media).unwrap();

        let mut names = std::fs::read_dir(&media)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, [std::ffi::OsString::from("existing.txt")]);
    }

    #[test]
    fn clips_dir_reports_configured_and_fallback_probe_failures() {
        let dir = TestDir::new("clipline-service", "double-probe-failure");
        let configured = dir.path().join("configured");
        let fallback = dir.path().join("fallback");

        let error = clips_dir_resolved_with_probe(
            &configured,
            || fallback.clone(),
            |_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected denial",
                ))
            },
        )
        .unwrap_err();

        assert!(error.contains(&configured.display().to_string()), "{error}");
        assert!(error.contains(&fallback.display().to_string()), "{error}");
    }

    #[test]
    fn temp_guard_flags_clips_inside_temp_root() {
        let dir = TestDir::new("clipline-service", "temp-guard");
        let temp_root = dir.path().join("temp");
        let inside = temp_root.join("Videos").join("Clipline");
        std::fs::create_dir_all(&inside).unwrap();

        assert!(is_within_temp(&inside, &temp_root));
    }

    #[test]
    fn temp_guard_allows_clips_outside_temp_root() {
        let dir = TestDir::new("clipline-service", "temp-guard-outside");
        let temp_root = dir.path().join("temp");
        let outside = dir.path().join("media").join("Clipline");
        std::fs::create_dir_all(&temp_root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        assert!(!is_within_temp(&outside, &temp_root));
    }

    #[test]
    fn full_session_temp_reservation_skips_existing_temp() {
        let dir = TestDir::new("clipline-service", "session-temp-reservation");
        let stamp = 1_725_000_000;
        let occupied_final = dir.path().join(format!("session_{stamp}.mp4"));
        let occupied_temp = occupied_final.with_extension("mp4.recording");
        let sentinel = b"active recorder bytes";
        std::fs::write(&occupied_temp, sentinel).unwrap();
        let occupied_suffix_final = dir.path().join(format!("session_{stamp}_1.mp4"));
        std::fs::write(&occupied_suffix_final, b"finished recording").unwrap();

        let (final_path, temp_path, _file) =
            reserve_full_session_path_at(dir.path(), "session", stamp).unwrap();

        assert_eq!(std::fs::read(&occupied_temp).unwrap(), sentinel);
        assert_ne!(temp_path, occupied_temp);
        assert_eq!(
            final_path,
            dir.path().join(format!("session_{stamp}_2.mp4"))
        );
        assert_eq!(
            temp_path,
            dir.path().join(format!("session_{stamp}_2.mp4.recording"))
        );
        assert_eq!(
            std::fs::read(final_path.with_extension("clipline.json")).unwrap(),
            b"{}"
        );
    }

    #[test]
    fn media_path_reservation_skips_orphaned_ownership_markers() {
        let dir = TestDir::new("clipline-service", "ownership-marker-reservation");
        let stamp = 1_725_000_002;
        let occupied = dir.path().join(format!("clip_{stamp}.mp4"));
        ensure_clip_owned(&occupied).unwrap();

        let replay = unique_media_path_at(dir.path(), "clip", stamp);
        let (session, temp, _file) =
            reserve_full_session_path_at(dir.path(), "session", stamp).unwrap();

        assert_eq!(replay, dir.path().join(format!("clip_{stamp}_1.mp4")));
        assert_eq!(session, dir.path().join(format!("session_{stamp}.mp4")));
        assert!(temp.exists());
        assert!(session.with_extension("clipline.json").is_file());
    }

    #[test]
    fn full_session_temp_reservation_retries_when_final_appears_during_reservation() {
        let dir = TestDir::new("clipline-service", "session-finalization-race");
        let stamp = 1_725_000_001;
        let raced_final = dir.path().join(format!("session_{stamp}.mp4"));
        let raced_temp = raced_final.with_extension("mp4.recording");
        let mut finalize_before_first_reservation = true;

        let (final_path, temp_path, _file) = reserve_full_session_path_at_with(
            dir.path(),
            "session",
            stamp,
            |candidate_final, candidate_temp| {
                if finalize_before_first_reservation {
                    finalize_before_first_reservation = false;
                    std::fs::write(candidate_final, b"old finalized recording").unwrap();
                }
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(candidate_temp)
            },
        )
        .unwrap();

        assert_eq!(
            std::fs::read(&raced_final).unwrap(),
            b"old finalized recording"
        );
        assert!(!raced_temp.exists());
        assert_eq!(
            final_path,
            dir.path().join(format!("session_{stamp}_1.mp4"))
        );
        assert_eq!(
            temp_path,
            dir.path().join(format!("session_{stamp}_1.mp4.recording"))
        );
    }

    #[test]
    fn finalized_session_rename_accepts_preexisting_final_file() {
        let dir = TestDir::new("clipline-service", "session-rename-recovered");
        let final_path = dir.path().join("session.mp4");
        std::fs::write(&final_path, b"mp4").unwrap();
        let recording = FullSessionRecording {
            final_path,
            temp_path: dir.path().join("session.mp4.recording"),
            wall_start_unix: 0,
        };
        let (tx, rx) = mpsc::channel();

        assert!(rename_finalized_session(&recording, &tx));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn finalized_session_rename_preserves_non_empty_temp_on_failure() {
        let dir = TestDir::new("clipline-service", "session-rename-preserve");
        let temp_path = dir.path().join("session.mp4.recording");
        std::fs::write(&temp_path, b"recoverable hybrid mp4").unwrap();
        let recording = FullSessionRecording {
            final_path: dir.path().join("missing-parent").join("session.mp4"),
            temp_path: temp_path.clone(),
            wall_start_unix: 0,
        };
        let (tx, rx) = mpsc::channel();

        assert!(!rename_finalized_session(&recording, &tx));
        assert_eq!(
            std::fs::read(&temp_path).unwrap(),
            b"recoverable hybrid mp4"
        );
        let Event::Error { message } = rx.try_recv().unwrap() else {
            panic!("expected recovery warning");
        };
        assert!(message.contains("recoverable"), "{message}");
        assert!(message.contains("session.mp4.recording"), "{message}");
    }

    #[test]
    fn failed_full_session_finish_preserves_non_empty_and_removes_empty_temp() {
        let dir = TestDir::new("clipline-service", "session-finish-preserve");
        let recoverable = dir.path().join("recoverable.mp4.recording");
        let empty = dir.path().join("empty.mp4.recording");
        std::fs::write(&recoverable, b"hybrid mp4").unwrap();
        std::fs::write(&empty, b"").unwrap();
        ensure_clip_owned(&recoverable).unwrap();
        ensure_clip_owned(&empty).unwrap();
        let (tx, rx) = mpsc::channel();

        handle_full_session_finish_error(&recoverable, &tx, "writer failed");
        handle_full_session_finish_error(&empty, &tx, "writer failed");

        assert!(recoverable.exists());
        assert!(clip_ownership_marker_path(&recoverable).unwrap().exists());
        assert!(!empty.exists());
        assert!(!clip_ownership_marker_path(&empty).unwrap().exists());
        let Event::Error { message } = rx.try_recv().unwrap() else {
            panic!("expected recovery warning");
        };
        assert!(message.contains("recoverable.mp4.recording"), "{message}");
    }

    #[test]
    fn finalized_session_rename_warns_when_temp_and_final_are_missing() {
        let dir = TestDir::new("clipline-service", "session-rename-missing");
        let recording = FullSessionRecording {
            final_path: dir.path().join("session.mp4"),
            temp_path: dir.path().join("session.mp4.recording"),
            wall_start_unix: 0,
        };
        let (tx, rx) = mpsc::channel();

        assert!(!rename_finalized_session(&recording, &tx));
        let Event::Error { message } = rx.try_recv().unwrap() else {
            panic!("expected warning");
        };
        assert!(message.contains("finalize full session"));
    }

    #[test]
    fn replay_cache_sweep_removes_stale_instance_and_preserves_live_quota() {
        let dir = TestDir::new("clipline-service", "replay-cache-sweep");
        let stale = dir.path().join("clipline-replay-cache-100-41-0");
        let live = dir.path().join("clipline-replay-cache-101-42-0");
        let unrelated = dir.path().join("somebody-elses-folder");
        for run in [&stale, &live, &unrelated] {
            std::fs::create_dir(run).unwrap();
        }
        write_replay_cache_owner(
            &stale,
            &ReplayCacheOwner {
                process_instance_id: "41:1000".into(),
                created_at_unix: 100,
            },
        )
        .unwrap();
        write_replay_cache_owner(
            &live,
            &ReplayCacheOwner {
                process_instance_id: "42:2000".into(),
                created_at_unix: 101,
            },
        )
        .unwrap();
        std::fs::write(stale.join("seg.bin"), vec![1; 17]).unwrap();
        std::fs::write(live.join("seg.bin"), vec![2; 23]).unwrap();
        std::fs::write(unrelated.join("keep.txt"), b"keep").unwrap();

        let preserved = sweep_replay_cache_runs(
            dir.path(),
            SystemTime::now() + Duration::from_secs(48 * 60 * 60),
            |pid| match pid {
                41 => Ok("41:9999".into()),
                42 => Ok("42:2000".into()),
                _ => Err("unexpected pid".into()),
            },
        )
        .unwrap();

        assert!(!stale.exists());
        assert!(live.exists());
        assert!(unrelated.exists());
        assert!(preserved >= 23);
    }

    #[test]
    fn replay_cache_sweep_preserves_ambiguous_fresh_run() {
        let dir = TestDir::new("clipline-service", "replay-cache-ambiguous");
        let run = dir.path().join("clipline-replay-cache-100-42-0");
        std::fs::create_dir(&run).unwrap();
        std::fs::write(run.join("seg.bin"), vec![3; 29]).unwrap();

        let preserved = sweep_replay_cache_runs(dir.path(), SystemTime::now(), |_| {
            Err("process cannot be queried".into())
        })
        .unwrap();

        assert!(run.exists());
        assert_eq!(preserved, 29);
    }

    #[test]
    fn replay_cache_sweep_removes_ambiguous_run_only_after_grace_period() {
        let dir = TestDir::new("clipline-service", "replay-cache-aged");
        let run = dir.path().join("clipline-replay-cache-100-42-0");
        std::fs::create_dir(&run).unwrap();
        std::fs::write(run.join("seg.bin"), vec![4; 31]).unwrap();

        let preserved = sweep_replay_cache_runs(
            dir.path(),
            SystemTime::now() + Duration::from_secs(25 * 60 * 60),
            |_| Err("process cannot be queried".into()),
        )
        .unwrap();

        assert!(!run.exists());
        assert_eq!(preserved, 0);
    }

    #[test]
    fn prepared_replay_storage_cleans_untransferred_run() {
        let dir = TestDir::new("clipline-service", "replay-cache-construction");
        let run = dir.path().join("clipline-replay-cache-100-42-0");
        std::fs::create_dir(&run).unwrap();
        std::fs::write(run.join(REPLAY_CACHE_OWNER_FILE), b"owned").unwrap();

        drop(PreparedReplayStorage::disk(run.clone(), 1024));

        assert!(!run.exists());
    }

    #[test]
    fn low_space_runtime_failure_always_finalizes_and_keeps_primary_error() {
        let finalized = std::cell::Cell::new(false);

        let message = finalize_runtime_failure("replay cache disk is low".into(), || {
            finalized.set(true);
            Some("finish: writer failed".into())
        });

        assert!(finalized.get());
        assert!(message.starts_with("replay cache disk is low"), "{message}");
        assert!(message.contains("finish: writer failed"), "{message}");
    }
}
