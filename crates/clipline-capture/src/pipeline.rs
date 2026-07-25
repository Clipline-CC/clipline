use std::io::{self, Seek, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use clipline_buffer::{
    DiskReplayRing, DiskSegment, DiskTrackRef, ReplayRing, SampleInfo, Segment, TrackSamples,
};
use clipline_mp4::{FragSampleRef, HybridMp4Writer, SourceSample, TrackConfig};

use crate::traits::{
    AudioPacket, AudioSource, CaptureEngine, CaptureError, EncodeError, EncodedPacket, Encoder,
};

const MAX_PENDING_GOP_BYTES: usize = 64 * 1024 * 1024;
/// Normal replay GOPs are about 500 ms. This generous ceiling prevents a
/// broken encoder from retaining an arbitrarily long video/audio segment.
const MAX_PENDING_GOP_DURATION_S: f64 = 10.0;
const FULL_SESSION_QUEUE_MAX_BYTES: usize = 128 * 1024 * 1024;
const FULL_SESSION_QUEUE_MAX_SEGMENTS: usize = 8;
const MID_STREAM_REPLAY_OPUS_PRE_SKIP: u16 = 960; // One 20 ms Opus frame at 48 kHz.

#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error(transparent)]
    Capture(#[from] CaptureError),
    #[error(transparent)]
    Encode(#[from] EncodeError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Where the rolling replay buffer lives, and how much it retains.
///
/// `retention_s` bounds retention by wall-clock span as well as bytes. The byte
/// budget carries an encoder-overshoot headroom, so on its own it lets the
/// buffer grow to the whole budget — roughly twice the intended span, and
/// further when the encoder undershoots. `f64::INFINITY` disables the bound.
#[derive(Debug)]
pub enum ReplayStorageConfig {
    Memory {
        max_bytes: usize,
        retention_s: f64,
    },
    Disk {
        max_bytes: usize,
        retention_s: f64,
        dir: PathBuf,
    },
}

enum ReplayStorage {
    Memory(ReplayRing),
    Disk(DiskReplayRing),
}

enum ReplayWindow<'a> {
    Memory(Vec<&'a Segment>),
    Disk(Vec<&'a DiskSegment>),
}

impl ReplayWindow<'_> {
    fn bounds(&self) -> Option<(f64, f64)> {
        match self {
            Self::Memory(segments) => {
                Some((segments.first()?.pts_start_s, segments.last()?.pts_end_s()))
            }
            Self::Disk(segments) => {
                Some((segments.first()?.pts_start_s, segments.last()?.pts_end_s()))
            }
        }
    }
}

impl ReplayStorage {
    fn len(&self) -> usize {
        match self {
            Self::Memory(ring) => ring.len(),
            Self::Disk(ring) => ring.len(),
        }
    }

    fn bytes(&self) -> usize {
        match self {
            Self::Memory(ring) => ring.bytes(),
            Self::Disk(ring) => ring.bytes(),
        }
    }

    fn buffered_span_s(&self) -> f64 {
        match self {
            Self::Memory(ring) => segment_span(
                ring.segments()
                    .map(|segment| (segment.pts_start_s, segment.pts_end_s())),
            ),
            Self::Disk(ring) => segment_span(
                ring.segments()
                    .map(|segment| (segment.pts_start_s, segment.pts_end_s())),
            ),
        }
    }

    fn save_window_bounds(
        &self,
        window_s: f64,
        exclude_before_s: Option<f64>,
    ) -> Option<(f64, f64)> {
        self.save_window(window_s, exclude_before_s).bounds()
    }

    fn save_window(&self, window_s: f64, exclude_before_s: Option<f64>) -> ReplayWindow<'_> {
        match self {
            Self::Memory(ring) => {
                ReplayWindow::Memory(ring.save_window(window_s, exclude_before_s))
            }
            Self::Disk(ring) => ReplayWindow::Disk(ring.save_window(window_s, exclude_before_s)),
        }
    }

    fn push(&mut self, segment: Arc<Segment>) -> io::Result<()> {
        match self {
            Self::Memory(ring) => ring.push_shared(segment),
            Self::Disk(ring) => ring.push_ref(&segment)?,
        }
        Ok(())
    }
}

fn segment_span(mut segments: impl Iterator<Item = (f64, f64)>) -> f64 {
    let Some((first_start, first_end)) = segments.next() else {
        return 0.0;
    };
    segments.fold(first_end - first_start, |_, (_, end)| end - first_start)
}

pub trait WriteSeek: Write + Seek + Send {}

impl<T: Write + Seek + Send> WriteSeek for T {}

#[derive(Debug)]
pub struct FullSessionSummary {
    pub start_s: f64,
    pub end_s: f64,
    pub duration_s: f64,
}

struct FullSessionSink {
    tx: SyncSender<FullSessionWriteMsg>,
    join: JoinHandle<()>,
    queued_bytes: Arc<AtomicUsize>,
    max_queue_bytes: usize,
    audio_cfgs: Vec<clipline_mp4::AudioTrackConfig>,
    video_cfg: Option<clipline_mp4::VideoTrackConfig>,
    start_s: Option<f64>,
    end_s: Option<f64>,
    send_error: Option<String>,
}

struct FullSessionSegment {
    video_cfg: clipline_mp4::VideoTrackConfig,
    audio_cfgs: Vec<clipline_mp4::AudioTrackConfig>,
    segment: Arc<Segment>,
    reserved_bytes: usize,
}

enum FullSessionWriteMsg {
    Segment(FullSessionSegment),
    Finish(Sender<io::Result<()>>),
}

/// The recording pipeline (ddoc §3): capture → encode → GOP-aligned
/// segments → replay ring. Synchronous pull loop; production runs it on a
/// dedicated thread.
pub struct Recorder<C: CaptureEngine, E: Encoder> {
    capture: C,
    encoder: E,
    ring: ReplayStorage,
    pending: Vec<EncodedPacket>,
    /// Encoded video payload held for the current unsealed GOP.
    pending_bytes: usize,
    /// Encoded audio payload held across all tracks for the current GOP.
    pending_audio_bytes: usize,
    pending_byte_budget: usize,
    pre_keyframe_bytes: usize,
    pending_started_pts_s: Option<f64>,
    audio_sources: Vec<Box<dyn AudioSource>>,
    pending_audio: Vec<Vec<AudioPacket>>,
    /// pts of the first video packet — the recording's timeline start.
    /// Audio captured before it (engine-init lead-in) is dropped so both
    /// tracks begin together in the file.
    video_start_pts_s: Option<f64>,
    full_session: Option<FullSessionSink>,
}

impl<C: CaptureEngine, E: Encoder> Recorder<C, E> {
    /// In-memory replay buffer bounded only by bytes. A retention window cannot
    /// be derived from a byte budget, so callers that want one use
    /// [`Recorder::with_retention`] or [`ReplayStorageConfig`].
    pub fn new(capture: C, encoder: E, max_buffer_bytes: usize) -> Self {
        Self::with_retention(capture, encoder, max_buffer_bytes, f64::INFINITY)
    }

    /// In-memory replay buffer bounded by bytes and a retention window (s).
    pub fn with_retention(
        capture: C,
        encoder: E,
        max_buffer_bytes: usize,
        retention_s: f64,
    ) -> Self {
        Self {
            capture,
            encoder,
            ring: ReplayStorage::Memory(ReplayRing::with_retention(max_buffer_bytes, retention_s)),
            pending: Vec::new(),
            pending_bytes: 0,
            pending_audio_bytes: 0,
            pending_byte_budget: pending_byte_budget(max_buffer_bytes),
            pre_keyframe_bytes: 0,
            pending_started_pts_s: None,
            audio_sources: Vec::new(),
            pending_audio: Vec::new(),
            video_start_pts_s: None,
            full_session: None,
        }
    }

    pub fn new_with_replay_storage(
        capture: C,
        encoder: E,
        storage: ReplayStorageConfig,
    ) -> io::Result<Self> {
        let storage_max_bytes = match &storage {
            ReplayStorageConfig::Memory { max_bytes, .. } => *max_bytes,
            ReplayStorageConfig::Disk { max_bytes, .. } => *max_bytes,
        };
        let ring = match storage {
            ReplayStorageConfig::Memory {
                max_bytes,
                retention_s,
            } => ReplayStorage::Memory(ReplayRing::with_retention(max_bytes, retention_s)),
            ReplayStorageConfig::Disk {
                max_bytes,
                retention_s,
                dir,
            } => ReplayStorage::Disk(DiskReplayRing::with_retention(max_bytes, retention_s, dir)?),
        };
        Ok(Self {
            capture,
            encoder,
            ring,
            pending: Vec::new(),
            pending_bytes: 0,
            pending_audio_bytes: 0,
            pending_byte_budget: pending_byte_budget(storage_max_bytes),
            pre_keyframe_bytes: 0,
            pending_started_pts_s: None,
            audio_sources: Vec::new(),
            pending_audio: Vec::new(),
            video_start_pts_s: None,
            full_session: None,
        })
    }

    /// Attach an audio source as the next audio track (ddoc §10:
    /// game / mic / system).
    pub fn with_audio(mut self, source: Box<dyn AudioSource>) -> Self {
        self.audio_sources.push(source);
        self.pending_audio.push(Vec::new());
        self
    }

    /// Process one captured frame (audio drain → encode → GOP sealing).
    /// `Ok(false)` = the capture source ended. Errors pass through —
    /// callers running live decide how to treat `CaptureError::Timeout`
    /// (an idle screen delivers no frames; that is not fatal).
    pub fn step(&mut self) -> Result<bool, PipelineError> {
        self.step_with_frame(|_| {})
    }

    /// Process one captured frame and expose it before encoding. This keeps
    /// side-channel observers (like the app's low-rate preview) out of the
    /// core capture/encode path when they are not installed.
    pub fn step_with_frame(
        &mut self,
        mut observe: impl FnMut(&crate::traits::Frame),
    ) -> Result<bool, PipelineError> {
        let Some(frame) = self.capture.next_frame()? else {
            return Ok(false);
        };
        observe(&frame);
        self.poll_audio_until(frame.pts_s)?;
        for pkt in self.encoder.encode(&frame)? {
            self.push_encoded_packet(pkt)?;
        }
        self.validate_pending_limits(frame.pts_s)?;
        Ok(true)
    }

    /// End of stream: drain the encoder, drain audio to the final GOP's
    /// end, seal the trailing partial GOP.
    pub fn finish_stream(&mut self) -> Result<(), PipelineError> {
        for pkt in self.encoder.finish()? {
            self.push_encoded_packet(pkt)?;
        }
        if self.pending.is_empty()
            && self.video_start_pts_s.is_none()
            && self.pending_payload_bytes() > 0
        {
            return Err(EncodeError::Backend(format!(
                "encoder ended before producing an initial keyframe ({} bytes were dropped before the first keyframe)",
                self.pending_payload_bytes()
            ))
            .into());
        }
        if !self.pending.is_empty() {
            let end = self
                .pending
                .last()
                .map(|p| p.pts_s + p.duration_s)
                .unwrap_or(0.0);
            self.finish_audio_until(end)?;
            self.validate_pending_limits(end)?;
            self.seal_pending(f64::INFINITY)?;
        }
        Ok(())
    }

    /// Drive the loop until the capture source ends, sealing a segment at
    /// every GOP boundary (a keyframe closes the previous GOP). Audio
    /// sources are drained per frame; packets ride in the segment whose
    /// GOP interval contains them.
    pub fn run_to_end(&mut self) -> Result<(), PipelineError> {
        while self.step()? {}
        self.finish_stream()
    }

    pub fn ring(&self) -> Option<&ReplayRing> {
        match &self.ring {
            ReplayStorage::Memory(ring) => Some(ring),
            ReplayStorage::Disk(_) => None,
        }
    }

    pub fn ring_len(&self) -> usize {
        self.ring.len()
    }

    pub fn ring_bytes(&self) -> usize {
        self.ring.bytes()
    }

    pub fn buffered_span_s(&self) -> f64 {
        self.ring.buffered_span_s()
    }

    pub fn save_window_bounds(
        &self,
        window_s: f64,
        exclude_before_s: Option<f64>,
    ) -> Option<(f64, f64)> {
        self.ring.save_window_bounds(window_s, exclude_before_s)
    }

    pub fn encoder(&self) -> &E {
        &self.encoder
    }

    pub fn start_full_session<W: Write + Seek + Send + 'static>(&mut self, w: W) -> io::Result<()> {
        self.start_full_session_with_limits(
            w,
            FULL_SESSION_QUEUE_MAX_BYTES,
            FULL_SESSION_QUEUE_MAX_SEGMENTS,
        )
    }

    fn start_full_session_with_limits<W: Write + Seek + Send + 'static>(
        &mut self,
        w: W,
        max_queue_bytes: usize,
        max_queue_segments: usize,
    ) -> io::Result<()> {
        if self.full_session.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "full session already recording",
            ));
        }
        let audio_cfgs: Vec<_> = self
            .audio_sources
            .iter()
            .map(|source| source.track_config())
            .collect();
        if max_queue_bytes == 0 || max_queue_segments == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "full session queue limits must be non-zero",
            ));
        }
        let (tx, join, queued_bytes) =
            spawn_full_session_writer(Box::new(w) as Box<dyn WriteSeek>, max_queue_segments)?;
        self.full_session = Some(FullSessionSink {
            tx,
            join,
            queued_bytes,
            max_queue_bytes,
            audio_cfgs,
            video_cfg: None,
            start_s: None,
            end_s: None,
            send_error: None,
        });
        Ok(())
    }

    pub fn finish_full_session(&mut self) -> io::Result<Option<FullSessionSummary>> {
        let Some(sink) = self.full_session.take() else {
            return Ok(None);
        };
        let start_s = sink.start_s;
        let end_s = sink.end_s;
        let send_error = sink.send_error.clone();
        finish_full_session_writer(sink)?;
        if let Some(error) = send_error {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, error));
        }
        let Some(start_s) = start_s else {
            return Ok(None);
        };
        let end_s = end_s.unwrap_or(start_s);
        Ok(Some(FullSessionSummary {
            start_s,
            end_s,
            duration_s: end_s - start_s,
        }))
    }

    /// Save the trailing `window_s` seconds as a finalized Hybrid MP4
    /// written to `w` (ddoc §6). `exclude_before_s` is the smart
    /// no-overlap mode. Returns the writer and the end pts of the saved
    /// footage — pass it back as `exclude_before_s` next time.
    ///
    /// Erroring (rather than writing an empty file) when no new footage
    /// exists lets the hotkey handler tell the user "nothing new to save".
    pub fn save_replay<W: Write + Seek>(
        &self,
        w: W,
        window_s: f64,
        exclude_before_s: Option<f64>,
    ) -> io::Result<(W, f64)> {
        let window = self.ring.save_window(window_s, exclude_before_s);
        let Some((timeline_origin_s, end_pts)) = window.bounds() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "no new footage in window",
            ));
        };
        let video_cfg = self.encoder.track_config();
        let starts_at_stream_origin = self.replay_starts_at_stream_origin(timeline_origin_s);
        let audio_cfgs: Vec<_> = self
            .audio_sources
            .iter()
            .map(|s| {
                let mut cfg = s.track_config();
                if !starts_at_stream_origin {
                    cfg.pre_skip = MID_STREAM_REPLAY_OPUS_PRE_SKIP;
                }
                cfg
            })
            .collect();
        let mut track_cfgs = vec![TrackConfig::Video(video_cfg.clone())];
        for cfg in &audio_cfgs {
            track_cfgs.push(TrackConfig::Audio(cfg.clone()));
        }
        let mut writer = HybridMp4Writer::new_multi(w, track_cfgs)?;
        match window {
            ReplayWindow::Memory(segments) => {
                for segment in segments {
                    write_memory_replay_segment(
                        &mut writer,
                        segment,
                        &video_cfg,
                        &audio_cfgs,
                        timeline_origin_s,
                    )?;
                }
            }
            ReplayWindow::Disk(segments) => {
                for segment in segments {
                    write_disk_replay_segment(
                        &mut writer,
                        segment,
                        &video_cfg,
                        &audio_cfgs,
                        timeline_origin_s,
                    )?;
                }
            }
        }
        Ok((writer.finalize()?, end_pts))
    }

    fn replay_starts_at_stream_origin(&self, first_pts_start_s: f64) -> bool {
        let origin = self.video_start_pts_s.unwrap_or(first_pts_start_s);
        (first_pts_start_s - origin).abs() <= 1e-9
    }

    fn seal_pending(&mut self, boundary_pts_s: f64) -> Result<(), PipelineError> {
        let pts_start_s = self.pending[0].pts_s;
        let starts_with_keyframe = self.pending[0].is_keyframe;
        let video_timescale = self.encoder.track_config().timescale;
        // ddoc §6: the timeline follows capture stamps, not encoder cadence
        // claims. Each sample lasts until the next pts; the sealing
        // keyframe's pts closes the GOP exactly; only the final seal
        // (boundary = ∞) trusts the encoder's own duration. Finite GOPs are
        // quantized against that closing keyframe as one timeline so
        // multiple sub-tick intervals cannot accumulate past it.
        // Compute before taking pending state: a validation failure must not
        // silently discard video while leaving its audio behind.
        let durations = sealed_video_durations(&self.pending, boundary_pts_s, video_timescale)?;
        let sealed_video_bytes = self.pending_bytes;
        let packets = std::mem::take(&mut self.pending);
        self.pending_bytes = 0;
        let duration_s: f64 = durations.iter().sum();
        let mut data = Vec::with_capacity(sealed_video_bytes);
        let mut samples = Vec::with_capacity(packets.len());
        for (p, d) in packets.into_iter().zip(durations) {
            samples.push(SampleInfo {
                size: p.data.len() as u32,
                duration_s: d,
                is_sync: p.is_keyframe,
            });
            data.extend_from_slice(&p.data);
        }

        // Audio captured before the first video packet is engine-init
        // lead-in: drop it, or video plays early by that offset. Opus packets
        // are indivisible, so a packet straddling the origin is dropped too.
        let timeline_start = self.video_start_pts_s.unwrap_or(pts_start_s);
        drop_audio_before_timeline(&mut self.pending_audio, timeline_start);
        // Audio packets ending at or before the boundary belong to this GOP.
        let mut audio = Vec::with_capacity(self.pending_audio.len());
        for pending in &mut self.pending_audio {
            let split = pending
                .iter()
                .position(|p| p.pts_s + p.duration_s > boundary_pts_s + 1e-9)
                .unwrap_or(pending.len());
            let selected_bytes = pending[..split].iter().fold(0usize, |total, packet| {
                total.saturating_add(packet.data.len())
            });
            let mut track = TrackSamples {
                pts_start_s: None,
                data: Vec::with_capacity(selected_bytes),
                samples: Vec::with_capacity(split),
            };
            for p in pending.drain(..split) {
                track.pts_start_s.get_or_insert(p.pts_s);
                track.samples.push(SampleInfo {
                    size: p.data.len() as u32,
                    duration_s: p.duration_s,
                    is_sync: true, // every Opus packet is independently decodable
                });
                track.data.extend_from_slice(&p.data);
            }
            audio.push(track);
        }
        self.recount_pending_audio_bytes();
        self.pending_started_pts_s = self
            .pending_audio
            .iter()
            .flat_map(|track| track.iter())
            .map(|packet| packet.pts_s)
            .filter(|pts| pts.is_finite())
            .min_by(f64::total_cmp);

        let seg = Arc::new(Segment {
            starts_with_keyframe,
            pts_start_s,
            duration_s,
            data,
            samples,
            audio,
        });
        let queue_full_session = self
            .full_session
            .as_ref()
            .is_some_and(|sink| sink.send_error.is_none());
        self.ring.push(Arc::clone(&seg))?;
        if queue_full_session {
            self.queue_full_session_segment(seg);
        }
        Ok(())
    }

    fn push_encoded_packet(&mut self, pkt: EncodedPacket) -> Result<(), PipelineError> {
        if self.video_start_pts_s.is_none() {
            if !pkt.is_keyframe {
                self.note_pending_start(pkt.pts_s);
                self.pre_keyframe_bytes = self.pre_keyframe_bytes.saturating_add(pkt.data.len());
                return Ok(());
            }
            self.video_start_pts_s = Some(pkt.pts_s);
            self.pre_keyframe_bytes = 0;
            drop_audio_before_timeline(&mut self.pending_audio, pkt.pts_s);
            self.recount_pending_audio_bytes();
            self.pending_started_pts_s = Some(pkt.pts_s);
        }

        if pkt.is_keyframe && !self.pending.is_empty() {
            self.validate_pending_limits(pkt.pts_s)?;
            self.seal_pending(pkt.pts_s)?;
        }

        self.note_pending_start(pkt.pts_s);
        self.pending_bytes = self.pending_bytes.saturating_add(pkt.data.len());
        self.pending.push(pkt);
        Ok(())
    }

    fn poll_audio_until(&mut self, until_pts_s: f64) -> Result<(), PipelineError> {
        let mut added_bytes = 0usize;
        let mut first_pts_s: Option<f64> = None;
        for (source, pending) in self.audio_sources.iter_mut().zip(&mut self.pending_audio) {
            let packets = source.poll_packets(until_pts_s)?;
            added_bytes = packets.iter().fold(added_bytes, |total, packet| {
                total.saturating_add(packet.data.len())
            });
            for packet in &packets {
                if packet.pts_s.is_finite() {
                    first_pts_s =
                        Some(first_pts_s.map_or(packet.pts_s, |pts| pts.min(packet.pts_s)));
                }
            }
            pending.extend(packets);
        }
        self.pending_audio_bytes = self.pending_audio_bytes.saturating_add(added_bytes);
        if let Some(pts_s) = first_pts_s {
            self.note_pending_start(pts_s);
        }
        Ok(())
    }

    fn finish_audio_until(&mut self, until_pts_s: f64) -> Result<(), PipelineError> {
        let mut added_bytes = 0usize;
        let mut first_pts_s: Option<f64> = None;
        for (source, pending) in self.audio_sources.iter_mut().zip(&mut self.pending_audio) {
            let packets = source.finish_packets(until_pts_s)?;
            added_bytes = packets.iter().fold(added_bytes, |total, packet| {
                total.saturating_add(packet.data.len())
            });
            for packet in &packets {
                if packet.pts_s.is_finite() {
                    first_pts_s =
                        Some(first_pts_s.map_or(packet.pts_s, |pts| pts.min(packet.pts_s)));
                }
            }
            pending.extend(packets);
        }
        self.pending_audio_bytes = self.pending_audio_bytes.saturating_add(added_bytes);
        if let Some(pts_s) = first_pts_s {
            self.note_pending_start(pts_s);
        }
        Ok(())
    }

    fn note_pending_start(&mut self, pts_s: f64) {
        if !pts_s.is_finite() {
            return;
        }
        self.pending_started_pts_s = Some(
            self.pending_started_pts_s
                .map_or(pts_s, |current| current.min(pts_s)),
        );
    }

    fn recount_pending_audio_bytes(&mut self) {
        self.pending_audio_bytes = self
            .pending_audio
            .iter()
            .flat_map(|track| track.iter())
            .fold(0usize, |total, packet| {
                total.saturating_add(packet.data.len())
            });
    }

    fn validate_pending_limits(&self, now_pts_s: f64) -> Result<(), PipelineError> {
        let pending_payload_bytes = self.pending_payload_bytes();
        if pending_payload_bytes > self.pending_byte_budget {
            return Err(EncodeError::Backend(format!(
                "encoder did not produce a keyframe before pending video/audio GOP budget was exceeded ({pending_payload_bytes} > {} bytes)",
                self.pending_byte_budget
            ))
            .into());
        }
        if let Some(start_pts_s) = self.pending_started_pts_s {
            let duration_s = now_pts_s - start_pts_s;
            if duration_s.is_finite() && duration_s > MAX_PENDING_GOP_DURATION_S {
                return Err(EncodeError::Backend(format!(
                    "encoder did not produce a keyframe before pending GOP duration exceeded {:.1} seconds ({duration_s:.3} seconds)",
                    MAX_PENDING_GOP_DURATION_S
                ))
                .into());
            }
        }
        Ok(())
    }

    fn pending_payload_bytes(&self) -> usize {
        self.pre_keyframe_bytes
            .saturating_add(self.pending_bytes)
            .saturating_add(self.pending_audio_bytes)
    }

    fn queue_full_session_segment(&mut self, seg: Arc<Segment>) {
        let Some(sink) = &mut self.full_session else {
            return;
        };
        if sink.send_error.is_some() {
            return;
        }
        let reserved_bytes = seg.byte_len();
        if !try_reserve_queue_bytes(&sink.queued_bytes, reserved_bytes, sink.max_queue_bytes) {
            sink.send_error = Some(format!(
                "full session writer queue byte budget exceeded ({reserved_bytes} byte segment, {} of {} bytes already queued); full-session recording stopped",
                sink.queued_bytes.load(Ordering::Acquire),
                sink.max_queue_bytes
            ));
            return;
        }
        let start_s = seg.pts_start_s;
        let end_s = seg.pts_end_s();
        let video_cfg = sink
            .video_cfg
            .get_or_insert_with(|| self.encoder.track_config())
            .clone();
        let msg = FullSessionWriteMsg::Segment(FullSessionSegment {
            video_cfg,
            audio_cfgs: sink.audio_cfgs.clone(),
            segment: seg,
            reserved_bytes,
        });
        match sink.tx.try_send(msg) {
            Ok(()) => {
                sink.start_s.get_or_insert(start_s);
                sink.end_s = Some(end_s);
            }
            Err(TrySendError::Full(msg)) => {
                release_message_reservation(&sink.queued_bytes, &msg);
                sink.send_error = Some(
                    "full session writer queue reached its segment limit; full-session recording stopped"
                        .into(),
                );
            }
            Err(TrySendError::Disconnected(msg)) => {
                release_message_reservation(&sink.queued_bytes, &msg);
                sink.send_error = Some("full session writer stopped".into());
            }
        }
    }
}

fn sealed_video_durations(
    packets: &[EncodedPacket],
    boundary_pts_s: f64,
    timescale: u32,
) -> io::Result<Vec<f64>> {
    if timescale == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "video timescale must be nonzero",
        ));
    }
    let Some(first) = packets.first() else {
        return Ok(Vec::new());
    };
    if !boundary_pts_s.is_finite() {
        let minimum_duration_s = 1.0 / f64::from(timescale);
        return Ok((0..packets.len())
            .map(|index| {
                let next_pts_s = packets
                    .get(index + 1)
                    .map(|next| next.pts_s)
                    .unwrap_or(boundary_pts_s);
                if next_pts_s.is_finite() {
                    (next_pts_s - packets[index].pts_s).max(minimum_duration_s)
                } else {
                    packets[index].duration_s
                }
            })
            .collect());
    }

    let scale = f64::from(timescale);
    let total_ticks_f = (boundary_pts_s - first.pts_s) * scale;
    if !total_ticks_f.is_finite() || total_ticks_f > u64::MAX as f64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid sealed video GOP boundary",
        ));
    }
    let sample_count = u64::try_from(packets.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "video GOP sample count exceeds timeline capacity",
        )
    })?;
    // Encoded inter frames cannot be dropped safely: later frames may depend
    // on them. If pathological finite stamps provide fewer ticks than
    // samples, retain every packet and extend only enough to assign the
    // positive durations required by the MP4 writer.
    let total_ticks = (total_ticks_f.max(0.0).round() as u64).max(sample_count);

    let mut previous_end = 0_u64;
    let mut durations = Vec::with_capacity(packets.len());
    for (index, packet) in packets.iter().enumerate() {
        let next_pts_s = packets
            .get(index + 1)
            .map(|next| next.pts_s)
            .unwrap_or(boundary_pts_s);
        let interval_ticks = (next_pts_s - packet.pts_s) * scale;
        if !interval_ticks.is_finite() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid video sample interval",
            ));
        }

        let desired_end_f = (next_pts_s - first.pts_s) * scale;
        if !desired_end_f.is_finite() || desired_end_f > u64::MAX as f64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid video sample timestamp",
            ));
        }
        let desired_end = desired_end_f.max(0.0).round() as u64;
        let remaining = sample_count - index as u64 - 1;
        let earliest_end = previous_end + 1;
        let latest_end = total_ticks - remaining;
        let end = desired_end.clamp(earliest_end, latest_end);
        durations.push((end - previous_end) as f64 / scale);
        previous_end = end;
    }
    debug_assert_eq!(previous_end, total_ticks);
    Ok(durations)
}

fn drop_audio_before_timeline(pending_audio: &mut [Vec<AudioPacket>], timeline_start_s: f64) {
    for pending in pending_audio {
        pending.retain(|packet| packet.pts_s >= timeline_start_s - 1e-9);
    }
}

#[derive(Clone, Copy, Debug)]
struct SampleSelection {
    first_sample: usize,
    first_byte: usize,
    pts_start_s: Option<f64>,
}

fn select_audio_after_replay_origin(
    pts_start_s: Option<f64>,
    samples: &[SampleInfo],
    payload_len: usize,
    timeline_start_s: f64,
) -> io::Result<SampleSelection> {
    if samples.is_empty() {
        return Ok(SampleSelection {
            first_sample: 0,
            first_byte: 0,
            pts_start_s: None,
        });
    }
    let Some(mut sample_start_s) = pts_start_s else {
        return Ok(SampleSelection {
            first_sample: 0,
            first_byte: 0,
            pts_start_s: None,
        });
    };
    let mut first_sample = 0usize;
    let mut first_byte = 0usize;
    while sample_start_s < timeline_start_s - 1e-9 {
        let Some(sample) = samples.get(first_sample) else {
            break;
        };
        if !sample.duration_s.is_finite() || sample.duration_s < 0.0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid media sample duration",
            ));
        }
        first_byte = first_byte
            .checked_add(sample.size as usize)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "sample byte range overflow")
            })?;
        sample_start_s += sample.duration_s;
        if !sample_start_s.is_finite() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid media sample timestamp",
            ));
        }
        first_sample += 1;
    }
    if first_byte > payload_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "sample metadata exceeds declared track data",
        ));
    }
    Ok(SampleSelection {
        first_sample,
        first_byte,
        pts_start_s: (first_sample < samples.len()).then_some(sample_start_s),
    })
}

fn segment_audio_selections(
    segment: &Segment,
    replay_origin_s: Option<f64>,
) -> io::Result<Vec<SampleSelection>> {
    segment
        .audio
        .iter()
        .map(|track| {
            if let Some(origin_s) = replay_origin_s {
                select_audio_after_replay_origin(
                    track.pts_start_s,
                    &track.samples,
                    track.data.len(),
                    origin_s,
                )
            } else {
                Ok(SampleSelection {
                    first_sample: 0,
                    first_byte: 0,
                    pts_start_s: track.pts_start_s,
                })
            }
        })
        .collect()
}

fn pending_byte_budget(max_buffer_bytes: usize) -> usize {
    max_buffer_bytes.clamp(1, MAX_PENDING_GOP_BYTES)
}

fn spawn_full_session_writer(
    target: Box<dyn WriteSeek>,
    max_queue_segments: usize,
) -> io::Result<(
    SyncSender<FullSessionWriteMsg>,
    JoinHandle<()>,
    Arc<AtomicUsize>,
)> {
    let (tx, rx) = mpsc::sync_channel(max_queue_segments);
    let queued_bytes = Arc::new(AtomicUsize::new(0));
    let writer_queued_bytes = Arc::clone(&queued_bytes);
    let join = thread::Builder::new()
        .name("clipline-full-session-writer".into())
        .spawn(move || full_session_writer_loop(target, rx, writer_queued_bytes))?;
    Ok((tx, join, queued_bytes))
}

fn finish_full_session_writer(sink: FullSessionSink) -> io::Result<()> {
    let (reply_tx, reply_rx) = mpsc::channel();
    sink.tx
        .send(FullSessionWriteMsg::Finish(reply_tx))
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "full session writer stopped"))?;
    let result = reply_rx.recv().unwrap_or_else(|_| {
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "full session writer stopped before finalizing",
        ))
    });
    let join_result = sink.join.join();
    match (result, join_result) {
        (Err(e), _) => Err(e),
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(_)) => Err(io::Error::other("full session writer thread panicked")),
    }
}

fn full_session_writer_loop(
    target: Box<dyn WriteSeek>,
    rx: Receiver<FullSessionWriteMsg>,
    queued_bytes: Arc<AtomicUsize>,
) {
    let mut target = Some(target);
    let mut writer: Option<HybridMp4Writer<Box<dyn WriteSeek>>> = None;
    let mut timeline_origin_s = None;
    let mut first_error: Option<io::Error> = None;
    while let Ok(msg) = rx.recv() {
        match msg {
            FullSessionWriteMsg::Segment(segment) => {
                let reserved_bytes = segment.reserved_bytes;
                if first_error.is_none() {
                    if let Err(e) = write_full_session_segment(
                        &mut target,
                        &mut writer,
                        &mut timeline_origin_s,
                        segment.video_cfg,
                        segment.audio_cfgs,
                        segment.segment,
                    ) {
                        first_error = Some(e);
                        writer = None;
                    }
                }
                queued_bytes.fetch_sub(reserved_bytes, Ordering::AcqRel);
            }
            FullSessionWriteMsg::Finish(reply) => {
                let result = if let Some(e) = first_error.take() {
                    Err(e)
                } else if let Some(writer) = writer.take() {
                    writer.finalize().map(|_| ())
                } else {
                    Ok(())
                };
                let _ = reply.send(result);
                break;
            }
        }
    }
}

fn write_full_session_segment(
    target: &mut Option<Box<dyn WriteSeek>>,
    writer: &mut Option<HybridMp4Writer<Box<dyn WriteSeek>>>,
    timeline_origin_s: &mut Option<f64>,
    video_cfg: clipline_mp4::VideoTrackConfig,
    audio_cfgs: Vec<clipline_mp4::AudioTrackConfig>,
    seg: Arc<Segment>,
) -> io::Result<()> {
    let origin_s = *timeline_origin_s.get_or_insert(seg.pts_start_s);
    if writer.is_none() {
        let mut track_cfgs = vec![TrackConfig::Video(video_cfg.clone())];
        for cfg in &audio_cfgs {
            track_cfgs.push(TrackConfig::Audio(cfg.clone()));
        }
        let target = target.take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "full session writer target missing",
            )
        })?;
        *writer = Some(HybridMp4Writer::new_multi(target, track_cfgs)?);
    }
    let writer = writer.as_mut().expect("writer initialized");
    let audio_selections = segment_audio_selections(&seg, None)?;
    let timelines = set_segment_decode_times(
        writer,
        seg.pts_start_s,
        &audio_selections,
        &video_cfg,
        &audio_cfgs,
        origin_s,
    )?;
    let per_track =
        segment_fragment_refs(&seg, &audio_selections, &video_cfg, &audio_cfgs, &timelines)?;
    let slices: Vec<&[FragSampleRef<'_>]> = per_track.iter().map(|v| v.as_slice()).collect();
    writer.write_fragment_multi_borrowed(&slices)
}

fn write_memory_replay_segment<W: Write + Seek>(
    writer: &mut HybridMp4Writer<W>,
    segment: &Segment,
    video_cfg: &clipline_mp4::VideoTrackConfig,
    audio_cfgs: &[clipline_mp4::AudioTrackConfig],
    timeline_origin_s: f64,
) -> io::Result<()> {
    let audio_selections = segment_audio_selections(segment, Some(timeline_origin_s))?;
    let timelines = set_segment_decode_times(
        writer,
        segment.pts_start_s,
        &audio_selections,
        video_cfg,
        audio_cfgs,
        timeline_origin_s,
    )?;
    let per_track = segment_fragment_refs(
        segment,
        &audio_selections,
        video_cfg,
        audio_cfgs,
        &timelines,
    )?;
    let slices: Vec<&[FragSampleRef<'_>]> = per_track.iter().map(Vec::as_slice).collect();
    writer.write_fragment_multi_borrowed(&slices)
}

fn write_disk_replay_segment<W: Write + Seek>(
    writer: &mut HybridMp4Writer<W>,
    segment: &DiskSegment,
    video_cfg: &clipline_mp4::VideoTrackConfig,
    audio_cfgs: &[clipline_mp4::AudioTrackConfig],
    timeline_origin_s: f64,
) -> io::Result<()> {
    let audio_tracks: Vec<_> = segment.audio_tracks().collect();
    let audio_selections: Vec<_> = audio_tracks
        .iter()
        .map(|track| {
            select_audio_after_replay_origin(
                track.pts_start_s,
                track.samples,
                track.byte_len,
                timeline_origin_s,
            )
        })
        .collect::<io::Result<_>>()?;
    let timelines = set_segment_decode_times(
        writer,
        segment.pts_start_s,
        &audio_selections,
        video_cfg,
        audio_cfgs,
        timeline_origin_s,
    )?;
    let video = segment.video_track();
    let video_timeline = timelines.first().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "video fragment timeline is missing",
        )
    })?;
    let mut per_track = vec![quantized_source_samples(
        video,
        SampleSelection {
            first_sample: 0,
            first_byte: 0,
            pts_start_s: video.pts_start_s,
        },
        video_cfg.timescale,
        *video_timeline,
    )?];
    for (index, ((track, selection), cfg)) in audio_tracks
        .iter()
        .zip(&audio_selections)
        .zip(audio_cfgs)
        .enumerate()
    {
        let timeline = timelines.get(index + 1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "audio fragment timeline is missing",
            )
        })?;
        per_track.push(quantized_source_samples(
            *track,
            *selection,
            cfg.sample_rate,
            *timeline,
        )?);
    }
    per_track.resize_with(1 + audio_cfgs.len(), Vec::new);
    let slices: Vec<&[SourceSample]> = per_track.iter().map(Vec::as_slice).collect();
    let mut source = segment.open_payload()?;
    writer.write_fragment_multi_from_source(&mut source, &slices)
}

fn try_reserve_queue_bytes(queued: &AtomicUsize, bytes: usize, max_bytes: usize) -> bool {
    queued
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(bytes).filter(|next| *next <= max_bytes)
        })
        .is_ok()
}

fn release_message_reservation(queued: &AtomicUsize, msg: &FullSessionWriteMsg) {
    if let FullSessionWriteMsg::Segment(segment) = msg {
        queued.fetch_sub(segment.reserved_bytes, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy)]
struct FragmentTimeline {
    requested_start: u64,
    write_start: u64,
}

fn segment_fragment_refs<'a>(
    seg: &'a Segment,
    audio_selections: &[SampleSelection],
    video_cfg: &clipline_mp4::VideoTrackConfig,
    audio_cfgs: &[clipline_mp4::AudioTrackConfig],
    timelines: &[FragmentTimeline],
) -> io::Result<Vec<Vec<FragSampleRef<'a>>>> {
    let video_timeline = timelines.first().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "video fragment timeline is missing",
        )
    })?;
    let video = quantized_fragment_refs(
        seg.sample_slices(),
        &seg.samples,
        video_cfg.timescale,
        *video_timeline,
    )?;
    let mut per_track: Vec<Vec<FragSampleRef<'a>>> = vec![video];
    for (index, (track, cfg)) in seg.audio.iter().zip(audio_cfgs).enumerate() {
        let selection = audio_selections.get(index).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "audio sample selection is missing",
            )
        })?;
        let samples = track.samples.get(selection.first_sample..).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "audio sample selection exceeds track metadata",
            )
        })?;
        let timeline = timelines.get(index + 1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "audio fragment timeline is missing",
            )
        })?;
        per_track.push(quantized_fragment_refs(
            track.sample_slices().skip(selection.first_sample),
            samples,
            cfg.sample_rate,
            *timeline,
        )?);
    }
    // Segments recorded before an audio source was attached have fewer audio
    // tracks; pad with empty runs to keep alignment.
    per_track.resize_with(1 + audio_cfgs.len(), Vec::new);
    Ok(per_track)
}

fn quantized_fragment_refs<'a>(
    slices: impl Iterator<Item = io::Result<&'a [u8]>>,
    samples: &[SampleInfo],
    timescale: u32,
    timeline: FragmentTimeline,
) -> io::Result<Vec<FragSampleRef<'a>>> {
    let durations = quantized_sample_durations(samples, timescale, timeline)?;
    slices
        .zip(samples)
        .zip(durations)
        .map(|((slice, info), duration)| {
            Ok(FragSampleRef {
                data: slice?,
                duration,
                is_sync: info.is_sync,
            })
        })
        .collect()
}

fn quantized_source_samples(
    track: DiskTrackRef<'_>,
    selection: SampleSelection,
    timescale: u32,
    timeline: FragmentTimeline,
) -> io::Result<Vec<SourceSample>> {
    if selection.first_byte > track.byte_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "sample metadata exceeds declared track data",
        ));
    }
    let samples = track.samples.get(selection.first_sample..).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "sample selection exceeds track metadata",
        )
    })?;
    let durations = quantized_sample_durations(samples, timescale, timeline)?;
    let track_end = track
        .offset
        .checked_add(track.byte_len as u64)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "track byte range overflow"))?;
    let mut offset = track
        .offset
        .checked_add(selection.first_byte as u64)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "track byte range overflow"))?;
    samples
        .iter()
        .zip(durations)
        .map(|(info, duration)| {
            let sample_offset = offset;
            offset = offset.checked_add(u64::from(info.size)).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "sample byte range overflow")
            })?;
            if offset > track_end {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "sample metadata exceeds declared track data",
                ));
            }
            Ok(SourceSample {
                offset: sample_offset,
                size: info.size,
                duration,
                is_sync: info.is_sync,
            })
        })
        .collect()
}

fn quantized_sample_durations(
    samples: &[SampleInfo],
    timescale: u32,
    timeline: FragmentTimeline,
) -> io::Result<Vec<u32>> {
    let scale = f64::from(timescale);
    let total_s = samples.iter().try_fold(0.0_f64, |total, sample| {
        let next = total + sample.duration_s;
        if next.is_finite() && next >= 0.0 {
            Ok(next)
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid media sample duration",
            ))
        }
    })?;
    let relative_total_ticks = total_s * scale;
    if !relative_total_ticks.is_finite() || relative_total_ticks > u64::MAX as f64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "media duration overflow",
        ));
    }
    let requested_end = timeline
        .requested_start
        .checked_add(relative_total_ticks.round() as u64)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "media duration overflow"))?;
    let sample_count = u64::try_from(samples.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "media sample count overflow"))?;
    let minimum_end = timeline
        .write_start
        .checked_add(sample_count)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "media duration overflow"))?;
    let target_end = requested_end.max(minimum_end);

    let mut elapsed_s = 0.0_f64;
    let mut previous_end = timeline.write_start;
    samples
        .iter()
        .enumerate()
        .map(|(index, info)| {
            elapsed_s += info.duration_s;
            let relative_end = elapsed_s * scale;
            if !relative_end.is_finite() || relative_end < 0.0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid media sample duration",
                ));
            }
            // Quantize against the requested absolute timeline, but allocate
            // from the writer's actual frontier. A prior rounded overlap is
            // therefore absorbed by this run instead of becoming permanent
            // drift. Per-segment accumulation keeps the f64 error far below
            // half a timescale tick before this rounding step.
            let desired_end = timeline
                .requested_start
                .checked_add(relative_end.round() as u64)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "media duration overflow")
                })?;
            let remaining = sample_count - index as u64 - 1;
            let earliest_end = previous_end + 1;
            let latest_end = target_end - remaining;
            let end_ticks = desired_end.clamp(earliest_end, latest_end);
            let duration = end_ticks - previous_end;
            previous_end = end_ticks;
            u32::try_from(duration).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "media sample duration exceeds MP4 field",
                )
            })
        })
        .collect()
}

fn set_segment_decode_times<W: Write + Seek>(
    writer: &mut HybridMp4Writer<W>,
    segment_pts_start_s: f64,
    audio_selections: &[SampleSelection],
    video_cfg: &clipline_mp4::VideoTrackConfig,
    audio_cfgs: &[clipline_mp4::AudioTrackConfig],
    timeline_origin_s: f64,
) -> io::Result<Vec<FragmentTimeline>> {
    let mut timelines = vec![advance_track_decode_time(
        writer,
        0,
        relative_pts_ticks(segment_pts_start_s, timeline_origin_s, video_cfg.timescale)?,
    )?];
    for (index, cfg) in audio_cfgs.iter().enumerate() {
        let requested = audio_selections
            .get(index)
            .and_then(|selection| selection.pts_start_s)
            .map(|start_s| relative_pts_ticks(start_s, timeline_origin_s, cfg.sample_rate))
            .transpose()?;
        let timeline = if let Some(requested) = requested {
            advance_track_decode_time(writer, index + 1, requested)?
        } else {
            let current = writer.track_decode_time(index + 1)?;
            FragmentTimeline {
                requested_start: current,
                write_start: current,
            }
        };
        timelines.push(timeline);
    }
    Ok(timelines)
}

fn advance_track_decode_time<W: Write + Seek>(
    writer: &mut HybridMp4Writer<W>,
    track_index: usize,
    requested: u64,
) -> io::Result<FragmentTimeline> {
    let current = writer.track_decode_time(track_index)?;
    if requested > current {
        writer.set_track_decode_time(track_index, requested)?;
    }
    Ok(FragmentTimeline {
        requested_start: requested,
        write_start: current.max(requested),
    })
}

fn relative_pts_ticks(pts_s: f64, origin_s: f64, timescale: u32) -> io::Result<u64> {
    let relative = pts_s - origin_s;
    if !relative.is_finite() || relative < -1e-9 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "media sample timestamp precedes recording origin",
        ));
    }
    let ticks = relative.max(0.0) * f64::from(timescale);
    if ticks > u64::MAX as f64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "media sample timestamp exceeds MP4 timeline",
        ));
    }
    Ok(ticks.round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockCapture, MockEncoder};
    use crate::traits::{EncodedPacket, Frame};
    use clipline_mp4::{AudioTrackConfig, VideoTrackConfig};
    use clipline_test_utils::TestDir;

    struct NeverKeyframeEncoder {
        fps: u32,
    }

    struct PtsRemapEncoder {
        inner: MockEncoder,
        ticks: &'static [f64],
    }

    const CLOSELY_SPACED_TICKS: &[f64] = &[0.0, 900.0, 907.0, 2_700.0, 3_600.0];
    const REPEATED_SUB_TICK_TICKS: &[f64] = &[0.0, 900.0, 900.009, 900.018, 3_600.0];

    impl Encoder for PtsRemapEncoder {
        fn encode(&mut self, frame: &Frame) -> Result<Vec<EncodedPacket>, EncodeError> {
            let mut packets = self.inner.encode(frame)?;
            for packet in &mut packets {
                let index = (packet.pts_s * 30.0).round() as u64;
                let ticks = self.ticks.get(index as usize).copied().unwrap_or_else(|| {
                    let last_index = self.ticks.len() - 1;
                    self.ticks[last_index] + (index as usize - last_index) as f64 * 3_000.0
                });
                packet.pts_s = ticks / 90_000.0;
            }
            Ok(packets)
        }

        fn track_config(&self) -> VideoTrackConfig {
            self.inner.track_config()
        }
    }

    impl NeverKeyframeEncoder {
        fn new(fps: u32) -> Self {
            Self { fps }
        }
    }

    impl Encoder for NeverKeyframeEncoder {
        fn encode(&mut self, frame: &Frame) -> Result<Vec<EncodedPacket>, EncodeError> {
            Ok(vec![EncodedPacket {
                data: vec![0xEE; 128],
                pts_s: frame.pts_s,
                duration_s: 1.0 / self.fps as f64,
                is_keyframe: false,
            }])
        }

        fn track_config(&self) -> VideoTrackConfig {
            VideoTrackConfig::h264(
                128,
                128,
                90_000,
                vec![0x67, 0x64, 0x00, 0x0A, 0xAC],
                vec![0x68, 0xEE, 0x38, 0x80],
            )
        }
    }

    struct GappedAudioSource {
        packets: std::collections::VecDeque<AudioPacket>,
    }

    impl GappedAudioSource {
        fn new() -> Self {
            let mut packets = std::collections::VecDeque::new();
            for start in [1.2_f64, 3.2] {
                for index in 0..38 {
                    packets.push_back(AudioPacket {
                        data: vec![index as u8; 24],
                        pts_s: start + f64::from(index) * 0.02,
                        duration_s: 0.02,
                    });
                }
            }
            Self { packets }
        }
    }

    impl AudioSource for GappedAudioSource {
        fn poll_packets(&mut self, until_pts_s: f64) -> Result<Vec<AudioPacket>, CaptureError> {
            let mut output = Vec::new();
            while self
                .packets
                .front()
                .is_some_and(|packet| packet.pts_s + packet.duration_s <= until_pts_s + 1e-9)
            {
                output.push(self.packets.pop_front().expect("front packet exists"));
            }
            Ok(output)
        }

        fn track_config(&self) -> clipline_mp4::AudioTrackConfig {
            clipline_mp4::AudioTrackConfig {
                channels: 2,
                sample_rate: 48_000,
                pre_skip: 312,
            }
        }
    }

    fn edit_list_entries(bytes: &[u8]) -> Vec<(u32, i32)> {
        let fourcc = bytes
            .windows(4)
            .position(|window| window == b"elst")
            .expect("audio gap edit list");
        let payload = fourcc + 4;
        assert_eq!(bytes[payload], 0);
        let count = u32::from_be_bytes(bytes[payload + 4..payload + 8].try_into().unwrap());
        let mut entries = Vec::new();
        let mut pos = payload + 8;
        for _ in 0..count {
            entries.push((
                u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap()),
                i32::from_be_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()),
            ));
            pos += 12;
        }
        entries
    }

    #[test]
    fn groups_packets_into_gop_aligned_segments() {
        // 90 frames at 30 fps, GOP 30 → exactly 3 keyframe-led segments.
        let mut rec = Recorder::new(
            MockCapture::new(90, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        );
        rec.run_to_end().unwrap();
        let ring = rec.ring().unwrap();
        assert_eq!(ring.len(), 3);
        for seg in ring.segments() {
            assert!(seg.starts_with_keyframe);
            assert_eq!(seg.samples.len(), 30);
            assert!((seg.duration_s - 1.0).abs() < 1e-6);
        }
    }

    #[test]
    fn byte_budget_evicts_oldest_gop() {
        // Each MockEncoder sample is 64–70 bytes → a GOP of 30 ≈ ~2 KB.
        // Budget for ~2 GOPs: the first of three must be evicted.
        let mut rec = Recorder::new(MockCapture::new(90, 30), MockEncoder::new(30, 30), 4 * 1024);
        rec.run_to_end().unwrap();
        let ring = rec.ring().unwrap();
        assert_eq!(ring.len(), 2);
        let first = ring.segments().next().unwrap();
        assert!((first.pts_start_s - 1.0).abs() < 1e-6, "GOP at t=0 evicted");
    }

    #[test]
    fn pending_packets_are_byte_budgeted_when_keyframes_never_arrive() {
        let mut rec = Recorder::new(MockCapture::new(20, 30), NeverKeyframeEncoder::new(30), 512);

        let err = rec
            .run_to_end()
            .expect_err("unkeyframed stream should fail");

        assert!(
            err.to_string().contains("keyframe") && err.to_string().contains("budget"),
            "error should explain the keyframe/budget guard, got {err}"
        );
    }

    #[test]
    fn pending_gop_budget_counts_audio_payloads() {
        use crate::mock::MockAudioSource;

        let mut video_only =
            Recorder::new(MockCapture::new(10, 30), MockEncoder::new(30, 30), 1024);
        video_only
            .run_to_end()
            .expect("video payload alone fits the pending budget");

        let mut with_audio =
            Recorder::new(MockCapture::new(10, 30), MockEncoder::new(30, 30), 1024)
                .with_audio(Box::new(MockAudioSource::new(48_000, 20)));
        let error = with_audio
            .run_to_end()
            .expect_err("audio must consume the same pending GOP budget");

        assert!(
            error.to_string().contains("video/audio GOP budget"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn pending_gop_duration_is_bounded_when_keyframes_stop() {
        let mut recorder = Recorder::new(
            MockCapture::new(360, 30),
            MockEncoder::new(1000, 30),
            usize::MAX,
        );

        let error = recorder
            .run_to_end()
            .expect_err("an encoder must not retain an arbitrarily long GOP");

        assert!(
            error.to_string().contains("GOP duration") && error.to_string().contains("keyframe"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn short_stream_without_initial_keyframe_is_reported() {
        let mut rec = Recorder::new(
            MockCapture::new(1, 30),
            NeverKeyframeEncoder::new(30),
            usize::MAX,
        );

        let err = rec
            .run_to_end()
            .expect_err("short unkeyframed stream should fail");

        assert!(
            err.to_string().contains("keyframe") && err.to_string().contains("ended"),
            "error should explain that the stream ended before an initial keyframe, got {err}"
        );
        assert_eq!(rec.ring().unwrap().len(), 0);
    }

    #[test]
    fn audio_packets_land_in_their_gop_segments() {
        use crate::mock::MockAudioSource;
        let mut rec = Recorder::new(
            MockCapture::new(90, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        )
        .with_audio(Box::new(MockAudioSource::new(48_000, 20)));
        rec.run_to_end().unwrap();
        let ring = rec.ring().unwrap();
        assert_eq!(ring.len(), 3);
        for (i, seg) in ring.segments().enumerate() {
            assert_eq!(seg.audio.len(), 1, "one audio track");
            // 1 s GOP at 20 ms packets = 50 packets per segment.
            assert_eq!(seg.audio[0].samples.len(), 50, "segment {i}");
        }
        // First packet of the second segment starts at its GOP boundary.
        let seg2 = ring.segments().nth(1).unwrap();
        assert_eq!(&seg2.audio[0].data[..6], b"P00050");
        assert!((seg2.audio[0].pts_start_s.unwrap() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn delayed_and_gapped_audio_timing_survives_replay_and_full_session_muxing() {
        let dir = TestDir::new("clipline-pipeline", "gapped-audio-timeline");
        let full_path = dir.path().join("full.mp4");
        let mut recorder = Recorder::new(
            MockCapture::new(120, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        )
        .with_audio(Box::new(GappedAudioSource::new()));
        recorder
            .start_full_session(std::fs::File::create(&full_path).unwrap())
            .unwrap();

        recorder.run_to_end().unwrap();
        let segments: Vec<_> = recorder.ring().unwrap().segments().collect();
        assert!(segments[0].audio[0].samples.is_empty());
        assert!((segments[1].audio[0].pts_start_s.unwrap() - 1.2).abs() < 1e-9);
        assert!(segments[2].audio[0].samples.is_empty());
        assert!((segments[3].audio[0].pts_start_s.unwrap() - 3.2).abs() < 1e-9);

        let replay = recorder
            .save_replay(std::io::Cursor::new(Vec::new()), 10.0, None)
            .map(|(writer, _)| writer.into_inner())
            .unwrap();
        recorder.finish_full_session().unwrap().unwrap();
        let full = std::fs::read(full_path).unwrap();
        let expected = vec![
            (864_000, -1),
            (547_200, 0),
            (892_800, -1),
            (547_200, 36_480),
        ];
        assert_eq!(edit_list_entries(&replay), expected);
        assert_eq!(edit_list_entries(&full), expected);
    }

    #[test]
    fn pending_audio_reservation_is_released_when_each_gop_seals() {
        use crate::mock::MockAudioSource;

        let mut recorder =
            Recorder::new(MockCapture::new(90, 30), MockEncoder::new(30, 30), 8 * 1024)
                .with_audio(Box::new(MockAudioSource::new(48_000, 20)));

        recorder
            .run_to_end()
            .expect("each individual GOP fits even though all three do not");
        assert_eq!(
            recorder.ring().unwrap().len(),
            2,
            "ring still enforces its budget"
        );
    }

    #[test]
    fn save_replay_preserves_multiple_audio_tracks() {
        use crate::mock::MockAudioSource;
        use clipline_mp4::walker::{children, find, walk};

        let mut rec = Recorder::new(
            MockCapture::new(90, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        )
        .with_audio(Box::new(MockAudioSource::new(48_000, 20)))
        .with_audio(Box::new(MockAudioSource::new(48_000, 20)));

        rec.run_to_end().unwrap();
        for seg in rec.ring().unwrap().segments() {
            assert_eq!(seg.audio.len(), 2, "system plus microphone tracks");
        }

        let (buf, _) = rec
            .save_replay(std::io::Cursor::new(Vec::new()), 10.0, None)
            .map(|(w, e)| (w.into_inner(), e))
            .expect("multi-audio save");
        let boxes = walk(&buf);
        let moov = find(&boxes, b"moov").expect("moov");
        let kids = children(&buf, moov);
        let traks = kids.iter().filter(|b| &b.fourcc == b"trak").count();
        assert_eq!(traks, 3, "video plus two audio tracks");
    }

    fn first_opus_pre_skip(buf: &[u8]) -> u16 {
        let fourcc = buf
            .windows(4)
            .position(|window| window == b"dOps")
            .expect("dOps box");
        let p = fourcc + 4;
        u16::from_be_bytes(buf[p + 2..p + 4].try_into().expect("pre-skip bytes"))
    }

    #[test]
    fn save_replay_from_stream_start_keeps_opus_pre_skip() {
        use crate::mock::MockAudioSource;

        let mut rec = Recorder::new(
            MockCapture::new(90, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        )
        .with_audio(Box::new(MockAudioSource::new(48_000, 20)));

        rec.run_to_end().unwrap();
        let (buf, _) = rec
            .save_replay(std::io::Cursor::new(Vec::new()), 10.0, None)
            .map(|(w, end)| (w.into_inner(), end))
            .expect("replay from stream start");

        assert_eq!(first_opus_pre_skip(&buf), 312);
    }

    #[test]
    fn save_replay_from_middle_discards_opus_start_preroll() {
        use crate::mock::MockAudioSource;

        let mut rec = Recorder::new(
            MockCapture::new(90, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        )
        .with_audio(Box::new(MockAudioSource::new(48_000, 20)));

        rec.run_to_end().unwrap();
        let (buf, _) = rec
            .save_replay(std::io::Cursor::new(Vec::new()), 1.5, None)
            .map(|(w, end)| (w.into_inner(), end))
            .expect("replay from middle");

        assert_eq!(
            first_opus_pre_skip(&buf),
            960,
            "mid-stream replay clips discard only the first Opus frame to avoid cold decoder startup artifacts"
        );
    }

    #[test]
    fn disk_replay_storage_saves_same_bytes_as_memory_storage() {
        use crate::mock::MockAudioSource;

        let mut ram = Recorder::new(
            OffsetCapture {
                inner: MockCapture::new(90, 30),
                offset_s: 0.51,
            },
            MockEncoder::new(30, 30),
            usize::MAX,
        )
        .with_audio(Box::new(MockAudioSource::new(48_000, 20)))
        .with_audio(Box::new(MockAudioSource::new(48_000, 20)));
        ram.run_to_end().unwrap();
        let (ram_buf, ram_end) = ram
            .save_replay(std::io::Cursor::new(Vec::new()), 1.5, None)
            .map(|(w, end)| (w.into_inner(), end))
            .unwrap();

        let dir = TestDir::new("clipline-pipeline", "disk-equivalence");
        let mut disk = Recorder::new_with_replay_storage(
            OffsetCapture {
                inner: MockCapture::new(90, 30),
                offset_s: 0.51,
            },
            MockEncoder::new(30, 30),
            ReplayStorageConfig::Disk {
                max_bytes: usize::MAX,
                // Matches the byte-only `Recorder::new` ring above.
                retention_s: f64::INFINITY,
                dir: dir.path().to_path_buf(),
            },
        )
        .unwrap()
        .with_audio(Box::new(MockAudioSource::new(48_000, 20)))
        .with_audio(Box::new(MockAudioSource::new(48_000, 20)));
        disk.run_to_end().unwrap();
        let (disk_buf, disk_end) = disk
            .save_replay(std::io::Cursor::new(Vec::new()), 1.5, None)
            .map(|(w, end)| (w.into_inner(), end))
            .unwrap();

        assert_eq!(disk.ring_len(), ram.ring_len());
        assert_eq!(disk.ring_bytes(), ram.ring_bytes());
        assert_eq!(disk_end, ram_end);
        assert_eq!(disk_buf, ram_buf);
    }

    #[test]
    fn memory_replay_window_borrows_retained_segment_allocation() {
        let mut recorder = Recorder::new(
            MockCapture::new(60, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        );
        recorder.run_to_end().unwrap();
        let retained = recorder.ring().unwrap().segments().next().unwrap() as *const Segment;

        let ReplayWindow::Memory(selected) = recorder.ring.save_window(10.0, None) else {
            panic!("memory recorder must return a borrowed memory window");
        };

        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0] as *const Segment, retained);
    }

    #[test]
    fn sample_selection_drops_straddling_audio_without_mutating_payload() {
        let track = TrackSamples {
            pts_start_s: Some(1.50),
            data: b"AB".to_vec(),
            samples: vec![
                SampleInfo {
                    size: 1,
                    duration_s: 0.02,
                    is_sync: true,
                },
                SampleInfo {
                    size: 1,
                    duration_s: 0.02,
                    is_sync: true,
                },
            ],
        };
        let original_ptr = track.data.as_ptr();
        let original_data = track.data.clone();
        let original_sample_count = track.samples.len();

        let selection = select_audio_after_replay_origin(
            track.pts_start_s,
            &track.samples,
            track.data.len(),
            1.51,
        )
        .unwrap();

        assert_eq!(selection.first_sample, 1);
        assert_eq!(selection.first_byte, 1);
        assert!((selection.pts_start_s.unwrap() - 1.52).abs() < 1e-9);
        assert_eq!(track.data, original_data);
        assert_eq!(track.samples.len(), original_sample_count);
        assert_eq!(
            track.data[selection.first_byte..].as_ptr(),
            original_ptr.wrapping_add(1)
        );
    }

    #[test]
    fn disk_replay_rejects_video_samples_crossing_into_audio_region() {
        let dir = TestDir::new("clipline-pipeline", "disk-track-boundary");
        let mut recorder = Recorder::new_with_replay_storage(
            MockCapture::new(0, 30),
            MockEncoder::new(30, 30),
            ReplayStorageConfig::Disk {
                max_bytes: usize::MAX,
                retention_s: f64::INFINITY,
                dir: dir.path().to_path_buf(),
            },
        )
        .unwrap();
        let malformed = Segment {
            starts_with_keyframe: true,
            pts_start_s: 0.0,
            duration_s: 1.0,
            data: vec![1, 2, 3, 4],
            samples: vec![SampleInfo {
                size: 5,
                duration_s: 1.0,
                is_sync: true,
            }],
            audio: vec![TrackSamples {
                pts_start_s: Some(0.0),
                data: vec![5, 6, 7, 8],
                samples: vec![SampleInfo {
                    size: 4,
                    duration_s: 1.0,
                    is_sync: true,
                }],
            }],
        };
        let ReplayStorage::Disk(ring) = &mut recorder.ring else {
            panic!("fixture must use disk replay storage");
        };
        ring.push(malformed).unwrap();
        recorder.video_start_pts_s = Some(0.0);

        let error = recorder
            .save_replay(std::io::Cursor::new(Vec::new()), 10.0, None)
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            error.to_string().contains("track data"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn memory_retention_bounds_the_ring_by_duration() {
        // 3s of footage in 1s GOPs. A 1.5s retention window keeps the two
        // newest GOPs, where a byte-only ring keeps all three.
        let mut bounded = Recorder::new_with_replay_storage(
            MockCapture::new(90, 30),
            MockEncoder::new(30, 30),
            ReplayStorageConfig::Memory {
                max_bytes: usize::MAX,
                retention_s: 1.5,
            },
        )
        .unwrap();
        bounded.run_to_end().unwrap();

        let mut unbounded = Recorder::new(
            MockCapture::new(90, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        );
        unbounded.run_to_end().unwrap();

        assert_eq!(
            unbounded.ring_len(),
            3,
            "`new` must stay byte-only so existing callers are unaffected"
        );
        assert_eq!(bounded.ring_len(), 2, "retention drops the oldest GOP");
        assert!(bounded.ring_bytes() < unbounded.ring_bytes());
    }

    #[test]
    fn disk_retention_matches_memory_retention() {
        let dir = TestDir::new("clipline-pipeline", "disk-retention");
        let mut disk = Recorder::new_with_replay_storage(
            MockCapture::new(90, 30),
            MockEncoder::new(30, 30),
            ReplayStorageConfig::Disk {
                max_bytes: usize::MAX,
                retention_s: 1.5,
                dir: dir.path().to_path_buf(),
            },
        )
        .unwrap();
        disk.run_to_end().unwrap();

        let mut ram = Recorder::with_retention(
            MockCapture::new(90, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
            1.5,
        );
        ram.run_to_end().unwrap();

        assert_eq!(disk.ring_len(), ram.ring_len());
        assert_eq!(disk.ring_bytes(), ram.ring_bytes());
    }

    #[test]
    fn full_session_sink_keeps_segments_evicted_from_replay_ring() {
        let dir = TestDir::new("clipline-pipeline", "full-session");
        let path = dir.path().join("session.mp4");
        let file = std::fs::File::create(&path).unwrap();
        let mut rec = Recorder::new(MockCapture::new(90, 30), MockEncoder::new(30, 30), 4 * 1024);

        rec.start_full_session(file).unwrap();
        rec.run_to_end().unwrap();
        let summary = rec.finish_full_session().unwrap().expect("session summary");

        assert_eq!(
            rec.ring().unwrap().len(),
            2,
            "oldest GOP evicted from replay ring"
        );
        assert!((summary.start_s - 0.0).abs() < 1e-6);
        assert!((summary.duration_s - 3.0).abs() < 1e-6);
        let data = std::fs::read(&path).unwrap();
        let duration = clipline_mp4::walker::movie_duration_s(&data).unwrap();
        assert!(
            (duration - 3.0).abs() < 1e-3,
            "full-session file keeps all GOPs, got {duration}"
        );
    }

    #[test]
    fn one_tick_segment_boundary_overlap_does_not_break_full_session_muxing() {
        let video_cfg = MockEncoder::new(30, 30).track_config();
        let timescale = f64::from(video_cfg.timescale);
        let segment = |start_ticks: f64, duration_ticks: f64| {
            Arc::new(Segment {
                starts_with_keyframe: true,
                pts_start_s: start_ticks / timescale,
                duration_s: duration_ticks / timescale,
                data: vec![0, 0, 0, 1],
                samples: vec![SampleInfo {
                    size: 4,
                    duration_s: duration_ticks / timescale,
                    is_sync: true,
                }],
                audio: Vec::new(),
            })
        };
        let mut target: Option<Box<dyn WriteSeek>> =
            Some(Box::new(std::io::Cursor::new(Vec::new())));
        let mut writer = None;
        let mut origin = None;

        write_full_session_segment(
            &mut target,
            &mut writer,
            &mut origin,
            video_cfg.clone(),
            Vec::new(),
            segment(0.0, 101.0),
        )
        .unwrap();
        write_full_session_segment(
            &mut target,
            &mut writer,
            &mut origin,
            video_cfg,
            Vec::new(),
            // Independent absolute rounding selects tick 100 even though the
            // preceding segment's locally quantized duration ended at 101.
            segment(100.0, 100.0),
        )
        .expect("a one-tick quantization overlap must clamp to the written frontier");
        writer.unwrap().finalize().unwrap();
    }

    #[test]
    fn repeated_segment_rounding_ties_do_not_accumulate_writer_drift() {
        let video_cfg = MockEncoder::new(30, 30).track_config();
        let timescale = f64::from(video_cfg.timescale);
        let segment = |index: u64| {
            let duration_ticks = if index == 4 { 100.0 } else { 1_000.6 };
            Arc::new(Segment {
                starts_with_keyframe: true,
                pts_start_s: index as f64 * 1_000.6 / timescale,
                duration_s: duration_ticks / timescale,
                data: vec![0, 0, 0, index as u8],
                samples: vec![SampleInfo {
                    size: 4,
                    duration_s: duration_ticks / timescale,
                    is_sync: true,
                }],
                audio: Vec::new(),
            })
        };
        let mut target: Option<Box<dyn WriteSeek>> =
            Some(Box::new(std::io::Cursor::new(Vec::new())));
        let mut writer = None;
        let mut origin = None;

        let expected_frontiers = [1_001, 2_002, 3_002, 4_003, 4_102];
        for (index, expected_frontier) in expected_frontiers.into_iter().enumerate() {
            write_full_session_segment(
                &mut target,
                &mut writer,
                &mut origin,
                video_cfg.clone(),
                Vec::new(),
                segment(index as u64),
            )
            .unwrap_or_else(|error| {
                panic!("segment {index} must absorb prior rounding drift: {error}")
            });
            assert_eq!(
                writer.as_ref().unwrap().track_decode_time(0).unwrap(),
                expected_frontier,
                "segment {index} must land on its global endpoint"
            );
        }
        writer.unwrap().finalize().unwrap();
    }

    #[test]
    fn sub_hundred_microsecond_frame_gap_does_not_break_full_session_finalization() {
        let dir = TestDir::new("clipline-pipeline", "sub-millisecond-gop-boundary");
        let path = dir.path().join("session.mp4");
        let mut recorder = Recorder::new(
            MockCapture::new(8, 30),
            PtsRemapEncoder {
                inner: MockEncoder::new(4, 30),
                // Seven ticks after the preceding frame: a valid positive
                // interval that the old 100 us floor inflated to 9 ticks.
                ticks: CLOSELY_SPACED_TICKS,
            },
            usize::MAX,
        );
        recorder
            .start_full_session(std::fs::File::create(&path).unwrap())
            .unwrap();

        recorder.run_to_end().unwrap();
        recorder
            .finish_full_session()
            .expect("a valid seven-tick frame interval must not inflate past the next GOP start")
            .expect("session summary");

        let first = recorder.ring().unwrap().segments().next().unwrap();
        assert_eq!((first.samples[1].duration_s * 90_000.0).round(), 7.0);
        assert!(clipline_mp4::walker::movie_duration_s(&std::fs::read(path).unwrap()).is_some());
    }

    #[test]
    fn repeated_sub_tick_gaps_do_not_accumulate_past_the_next_gop() {
        let dir = TestDir::new("clipline-pipeline", "repeated-sub-tick-gaps");
        let path = dir.path().join("session.mp4");
        let mut recorder = Recorder::new(
            MockCapture::new(8, 30),
            PtsRemapEncoder {
                inner: MockEncoder::new(4, 30),
                // WGC and MFT carry 100 ns timestamps. At a 90 kHz MP4
                // timescale, one 100 ns step is 0.009 tick, so two adjacent
                // steps must not become two permanent ticks of inflation.
                ticks: REPEATED_SUB_TICK_TICKS,
            },
            usize::MAX,
        );
        recorder
            .start_full_session(std::fs::File::create(&path).unwrap())
            .unwrap();

        recorder.run_to_end().unwrap();
        recorder
            .finish_full_session()
            .expect("two sub-tick gaps must not move the next GOP backward")
            .expect("session summary");

        let segments: Vec<_> = recorder.ring().unwrap().segments().collect();
        assert!((segments[0].pts_end_s() - segments[1].pts_start_s).abs() < 1e-12);
        assert!(segments[0]
            .samples
            .iter()
            .all(|sample| sample.duration_s * 90_000.0 >= 1.0));
        assert!(clipline_mp4::walker::movie_duration_s(&std::fs::read(path).unwrap()).is_some());
    }

    #[test]
    fn bounded_gop_absorbs_independent_sub_tick_timestamp_jitter() {
        let packets: Vec<_> = [0.0, 100.0, 99.4, 200.0, 199.4]
            .into_iter()
            .enumerate()
            .map(|(index, ticks)| EncodedPacket {
                data: vec![index as u8],
                pts_s: ticks / 90_000.0,
                duration_s: 1.0 / 90_000.0,
                is_keyframe: index == 0,
            })
            .collect();

        let durations = sealed_video_durations(&packets, 300.0 / 90_000.0, 90_000)
            .expect("local timestamp jitter must not terminate capture");
        assert_eq!(durations.len(), packets.len());
        assert!(durations.iter().all(|duration| duration * 90_000.0 >= 1.0));
        assert_eq!((durations.iter().sum::<f64>() * 90_000.0).round(), 300.0);
    }

    #[test]
    fn crowded_bounded_gop_extends_only_enough_for_positive_durations() {
        let packets: Vec<_> = [0.0, 0.2, 0.4]
            .into_iter()
            .enumerate()
            .map(|(index, ticks)| EncodedPacket {
                data: vec![index as u8],
                pts_s: ticks / 90_000.0,
                duration_s: 1.0 / 90_000.0,
                is_keyframe: index == 0,
            })
            .collect();

        let durations = sealed_video_durations(&packets, 2.0 / 90_000.0, 90_000)
            .expect("crowded finite timestamps must degrade without ending the session");
        assert_eq!(durations.len(), 3);
        assert!(durations
            .iter()
            .all(|duration| (duration * 90_000.0 - 1.0).abs() < 1e-9));
    }

    #[test]
    fn slightly_backward_single_packet_boundary_gets_one_tick() {
        let packets = vec![EncodedPacket {
            data: vec![0],
            pts_s: 0.0,
            duration_s: 1.0 / 90_000.0,
            is_keyframe: true,
        }];

        let durations = sealed_video_durations(&packets, -0.4 / 90_000.0, 90_000)
            .expect("a sub-tick boundary regression must not terminate capture");
        assert_eq!(durations, vec![1.0 / 90_000.0]);
    }

    #[test]
    fn failed_seal_preserves_pending_video_and_audio() {
        let mut recorder = Recorder::new(
            MockCapture::new(1, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        );
        recorder.pending = vec![EncodedPacket {
            data: vec![1, 2, 3],
            pts_s: f64::NAN,
            duration_s: 1.0 / 30.0,
            is_keyframe: true,
        }];
        recorder.pending_bytes = 3;
        recorder.pending_audio = vec![vec![AudioPacket {
            data: vec![4, 5],
            pts_s: 0.0,
            duration_s: 0.02,
        }]];
        recorder.pending_audio_bytes = 2;

        recorder
            .seal_pending(1.0)
            .expect_err("non-finite pending timestamps must still be rejected");

        assert_eq!(recorder.pending.len(), 1);
        assert_eq!(recorder.pending[0].data, vec![1, 2, 3]);
        assert_eq!(recorder.pending_bytes, 3);
        assert_eq!(recorder.pending_audio.len(), 1);
        assert_eq!(recorder.pending_audio[0][0].data, vec![4, 5]);
        assert_eq!(recorder.pending_audio_bytes, 2);
    }

    #[test]
    fn unbounded_gop_keeps_encoder_duration_for_non_finite_next_timestamp() {
        let packets = vec![
            EncodedPacket {
                data: vec![0],
                pts_s: 0.0,
                duration_s: 0.25,
                is_keyframe: true,
            },
            EncodedPacket {
                data: vec![1],
                pts_s: f64::INFINITY,
                duration_s: 0.5,
                is_keyframe: false,
            },
        ];

        assert_eq!(
            sealed_video_durations(&packets, f64::INFINITY, 90_000).unwrap(),
            vec![0.25, 0.5]
        );
    }

    #[test]
    fn zero_video_timescale_is_rejected_for_unbounded_gop() {
        let packets = vec![EncodedPacket {
            data: vec![0],
            pts_s: 0.0,
            duration_s: 0.25,
            is_keyframe: true,
        }];

        let error = sealed_video_durations(&packets, f64::INFINITY, 0)
            .expect_err("all seals require a valid MP4 video timescale");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn capture_timeline_never_moves_strict_writer_backward() {
        let cfg = MockEncoder::new(30, 30).track_config();
        let mut writer = HybridMp4Writer::new(std::io::Cursor::new(Vec::new()), cfg).unwrap();
        writer.set_track_decode_time(0, 100).unwrap();

        let one_tick = advance_track_decode_time(&mut writer, 0, 99).unwrap();
        assert_eq!(writer.track_decode_time(0).unwrap(), 100);
        assert_eq!(one_tick.requested_start, 99);
        assert_eq!(one_tick.write_start, 100);

        let larger_regression = advance_track_decode_time(&mut writer, 0, 98).unwrap();
        assert_eq!(writer.track_decode_time(0).unwrap(), 100);
        assert_eq!(larger_regression.requested_start, 98);
        assert_eq!(larger_regression.write_start, 100);
    }

    #[test]
    fn full_session_initializes_muxer_after_encoder_config_is_ready() {
        let dir = TestDir::new("clipline-pipeline", "full-session-lazy-config");
        let path = dir.path().join("session.mp4");
        let file = std::fs::File::create(&path).unwrap();
        let mut rec = Recorder::new(
            MockCapture::new(60, 30),
            DelayedTrackConfig {
                inner: MockEncoder::new(30, 30),
                encoded_any: false,
            },
            usize::MAX,
        );

        rec.start_full_session(file).unwrap();
        rec.run_to_end().unwrap();
        rec.finish_full_session().unwrap().expect("session summary");

        let data = std::fs::read(&path).unwrap();
        assert!(
            data.windows(DELAYED_SPS.len()).any(|w| w == DELAYED_SPS),
            "full-session moov must use the encoder config populated by the first packets"
        );
    }

    #[test]
    fn full_session_write_failure_does_not_abort_replay_capture() {
        let mut rec = Recorder::new(
            MockCapture::new(90, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        );

        rec.start_full_session(FailingWriter).unwrap();
        rec.run_to_end()
            .expect("secondary session sink must not stop capture");

        assert_eq!(rec.ring().unwrap().len(), 3);
        let err = rec.finish_full_session().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);
    }

    #[test]
    fn full_session_queue_budget_failure_does_not_abort_replay_capture() {
        let mut rec = Recorder::new(
            MockCapture::new(90, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        );

        rec.start_full_session_with_limits(std::io::Cursor::new(Vec::new()), 1, 1)
            .unwrap();
        rec.run_to_end()
            .expect("full-session backpressure must not stop replay capture");

        assert_eq!(rec.ring().unwrap().len(), 3);
        let err = rec.finish_full_session().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
        assert!(
            err.to_string().contains("queue byte budget"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn full_session_queue_byte_reservations_never_exceed_the_limit() {
        let queued = AtomicUsize::new(0);

        assert!(try_reserve_queue_bytes(&queued, 6, 10));
        assert_eq!(queued.load(Ordering::Acquire), 6);
        assert!(!try_reserve_queue_bytes(&queued, 5, 10));
        assert_eq!(queued.load(Ordering::Acquire), 6);

        queued.fetch_sub(6, Ordering::AcqRel);
        assert!(try_reserve_queue_bytes(&queued, 10, 10));
        assert_eq!(queued.load(Ordering::Acquire), 10);
    }

    #[test]
    fn stalled_full_session_writer_hits_segment_limit_without_blocking_capture() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let writer = GatedWriter {
            inner: std::io::Cursor::new(Vec::new()),
            entered: Some(entered_tx),
            release: release_rx,
        };
        let mut rec = Recorder::new(
            MockCapture::new(90, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        );
        rec.start_full_session_with_limits(writer, usize::MAX, 1)
            .unwrap();

        for _ in 0..31 {
            assert!(rec.step().unwrap());
        }
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("writer must begin the first segment");
        rec.run_to_end()
            .expect("stalled full-session output must not block capture");
        assert_eq!(rec.ring().unwrap().len(), 3);

        release_tx.send(()).unwrap();
        let err = rec.finish_full_session().unwrap_err();
        assert!(
            err.to_string().contains("segment limit"),
            "unexpected error: {err}"
        );
    }

    struct GatedWriter {
        inner: std::io::Cursor<Vec<u8>>,
        entered: Option<Sender<()>>,
        release: Receiver<()>,
    }

    impl Write for GatedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if let Some(entered) = self.entered.take() {
                entered
                    .send(())
                    .map_err(|_| io::Error::other("gate observer stopped"))?;
                self.release
                    .recv()
                    .map_err(|_| io::Error::other("gate released by disconnect"))?;
            }
            self.inner.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.inner.flush()
        }
    }

    impl Seek for GatedWriter {
        fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("disk full"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Seek for FailingWriter {
        fn seek(&mut self, _pos: io::SeekFrom) -> io::Result<u64> {
            Ok(0)
        }
    }

    const DELAYED_SPS: &[u8] = &[0x67, 0x64, 0x00, 0x0A, 0xAC];

    struct DelayedTrackConfig {
        inner: MockEncoder,
        encoded_any: bool,
    }

    impl Encoder for DelayedTrackConfig {
        fn encode(
            &mut self,
            frame: &crate::traits::Frame,
        ) -> Result<Vec<crate::traits::EncodedPacket>, crate::traits::EncodeError> {
            let packets = self.inner.encode(frame)?;
            self.encoded_any = true;
            Ok(packets)
        }

        fn track_config(&self) -> clipline_mp4::VideoTrackConfig {
            if self.encoded_any {
                return self.inner.track_config();
            }
            clipline_mp4::VideoTrackConfig::h264(128, 128, 90_000, Vec::new(), Vec::new())
        }
    }

    /// Wraps MockEncoder but holds back the latest packet until finish() —
    /// models real encoders' internal buffering.
    struct OneFrameLatency {
        inner: MockEncoder,
        held: Option<crate::traits::EncodedPacket>,
    }

    impl Encoder for OneFrameLatency {
        fn encode(
            &mut self,
            frame: &crate::traits::Frame,
        ) -> Result<Vec<crate::traits::EncodedPacket>, crate::traits::EncodeError> {
            let mut out = self.inner.encode(frame)?;
            let newly = out.pop();
            let released = self.held.take();
            self.held = newly;
            Ok(released.into_iter().collect())
        }

        fn track_config(&self) -> clipline_mp4::VideoTrackConfig {
            self.inner.track_config()
        }

        fn finish(
            &mut self,
        ) -> Result<Vec<crate::traits::EncodedPacket>, crate::traits::EncodeError> {
            Ok(self.held.take().into_iter().collect())
        }
    }

    /// Encoder echoing nominal durations while pts jitters (VRR-style):
    /// the sealed timeline must follow the STAMPS (ddoc §6), not the echo.
    struct JitteryEncoder {
        inner: MockEncoder,
    }

    impl Encoder for JitteryEncoder {
        fn encode(
            &mut self,
            frame: &crate::traits::Frame,
        ) -> Result<Vec<crate::traits::EncodedPacket>, crate::traits::EncodeError> {
            let mut pkts = self.inner.encode(frame)?;
            for p in &mut pkts {
                // Stamps: frames alternate 10 ms / 30 ms apart, while the
                // encoder still claims a flat 1/30 s duration.
                let idx = (p.pts_s * 30.0).round();
                p.pts_s = (idx / 2.0).floor() * 0.04 + if idx % 2.0 == 1.0 { 0.01 } else { 0.0 };
            }
            Ok(pkts)
        }
        fn track_config(&self) -> clipline_mp4::VideoTrackConfig {
            self.inner.track_config()
        }
    }

    #[test]
    fn sealed_durations_come_from_pts_deltas_not_encoder_claims() {
        // GOP of 4 over 8 frames → two segments, boundary at frame 4.
        let enc = JitteryEncoder {
            inner: MockEncoder::new(4, 30),
        };
        let mut rec = Recorder::new(MockCapture::new(8, 30), enc, usize::MAX);
        rec.run_to_end().unwrap();
        let segs: Vec<_> = rec.ring().unwrap().segments().collect();
        assert_eq!(segs.len(), 2);
        // Within a GOP: 10/30/10 ms gaps, NOT the encoder's flat 33.3 ms.
        let d: Vec<f64> = segs[0].samples.iter().map(|s| s.duration_s).collect();
        assert!((d[0] - 0.01).abs() < 1e-9, "got {d:?}");
        assert!((d[1] - 0.03).abs() < 1e-9, "got {d:?}");
        assert!((d[2] - 0.01).abs() < 1e-9, "got {d:?}");
        // Boundary: last sample of GOP 1 closes exactly at GOP 2's keyframe.
        let gop2_start = segs[1].pts_start_s;
        assert!(
            (segs[0].pts_end_s() - gop2_start).abs() < 1e-9,
            "no gap, no overlap"
        );
        // Final seal falls back to the encoder duration for the last sample.
        let last = segs[1].samples.last().unwrap();
        assert!((last.duration_s - 1.0 / 30.0).abs() < 1e-9);
    }

    #[test]
    fn sealed_gop_vectors_have_no_geometric_growth_slack() {
        let mut recorder = Recorder::new(
            MockCapture::new(0, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        );
        recorder.pending = vec![
            EncodedPacket {
                data: vec![1; 3],
                pts_s: 0.0,
                duration_s: 0.1,
                is_keyframe: true,
            },
            EncodedPacket {
                data: vec![2; 5],
                pts_s: 0.1,
                duration_s: 0.1,
                is_keyframe: false,
            },
            EncodedPacket {
                data: vec![3; 7],
                pts_s: 0.2,
                duration_s: 0.1,
                is_keyframe: false,
            },
        ];
        recorder.pending_bytes = 15;
        recorder.video_start_pts_s = Some(0.0);
        recorder.pending_audio = vec![vec![
            AudioPacket {
                data: vec![4; 3],
                pts_s: 0.0,
                duration_s: 0.1,
            },
            AudioPacket {
                data: vec![5; 5],
                pts_s: 0.1,
                duration_s: 0.1,
            },
            AudioPacket {
                data: vec![6; 7],
                pts_s: 0.2,
                duration_s: 0.1,
            },
        ]];
        recorder.pending_audio_bytes = 15;

        recorder.seal_pending(0.3).unwrap();

        let segment = recorder
            .ring()
            .unwrap()
            .segments()
            .next()
            .expect("sealed segment");
        assert_eq!(segment.data.capacity(), segment.data.len());
        assert_eq!(segment.samples.capacity(), segment.samples.len());
        assert_eq!(
            segment.audio[0].data.capacity(),
            segment.audio[0].data.len()
        );
        assert_eq!(
            segment.audio[0].samples.capacity(),
            segment.audio[0].samples.len()
        );
    }

    #[test]
    fn run_to_end_drains_encoder_via_finish() {
        let enc = OneFrameLatency {
            inner: MockEncoder::new(30, 30),
            held: None,
        };
        let mut rec = Recorder::new(MockCapture::new(30, 30), enc, usize::MAX);
        rec.run_to_end().unwrap();
        // All 30 frames present despite the encoder's one-frame latency.
        let total: usize = rec
            .ring()
            .unwrap()
            .segments()
            .map(|s| s.samples.len())
            .sum();
        assert_eq!(total, 30);
    }

    struct FinishOnlyAudioSource {
        finished: bool,
    }

    impl AudioSource for FinishOnlyAudioSource {
        fn poll_packets(&mut self, _until_pts_s: f64) -> Result<Vec<AudioPacket>, CaptureError> {
            Ok(Vec::new())
        }

        fn finish_packets(&mut self, until_pts_s: f64) -> Result<Vec<AudioPacket>, CaptureError> {
            if self.finished || until_pts_s + 1e-9 < 0.98 {
                return Ok(Vec::new());
            }
            self.finished = true;
            Ok(vec![AudioPacket {
                data: vec![0xAB; 24],
                pts_s: 0.96,
                duration_s: 0.02,
            }])
        }

        fn track_config(&self) -> AudioTrackConfig {
            AudioTrackConfig {
                channels: 2,
                sample_rate: 48_000,
                pre_skip: 312,
            }
        }
    }

    #[test]
    fn finish_stream_retains_audio_available_only_during_terminal_drain() {
        let mut recorder = Recorder::new(
            MockCapture::new(30, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        )
        .with_audio(Box::new(FinishOnlyAudioSource { finished: false }));

        recorder.run_to_end().unwrap();

        let segment = recorder.ring().unwrap().segments().next().unwrap();
        assert_eq!(segment.audio[0].samples.len(), 1);
        assert!((segment.audio[0].pts_start_s.unwrap() - 0.96).abs() < 1e-9);
    }

    #[test]
    fn save_replay_works_between_steps_while_recording() {
        let mut rec = Recorder::new(
            MockCapture::new(90, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        );
        // Two GOPs in: a save must succeed without ending the recording.
        for _ in 0..60 {
            assert!(rec.step().unwrap());
        }
        let (buf, end) = rec
            .save_replay(std::io::Cursor::new(Vec::new()), 10.0, None)
            .map(|(w, e)| (w.into_inner(), e))
            .expect("mid-recording save");
        assert!(!buf.is_empty());
        assert!(
            (end - 1.0).abs() < 1e-6,
            "one sealed GOP at save time (second pending)"
        );
        // Recording continues; smart mode skips the already-saved second.
        for _ in 0..30 {
            assert!(rec.step().unwrap());
        }
        assert!(!rec.step().unwrap(), "source exhausted");
        rec.finish_stream().unwrap();
        let (_, end2) = rec
            .save_replay(std::io::Cursor::new(Vec::new()), 10.0, Some(end))
            .expect("post-finish save");
        assert!((end2 - 3.0).abs() < 1e-6, "everything sealed after finish");
        // run_to_end equivalence: same segment layout as the stepped path.
        let mut whole = Recorder::new(
            MockCapture::new(90, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        );
        whole.run_to_end().unwrap();
        assert_eq!(whole.ring().unwrap().len(), rec.ring().unwrap().len());
    }

    /// Shifts a capture source's pts later — models the real lead-in
    /// between clock creation and the first WGC frame.
    struct OffsetCapture {
        inner: MockCapture,
        offset_s: f64,
    }

    impl crate::traits::CaptureEngine for OffsetCapture {
        fn next_frame(
            &mut self,
        ) -> Result<Option<crate::traits::Frame>, crate::traits::CaptureError> {
            Ok(self.inner.next_frame()?.map(|mut f| {
                f.pts_s += self.offset_s;
                f
            }))
        }
    }

    #[test]
    fn audio_lead_in_before_first_video_frame_is_dropped() {
        // Video starts 0.5 s after the shared clock origin; audio has been
        // capturing (and silence-filling) since t=0. The pre-video audio
        // must not ride in the file or video plays early by the lead-in.
        use crate::mock::MockAudioSource;
        let cap = OffsetCapture {
            inner: MockCapture::new(60, 30),
            offset_s: 0.5,
        };
        let mut rec = Recorder::new(cap, MockEncoder::new(30, 30), usize::MAX)
            .with_audio(Box::new(MockAudioSource::new(48_000, 20)));
        rec.run_to_end().unwrap();
        let segs: Vec<_> = rec.ring().unwrap().segments().collect();
        assert_eq!(segs.len(), 2);
        // First segment: audio coverage matches video duration within one
        // 20 ms packet (the packet straddling the boundary is dropped).
        let covered: f64 = segs[0].audio[0].samples.iter().map(|s| s.duration_s).sum();
        assert!(
            (covered - segs[0].duration_s).abs() <= 0.02 + 1e-9,
            "lead-in dropped: covered {covered}, video {}",
            segs[0].duration_s
        );
        // And the first kept packet starts at/after the video start.
        // (MockAudioSource stamps pts; we can't read them back from the
        // sealed track, but coverage bounds above imply it.)
    }

    #[test]
    fn straddling_audio_lead_in_does_not_break_full_session_finalization() {
        use crate::mock::MockAudioSource;

        let dir = TestDir::new("clipline-pipeline", "straddling-audio-origin");
        let full_path = dir.path().join("full.mp4");
        let cap = OffsetCapture {
            inner: MockCapture::new(60, 30),
            // Deliberately place the first video frame inside the 500--520 ms
            // Opus packet rather than on a 20 ms packet boundary.
            offset_s: 0.51,
        };
        let mut recorder = Recorder::new(cap, MockEncoder::new(30, 30), usize::MAX)
            .with_audio(Box::new(MockAudioSource::new(48_000, 20)));
        recorder
            .start_full_session(std::fs::File::create(&full_path).unwrap())
            .unwrap();

        recorder.run_to_end().unwrap();
        let summary = recorder.finish_full_session().unwrap().unwrap();
        let first = recorder.ring().unwrap().segments().next().unwrap();
        assert!(
            first.audio[0].pts_start_s.unwrap() >= first.pts_start_s - 1e-9,
            "the first kept Opus packet must not precede the video origin"
        );
        assert!((summary.duration_s - 2.0).abs() < 1e-6);
        assert!(
            clipline_mp4::walker::movie_duration_s(&std::fs::read(full_path).unwrap()).is_some()
        );
    }

    #[test]
    fn replay_drops_audio_packet_straddling_selected_video_origin() {
        use crate::mock::MockAudioSource;

        let cap = OffsetCapture {
            inner: MockCapture::new(60, 30),
            // GOP boundaries land at x.51 s, halfway through x.50--x.52 s
            // Opus packets.
            offset_s: 0.51,
        };
        let mut recorder = Recorder::new(cap, MockEncoder::new(30, 30), usize::MAX)
            .with_audio(Box::new(MockAudioSource::new(48_000, 20)));
        recorder.run_to_end().unwrap();

        let segments: Vec<_> = recorder.ring().unwrap().segments().collect();
        assert_eq!(segments.len(), 2);
        assert!(
            segments[1].audio[0].pts_start_s.unwrap() < segments[1].pts_start_s,
            "fixture must put a straddling Opus packet before the selected GOP origin"
        );

        let ReplayWindow::Memory(selected) = recorder.ring.save_window(0.25, None) else {
            panic!("fixture uses memory replay storage");
        };
        let origin = selected[0].pts_start_s;
        let track = &selected[0].audio[0];
        let selection = select_audio_after_replay_origin(
            track.pts_start_s,
            &track.samples,
            track.data.len(),
            origin,
        )
        .unwrap();
        assert!(
            (selection.pts_start_s.unwrap() - 1.52).abs() < 1e-9,
            "discarding the 1.50--1.52 s packet must advance audio by exactly one packet"
        );
        assert_eq!(selection.first_sample, 1);

        let (replay, _) = recorder
            .save_replay(std::io::Cursor::new(Vec::new()), 0.25, None)
            .expect("a mid-stream replay must discard audio preceding its video origin");
        assert!(clipline_mp4::walker::movie_duration_s(&replay.into_inner()).is_some());
    }

    #[test]
    fn replay_origin_filter_cleans_audio_from_every_selected_segment() {
        let sample = || SampleInfo {
            size: 1,
            duration_s: 0.02,
            is_sync: true,
        };
        let segment = |pts_start_s, audio_start_s, audio: Vec<u8>| Segment {
            starts_with_keyframe: true,
            pts_start_s,
            duration_s: 1.0,
            data: vec![0],
            samples: vec![sample()],
            audio: vec![TrackSamples {
                pts_start_s: Some(audio_start_s),
                samples: vec![sample(); audio.len()],
                data: audio,
            }],
        };
        let selected = [
            segment(1.0, 1.0, vec![1]),
            segment(2.0, 0.98, vec![2, 3, 4]),
        ];
        let original: Vec<_> = selected
            .iter()
            .map(|segment| segment.audio[0].data.clone())
            .collect();

        let selections: Vec<_> = selected
            .iter()
            .map(|segment| {
                let track = &segment.audio[0];
                select_audio_after_replay_origin(
                    track.pts_start_s,
                    &track.samples,
                    track.data.len(),
                    1.0,
                )
                .unwrap()
            })
            .collect();

        assert_eq!(selections[0].first_byte, 0);
        assert_eq!(selections[1].pts_start_s, Some(1.0));
        assert_eq!(
            &selected[1].audio[0].data[selections[1].first_byte..],
            &[3, 4]
        );
        assert_eq!(
            selected[1].audio[0].samples.len() - selections[1].first_sample,
            2
        );
        for (segment, data) in selected.iter().zip(original) {
            assert_eq!(segment.audio[0].data, data);
        }
    }

    #[test]
    fn trailing_partial_gop_is_sealed_at_end() {
        // 45 frames, GOP 30 → one full GOP + one 15-frame partial.
        let mut rec = Recorder::new(
            MockCapture::new(45, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        );
        rec.run_to_end().unwrap();
        let counts: Vec<usize> = rec
            .ring()
            .unwrap()
            .segments()
            .map(|s| s.samples.len())
            .collect();
        assert_eq!(counts, vec![30, 15]);
    }
}
