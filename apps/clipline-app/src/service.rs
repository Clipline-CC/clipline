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
    clip_ownership_marker_path, ensure_clip_owned, ensure_session_clip_owned,
    recover_recording_files, remove_clip_ownership_marker, remove_emptied_session_dir_after_clip,
    reserve_session_recording_file, storage_status, sweep_emptied_session_dirs,
    write_session_metadata, StorageStatus,
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
const REPLAY_CACHE_RUN_PREFIX: &str = clipline_storage::REPLAY_CACHE_RUN_PREFIX;
const REPLAY_CACHE_OWNER_FILE: &str = clipline_storage::REPLAY_CACHE_OWNER_FILE;
const AMBIGUOUS_REPLAY_CACHE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
#[path = "service/media_root.rs"]
mod media_root;
#[path = "service/capture.rs"]
mod capture;
#[path = "service/config.rs"]
mod config;
#[path = "service/encoders.rs"]
mod encoders;
#[path = "service/replay.rs"]
mod replay;
#[path = "service/session.rs"]
mod session;
#[cfg(test)]
#[path = "service/tests.rs"]
mod tests;

use self::capture::*;
use self::encoders::*;
use self::replay::*;
use self::session::*;

pub use self::config::{
    ActiveGame, AudioChannelMode, AudioOptions, CaptureBackend, CaptureRegion, CaptureSource,
    EncoderOption, OutputResolution, OutputResolutionBounds, RecordingMode, ReplayStorageOptions,
    VideoEncoder, available_encoder_options, encoder_label,
};
pub use self::encoders::refresh_ffmpeg_encoder_capabilities;
pub use self::replay::DEFAULT_DISK_QUOTA_BYTES;

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
                    let path = unique_media_path(&session_dir, "clip");
                    match save(
                        &rec,
                        &path,
                        opts.replay_window_s,
                        opts.active_game.as_ref(),
                        league_queue.as_ref(),
                    ) {
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
                            cleanup_discarded_session(&path, &clips_dir);
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

fn warn_user(events: &Sender<Event>, message: String) {
    let _ = events.send(Event::Error { message });
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
