use std::io;
use std::io::{Seek, Write};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::TrySendError;

use clipline_buffer::{DiskReplayRing, ReplayRing, SampleInfo, Segment, TrackSamples};
use clipline_mp4::{HybridMp4Writer, TrackConfig};

use super::mux::{write_disk_replay_segment, write_memory_replay_segment};
use super::seal::{drop_audio_before_timeline, sealed_video_durations};
use super::session::{
    FullSessionSegment, FullSessionSink, FullSessionSummary, FullSessionWriteMsg, WriteSeek,
    finish_full_session_writer, release_message_reservation, spawn_full_session_writer,
    try_reserve_queue_bytes,
};
use super::storage::{
    FULL_SESSION_QUEUE_MAX_BYTES, FULL_SESSION_QUEUE_MAX_SEGMENTS,
    MAX_PENDING_GOP_DURATION_S, MID_STREAM_REPLAY_OPUS_PRE_SKIP, PipelineError, ReplayStorage,
    ReplayStorageConfig, ReplayWindow, pending_byte_budget,
};
use crate::traits::{
    AudioPacket, AudioSource, CaptureEngine, EncodeError, EncodedPacket, Encoder,
};


/// The recording pipeline (ddoc §3): capture → encode → GOP-aligned
/// segments → replay ring. Synchronous pull loop; production runs it on a
/// dedicated thread.
pub struct Recorder<C: CaptureEngine, E: Encoder> {
    capture: C,
    encoder: E,
    pub(crate) ring: ReplayStorage,
    pub(crate) pending: Vec<EncodedPacket>,
    /// Encoded video payload held for the current unsealed GOP.
    pub(crate) pending_bytes: usize,
    /// Encoded audio payload held across all tracks for the current GOP.
    pub(crate) pending_audio_bytes: usize,
    pending_byte_budget: usize,
    pre_keyframe_bytes: usize,
    pending_video_frames: usize,
    /// Encoders stamp the first sample with the configured frame duration;
    /// later variable-rate samples may span long static-screen gaps.
    nominal_video_duration_s: Option<f64>,
    audio_sources: Vec<Box<dyn AudioSource>>,
    pub(crate) pending_audio: Vec<Vec<AudioPacket>>,
    /// pts of the first video packet — the recording's timeline start.
    /// Audio captured before it (engine-init lead-in) is dropped so both
    /// tracks begin together in the file.
    pub(crate) video_start_pts_s: Option<f64>,
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
            pending_video_frames: 0,
            nominal_video_duration_s: None,
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
            pending_video_frames: 0,
            nominal_video_duration_s: None,
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
        self.validate_pending_limits()?;
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
            self.validate_pending_limits()?;
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

    /// Encoded payload bytes in the replay window that would be saved.
    /// Container tables and headers are not included.
    pub fn save_window_bytes(&self, window_s: f64, exclude_before_s: Option<f64>) -> usize {
        self.ring.save_window_bytes(window_s, exclude_before_s)
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

    pub(crate) fn start_full_session_with_limits<W: Write + Seek + Send + 'static>(
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

    /// Where the in-progress full session starts on the recording timeline,
    /// once its first segment has landed. The same value
    /// `FullSessionSummary::start_s` will report, so a live caller can place a
    /// marker exactly where the finished clip will show it. `None` when no
    /// session is being written, or before its first segment arrives.
    pub fn full_session_start_s(&self) -> Option<f64> {
        self.full_session.as_ref().and_then(|sink| sink.start_s)
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

    pub(crate) fn seal_pending(&mut self, boundary_pts_s: f64) -> Result<(), PipelineError> {
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
        self.pending_video_frames = 0;

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
                self.note_pending_video_frame(&pkt);
                self.pre_keyframe_bytes = self.pre_keyframe_bytes.saturating_add(pkt.data.len());
                return Ok(());
            }
            self.video_start_pts_s = Some(pkt.pts_s);
            self.pre_keyframe_bytes = 0;
            self.pending_video_frames = 0;
            drop_audio_before_timeline(&mut self.pending_audio, pkt.pts_s);
            self.recount_pending_audio_bytes();
        }

        if pkt.is_keyframe && !self.pending.is_empty() {
            self.validate_pending_limits()?;
            self.seal_pending(pkt.pts_s)?;
        }

        self.note_pending_video_frame(&pkt);
        self.pending_bytes = self.pending_bytes.saturating_add(pkt.data.len());
        self.pending.push(pkt);
        Ok(())
    }

    fn poll_audio_until(&mut self, until_pts_s: f64) -> Result<(), PipelineError> {
        let mut added_bytes = 0usize;
        for (source, pending) in self.audio_sources.iter_mut().zip(&mut self.pending_audio) {
            let packets = source.poll_packets(until_pts_s)?;
            added_bytes = packets.iter().fold(added_bytes, |total, packet| {
                total.saturating_add(packet.data.len())
            });
            pending.extend(packets);
        }
        self.pending_audio_bytes = self.pending_audio_bytes.saturating_add(added_bytes);
        Ok(())
    }

    fn finish_audio_until(&mut self, until_pts_s: f64) -> Result<(), PipelineError> {
        let mut added_bytes = 0usize;
        for (source, pending) in self.audio_sources.iter_mut().zip(&mut self.pending_audio) {
            let packets = source.finish_packets(until_pts_s)?;
            added_bytes = packets.iter().fold(added_bytes, |total, packet| {
                total.saturating_add(packet.data.len())
            });
            pending.extend(packets);
        }
        self.pending_audio_bytes = self.pending_audio_bytes.saturating_add(added_bytes);
        Ok(())
    }

    fn note_pending_video_frame(&mut self, packet: &EncodedPacket) {
        self.pending_video_frames = self.pending_video_frames.saturating_add(1);
        if self.nominal_video_duration_s.is_none()
            && packet.duration_s.is_finite()
            && packet.duration_s > 0.0
        {
            self.nominal_video_duration_s = Some(packet.duration_s);
        }
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

    fn validate_pending_limits(&self) -> Result<(), PipelineError> {
        let pending_payload_bytes = self.pending_payload_bytes();
        if pending_payload_bytes > self.pending_byte_budget {
            return Err(EncodeError::Backend(format!(
                "encoder did not produce a keyframe before pending video/audio GOP budget was exceeded ({pending_payload_bytes} > {} bytes)",
                self.pending_byte_budget
            ))
            .into());
        }
        if let Some(frame_duration_s) = self.nominal_video_duration_s {
            let duration_s = self.pending_video_frames as f64 * frame_duration_s;
            if duration_s.is_finite() && duration_s > MAX_PENDING_GOP_DURATION_S {
                return Err(EncodeError::Backend(format!(
                    "encoder did not produce a keyframe before pending GOP duration exceeded {:.1} seconds of encoded frame time ({duration_s:.3} seconds)",
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
