use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use clipline_mp4::{IndexedMovie, PlaybackTime, PlaybackTrackConfig, SeekPlan, TrackSampleRange};
use thiserror::Error;

use super::{D3D11VideoSurface, DecoderPreference, WindowsH264Decoder, WindowsWasapiRenderer};
use crate::{
    plan_audio_fill, plan_video_sample_buffers, AcceptedCommand, AdmitOutcome, AudioAvailability,
    AudioRenderer, AudioResetPoint, AudioTrackSpec, BackendComponent, BackendError,
    BackendErrorKind, ClockError, CommandInbox, ConvertedVideoSample, EncodedVideoPacket,
    EndOfStreamTracker, EnqueueError, EnqueueOutcome, FramePublisher, FrameScheduler,
    H264DecoderConfig, IndexedAudioPacketReader, LoadedVideoSample, MonotonicTime100ns,
    OpusDecoderBank, PipelineToken, PlaybackCommand, PlaybackEvent, PlaybackMetrics, PlaybackPhase,
    PlaybackSnapshot, PlaybackWorker, RebasedAudioClock, RecoveryDisposition, SeekTarget,
    SubmitStatus, TimelineAudioMixer, TimelineDuration, TimelinePosition, VideoDecoder,
    VideoSampleTransport, WorkGeneration, WorkerAction, WorkerActionKind, WorkerCompletion,
    WorkerError, WorkerSeekPlan, MAX_AUDIO_QUEUE_FRAMES, MAX_AUDIO_WRITE_FRAMES,
    MAX_OPUS_FRAME_SAMPLES, MAX_SELECTED_AUDIO_TRACKS, PLAYBACK_TIMELINE_HZ,
};

pub const SESSION_UPDATE_CAPACITY: usize = 64;
pub const SESSION_MAX_WAIT: Duration = Duration::from_millis(10);
const SESSION_IDLE_WAIT: Duration = Duration::from_millis(2);
const MAX_SESSION_PUMP_STEPS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionExit {
    Closed,
    ClientDisconnected,
}

#[derive(Debug, Error)]
pub enum SessionRunError {
    #[error(transparent)]
    Worker(#[from] WorkerError),
    #[error(transparent)]
    Update(#[from] SessionUpdateError),
    #[error("playback session invariant failed: {0}")]
    Invariant(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum SessionUpdatePayload {
    Snapshot(PlaybackSnapshot),
    Event(PlaybackEvent),
    Metrics(Box<PlaybackMetrics>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionUpdate {
    pub sequence: u64,
    pub token: PipelineToken,
    pub payload: SessionUpdatePayload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdatePublishOutcome {
    Queued,
    Replaced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SessionSendError {
    #[error("playback session command queue is full (capacity {capacity})")]
    Full { capacity: usize },
    #[error("playback session runtime is disconnected")]
    Disconnected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SessionUpdateError {
    #[error("playback session update queue is full (capacity {capacity})")]
    Full { capacity: usize },
    #[error("playback session client is disconnected")]
    Disconnected,
    #[error("playback session update sequence is exhausted")]
    SequenceExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateKey {
    Snapshot,
    Metrics,
}

impl SessionUpdatePayload {
    const fn coalescing_key(&self) -> Option<UpdateKey> {
        match self {
            Self::Snapshot(_) => Some(UpdateKey::Snapshot),
            Self::Metrics(_) => Some(UpdateKey::Metrics),
            Self::Event(_) => None,
        }
    }

    const fn is_terminal_event(&self) -> bool {
        matches!(
            self,
            Self::Event(
                PlaybackEvent::Ended { .. }
                    | PlaybackEvent::Error { .. }
                    | PlaybackEvent::Closed { .. }
            )
        )
    }
}

#[derive(Debug)]
struct SessionPorts {
    commands: CommandInbox,
    updates: VecDeque<SessionUpdate>,
    next_update_sequence: u64,
    client_connected: bool,
    runtime_connected: bool,
}

impl Default for SessionPorts {
    fn default() -> Self {
        Self {
            commands: CommandInbox::new(),
            updates: VecDeque::new(),
            next_update_sequence: 0,
            client_connected: true,
            runtime_connected: true,
        }
    }
}

#[derive(Debug)]
struct SessionShared {
    ports: Mutex<SessionPorts>,
    command_ready: Condvar,
    anchor: Instant,
}

#[derive(Debug)]
pub struct SessionClient {
    shared: Arc<SessionShared>,
}

#[derive(Debug)]
pub struct SessionRuntime {
    shared: Arc<SessionShared>,
}

pub fn session_channel() -> (SessionClient, SessionRuntime) {
    let shared = Arc::new(SessionShared {
        ports: Mutex::new(SessionPorts::default()),
        command_ready: Condvar::new(),
        anchor: Instant::now(),
    });
    (
        SessionClient {
            shared: Arc::clone(&shared),
        },
        SessionRuntime { shared },
    )
}

impl SessionClient {
    pub fn try_send(&self, command: PlaybackCommand) -> Result<EnqueueOutcome, SessionSendError> {
        let elapsed_100ns = self.shared.anchor.elapsed().as_nanos() / 100;
        let accepted_at = MonotonicTime100ns::new(u64::try_from(elapsed_100ns).unwrap_or(u64::MAX));
        self.try_send_at(command, accepted_at)
    }

    pub fn try_send_at(
        &self,
        command: PlaybackCommand,
        accepted_at: MonotonicTime100ns,
    ) -> Result<EnqueueOutcome, SessionSendError> {
        let mut ports = self
            .shared
            .ports
            .lock()
            .map_err(|_| SessionSendError::Disconnected)?;
        if !ports.runtime_connected {
            return Err(SessionSendError::Disconnected);
        }
        let outcome =
            ports
                .commands
                .enqueue_at(command, accepted_at)
                .map_err(|error| match error {
                    EnqueueError::QueueFull { capacity } => SessionSendError::Full { capacity },
                })?;
        drop(ports);
        self.shared.command_ready.notify_one();
        Ok(outcome)
    }

    pub fn try_recv_update(&self) -> Option<SessionUpdate> {
        self.shared
            .ports
            .lock()
            .ok()
            .and_then(|mut ports| ports.updates.pop_front())
    }
}

impl Drop for SessionClient {
    fn drop(&mut self) {
        if let Ok(mut ports) = self.shared.ports.lock() {
            ports.client_connected = false;
        }
        self.shared.command_ready.notify_all();
    }
}

impl SessionRuntime {
    pub fn try_recv_command(&mut self) -> Option<AcceptedCommand> {
        self.shared
            .ports
            .lock()
            .ok()
            .and_then(|mut ports| ports.commands.pop_front_accepted())
    }

    pub fn wait_for_command(&mut self, timeout: Duration) -> Option<AcceptedCommand> {
        let mut ports = self.shared.ports.lock().ok()?;
        if ports.commands.is_empty() && ports.client_connected {
            let waited = self
                .shared
                .command_ready
                .wait_timeout(ports, timeout.min(SESSION_MAX_WAIT))
                .ok()?;
            ports = waited.0;
        }
        ports.commands.pop_front_accepted()
    }

    pub fn publish_update(
        &mut self,
        token: PipelineToken,
        payload: SessionUpdatePayload,
    ) -> Result<UpdatePublishOutcome, SessionUpdateError> {
        let mut ports = self
            .shared
            .ports
            .lock()
            .map_err(|_| SessionUpdateError::Disconnected)?;
        if !ports.client_connected {
            return Err(SessionUpdateError::Disconnected);
        }
        let replacement = payload.coalescing_key().and_then(|key| {
            ports
                .updates
                .iter()
                .enumerate()
                .rev()
                .take_while(|(_, update)| !matches!(update.payload, SessionUpdatePayload::Event(_)))
                .find_map(|(index, update)| {
                    (update.payload.coalescing_key() == Some(key)).then_some(index)
                })
        });
        let limit = if payload.is_terminal_event() {
            SESSION_UPDATE_CAPACITY
        } else {
            SESSION_UPDATE_CAPACITY - 1
        };
        if replacement.is_none() && ports.updates.len() >= limit {
            return Err(SessionUpdateError::Full {
                capacity: SESSION_UPDATE_CAPACITY,
            });
        }
        let sequence = ports
            .next_update_sequence
            .checked_add(1)
            .ok_or(SessionUpdateError::SequenceExhausted)?;
        ports.next_update_sequence = sequence;
        let update = SessionUpdate {
            sequence,
            token,
            payload,
        };
        if let Some(index) = replacement {
            ports.updates[index] = update;
            Ok(UpdatePublishOutcome::Replaced)
        } else {
            ports.updates.push_back(update);
            Ok(UpdatePublishOutcome::Queued)
        }
    }

    pub fn run<P>(mut self, publisher: P) -> Result<SessionExit, SessionRunError>
    where
        P: FramePublisher<D3D11VideoSurface>,
    {
        let mut worker = PlaybackWorker::new();
        let mut pipeline = SessionPipeline::new(publisher, self.shared.anchor);
        let mut pending_action: Option<WorkerAction> = None;
        let mut last_snapshot = None;
        let mut last_metrics_publish = Instant::now();
        let mut last_terminal_metrics = None;

        if !self.publish_worker_updates(&mut worker, &mut last_snapshot)? {
            return Ok(SessionExit::ClientDisconnected);
        }

        loop {
            if !self.client_connected() {
                pipeline.close_media();
                return Ok(SessionExit::ClientDisconnected);
            }

            let mut made_progress = false;
            if let Some(accepted) = self.try_recv_command() {
                worker
                    .enqueue(accepted.command, accepted.accepted_at)
                    .map_err(|error| {
                        SessionRunError::Invariant(format!(
                            "session command transfer exceeded its identical worker bound: {error}"
                        ))
                    })?;
                made_progress = true;
            }

            match worker.next_action() {
                Ok(Some(action)) => {
                    pending_action = Some(action);
                    made_progress = true;
                }
                Ok(None) => {
                    if pending_action
                        .as_ref()
                        .is_some_and(|action| action.token() != worker.token())
                    {
                        pending_action = None;
                    }
                }
                Err(WorkerError::Command(error)) => {
                    let event = PlaybackEvent::Error {
                        generation: worker.token().work(),
                        message: error.to_string(),
                    };
                    if !self.publish_payload(worker.token(), SessionUpdatePayload::Event(event))? {
                        pipeline.close_media();
                        return Ok(SessionExit::ClientDisconnected);
                    }
                    made_progress = true;
                }
                Err(error) => {
                    pipeline.close_media();
                    return Err(error.into());
                }
            }

            if let Some(action) = pending_action.clone() {
                match pipeline.execute(&action) {
                    Ok(ActionProgress::Complete(completion)) => {
                        pending_action = None;
                        worker.complete(&action, completion)?;
                        made_progress = true;
                    }
                    Ok(ActionProgress::Pending) => {}
                    Err(error) => {
                        pipeline.note_failure(&error);
                        pending_action = None;
                        worker.fail(&action, error)?;
                        if worker.snapshot().phase == PlaybackPhase::Failed {
                            pipeline.close_media();
                        }
                        made_progress = true;
                    }
                }
            } else if matches!(
                worker.snapshot().phase,
                PlaybackPhase::Playing | PlaybackPhase::Paused
            ) {
                match pipeline.service_ready(&mut worker) {
                    Ok(progressed) => made_progress |= progressed,
                    Err(error) => {
                        pipeline.note_failure(&error);
                        worker.report_failure(worker.token(), error)?;
                        if worker.snapshot().phase == PlaybackPhase::Failed {
                            pipeline.close_media();
                        }
                        made_progress = true;
                    }
                }
            }

            if !self.publish_worker_updates(&mut worker, &mut last_snapshot)? {
                pipeline.close_media();
                return Ok(SessionExit::ClientDisconnected);
            }
            let terminal_key = matches!(
                worker.snapshot().phase,
                PlaybackPhase::Closed | PlaybackPhase::Failed
            )
            .then_some((worker.token(), worker.snapshot().phase))
            .filter(|(token, _)| token.work() != WorkGeneration::INITIAL);
            let terminal_metrics = terminal_key.is_some() && terminal_key != last_terminal_metrics;
            if terminal_metrics || last_metrics_publish.elapsed() >= Duration::from_millis(250) {
                if let Some(metrics) = pipeline.metrics_snapshot() {
                    if !self.publish_payload(
                        worker.token(),
                        SessionUpdatePayload::Metrics(Box::new(metrics)),
                    )? {
                        pipeline.close_media();
                        return Ok(SessionExit::ClientDisconnected);
                    }
                }
                last_metrics_publish = Instant::now();
                if terminal_metrics {
                    last_terminal_metrics = terminal_key;
                }
            }

            if worker.snapshot().phase == PlaybackPhase::Closed
                && pending_action.is_none()
                && last_snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.phase == PlaybackPhase::Closed)
                && worker.token().work() != WorkGeneration::INITIAL
            {
                pipeline.close_media();
                return Ok(SessionExit::Closed);
            }

            if !made_progress {
                if let Some(accepted) = self.wait_for_command(SESSION_IDLE_WAIT) {
                    worker
                        .enqueue(accepted.command, accepted.accepted_at)
                        .map_err(|error| {
                            SessionRunError::Invariant(format!(
                                "waited session command exceeded its identical worker bound: {error}"
                            ))
                        })?;
                }
            } else {
                std::thread::yield_now();
            }
        }
    }

    fn client_connected(&self) -> bool {
        self.shared
            .ports
            .lock()
            .is_ok_and(|ports| ports.client_connected)
    }

    fn publish_worker_updates(
        &mut self,
        worker: &mut PlaybackWorker,
        last_snapshot: &mut Option<PlaybackSnapshot>,
    ) -> Result<bool, SessionRunError> {
        let snapshot = worker.snapshot();
        if last_snapshot.as_ref() != Some(&snapshot) {
            if !self.publish_payload(
                worker.token(),
                SessionUpdatePayload::Snapshot(snapshot.clone()),
            )? {
                return Ok(false);
            }
            *last_snapshot = Some(snapshot);
        }
        for event in worker.take_events() {
            if !self.publish_payload(worker.token(), SessionUpdatePayload::Event(event))? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn publish_payload(
        &mut self,
        token: PipelineToken,
        payload: SessionUpdatePayload,
    ) -> Result<bool, SessionRunError> {
        match self.publish_update(token, payload) {
            Ok(_) => Ok(true),
            Err(SessionUpdateError::Disconnected) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
}

impl Drop for SessionRuntime {
    fn drop(&mut self) {
        if let Ok(mut ports) = self.shared.ports.lock() {
            ports.runtime_connected = false;
        }
        self.shared.command_ready.notify_all();
    }
}

struct SessionMedia {
    duration: PlaybackTime,
    video: VideoSampleTransport<File>,
    video_config: H264DecoderConfig,
    video_timescale: u32,
    video_sample_count: usize,
    video_end: TimelinePosition,
    decoder: WindowsH264Decoder,
    renderer: WindowsWasapiRenderer,
    audio_reader: IndexedAudioPacketReader<File>,
    audio_specs: BTreeMap<usize, AudioTrackSpec>,
    audio_bank: OpusDecoderBank,
    audio_mixer: TimelineAudioMixer,
    scheduler: FrameScheduler<D3D11VideoSurface>,
    accumulated_metrics: PlaybackMetrics,
    clock: RebasedAudioClock,
    eos: EndOfStreamTracker,
    settled_position: PlaybackTime,
    backend_token: PipelineToken,
    loaded_video: Option<LoadedVideoSample>,
    converted_video: Option<ConvertedVideoSample>,
    next_video_sample: usize,
    video_drain_sent: bool,
    audio_tracks: Vec<usize>,
    audio_sample_count: usize,
    next_audio_sample: usize,
    audio_finished: bool,
    audio_playback_start: u64,
    audio_mix_scratch: Vec<f32>,
    audio_output: Vec<f32>,
    started: bool,
    recreate_decoder: bool,
    recreate_audio: bool,
}

impl SessionMedia {
    fn open(path: &Path, token: PipelineToken) -> Result<Self, BackendError> {
        let video_movie = IndexedMovie::open(path)
            .map_err(|error| corrupt_media(BackendComponent::VideoDecoder, error))?;
        let video_track_index = video_movie
            .index()
            .tracks
            .iter()
            .position(|track| matches!(track.config, PlaybackTrackConfig::H264 { .. }))
            .ok_or_else(|| unavailable_media("media has no native H.264 video track"))?;
        let video_track = &video_movie.index().tracks[video_track_index];
        let video_plan = plan_video_sample_buffers(video_track, Default::default())
            .map_err(|error| corrupt_media(BackendComponent::VideoDecoder, error))?;
        let video_config = video_plan.config;
        let video_timescale = video_track.timescale;
        let video_sample_count = video_track.samples.len();
        let final_sample = video_track
            .samples
            .last()
            .ok_or_else(|| unavailable_media("H.264 video track has no samples"))?;
        let final_pts = u64::try_from(final_sample.pts).map_err(|_| {
            corrupt_media(
                BackendComponent::VideoDecoder,
                "final H.264 sample has a negative presentation timestamp",
            )
        })?;
        let final_end = final_pts
            .checked_add(u64::from(final_sample.duration))
            .ok_or_else(|| {
                corrupt_media(
                    BackendComponent::VideoDecoder,
                    "final H.264 sample interval overflows",
                )
            })?;
        let video_end = TimelinePosition::new(rescale_to_timeline(
            final_end,
            video_timescale,
            BackendComponent::VideoDecoder,
        )?);
        let duration = PlaybackTime::new(
            video_movie.index().duration_ticks,
            video_movie.index().movie_timescale,
        )
        .map_err(|error| corrupt_media(BackendComponent::VideoDecoder, error))?;
        let video = VideoSampleTransport::new(video_movie, video_track_index, token.work())
            .map_err(|error| corrupt_media(BackendComponent::VideoDecoder, error))?;

        let audio_movie = IndexedMovie::open(path)
            .map_err(|error| corrupt_media(BackendComponent::AudioRenderer, error))?;
        let mut audio_specs = BTreeMap::new();
        let mut audio_tracks = Vec::new();
        let mut audio_ranges = Vec::new();
        let mut audio_sample_count = None;
        for (track_index, track) in audio_movie.index().tracks.iter().enumerate() {
            let PlaybackTrackConfig::Opus {
                channels,
                sample_rate,
                pre_skip,
            } = track.config
            else {
                continue;
            };
            if audio_tracks.len() >= MAX_SELECTED_AUDIO_TRACKS {
                return Err(corrupt_media(
                    BackendComponent::AudioRenderer,
                    format!(
                        "media selects more than {MAX_SELECTED_AUDIO_TRACKS} default Opus tracks"
                    ),
                ));
            }
            if let Some(expected) = audio_sample_count {
                if expected != track.samples.len() {
                    return Err(corrupt_media(
                        BackendComponent::AudioRenderer,
                        "selected Clipline Opus tracks have different packet counts",
                    ));
                }
            } else {
                audio_sample_count = Some(track.samples.len());
            }
            let spec = AudioTrackSpec::new(track_index, channels, sample_rate, pre_skip)
                .map_err(|error| corrupt_media(BackendComponent::AudioRenderer, error))?;
            audio_specs.insert(track_index, spec);
            audio_tracks.push(track_index);
            audio_ranges.push(TrackSampleRange {
                track_index,
                samples: 0..track.samples.len(),
            });
        }
        let timeline_end = PlaybackTime::new(video_end.ticks(), PLAYBACK_TIMELINE_HZ)
            .map_err(|error| corrupt_media(BackendComponent::AudioRenderer, error))?;
        let audio_reader =
            IndexedAudioPacketReader::new(audio_movie, audio_ranges, timeline_end, token.work())
                .map_err(|error| corrupt_media(BackendComponent::AudioRenderer, error))?;
        let mut audio_bank = OpusDecoderBank::new();
        let selected_specs: Vec<_> = audio_tracks
            .iter()
            .filter_map(|track| audio_specs.get(track).copied())
            .collect();
        audio_bank
            .select_tracks(&selected_specs, token.work(), AudioResetPoint::FileStart)
            .map_err(|error| corrupt_media(BackendComponent::AudioRenderer, error))?;
        let audio_mixer = TimelineAudioMixer::new(MAX_AUDIO_QUEUE_FRAMES, 0)
            .map_err(|error| corrupt_media(BackendComponent::AudioRenderer, error))?;

        let mut decoder = WindowsH264Decoder::new(DecoderPreference::PreferHardware)?;
        decoder.configure(&video_config, token)?;
        let mut renderer = WindowsWasapiRenderer::open_default()?;
        renderer.reset(token)?;
        let raw = renderer.raw_clock()?;
        let mut clock = RebasedAudioClock::new(raw, TimelinePosition::new(0));
        clock
            .pause(raw)
            .map_err(|error| clock_backend_error("freeze initial audio clock", error))?;
        let seek_target = SeekTarget::new(TimelinePosition::new(0), 0);

        Ok(Self {
            duration,
            video,
            video_config,
            video_timescale,
            video_sample_count,
            video_end,
            decoder,
            renderer,
            audio_reader,
            audio_specs,
            audio_bank,
            audio_mixer,
            scheduler: FrameScheduler::new(token, seek_target),
            accumulated_metrics: PlaybackMetrics::default(),
            clock,
            eos: EndOfStreamTracker::new(video_end),
            settled_position: PlaybackTime {
                ticks: 0,
                timescale: video_timescale,
            },
            backend_token: token,
            loaded_video: None,
            converted_video: None,
            next_video_sample: 0,
            video_drain_sent: false,
            audio_tracks,
            audio_sample_count: audio_sample_count.unwrap_or(0),
            next_audio_sample: 0,
            audio_finished: false,
            audio_playback_start: 0,
            audio_mix_scratch: vec![0.0; MAX_OPUS_FRAME_SAMPLES * 2],
            audio_output: vec![0.0; MAX_AUDIO_WRITE_FRAMES * 2],
            started: false,
            recreate_decoder: false,
            recreate_audio: false,
        })
    }

    fn default_audio_tracks(&self) -> Vec<usize> {
        self.audio_specs.keys().copied().collect()
    }

    fn note_failure(&mut self, error: &BackendError) {
        if error.recovery != RecoveryDisposition::RecreateComponent {
            return;
        }
        match error.component {
            BackendComponent::VideoDecoder => self.recreate_decoder = true,
            BackendComponent::AudioRenderer => self.recreate_audio = true,
            BackendComponent::FramePublisher => {}
        }
    }

    fn close(&mut self) {
        if self.started {
            let _ = self.renderer.pause(self.backend_token);
            self.started = false;
        }
        self.scheduler.replace_pipeline(
            self.backend_token,
            SeekTarget::new(self.clock.position(), self.next_video_sample),
        );
        self.loaded_video = None;
        self.converted_video = None;
        self.audio_mixer.reset_at(self.clock.position().ticks());
        self.decoder.close();
        self.renderer.close();
    }

    fn metrics_snapshot(&self) -> PlaybackMetrics {
        let mut metrics = self.accumulated_metrics.clone();
        metrics.accumulate_generation(self.scheduler.metrics());
        metrics
    }
}

fn corrupt_media(component: BackendComponent, error: impl std::fmt::Display) -> BackendError {
    BackendError {
        component,
        kind: BackendErrorKind::CorruptInput,
        recovery: RecoveryDisposition::Fatal,
        native_code: None,
        message: error.to_string(),
    }
}

fn unavailable_media(message: impl Into<String>) -> BackendError {
    BackendError {
        component: BackendComponent::VideoDecoder,
        kind: BackendErrorKind::Unavailable,
        recovery: RecoveryDisposition::Fatal,
        native_code: None,
        message: message.into(),
    }
}

fn retry_media(component: BackendComponent, message: impl Into<String>) -> BackendError {
    BackendError {
        component,
        kind: BackendErrorKind::StaleWork,
        recovery: RecoveryDisposition::RetryPipeline,
        native_code: None,
        message: message.into(),
    }
}

fn clock_backend_error(operation: &'static str, error: ClockError) -> BackendError {
    BackendError {
        component: BackendComponent::AudioRenderer,
        kind: BackendErrorKind::EndpointInvalidated,
        recovery: RecoveryDisposition::RecreateComponent,
        native_code: None,
        message: format!("{operation}: {error}"),
    }
}

fn rescale_to_timeline(
    value: u64,
    timescale: u32,
    component: BackendComponent,
) -> Result<u64, BackendError> {
    if timescale == 0 {
        return Err(corrupt_media(component, "media timescale is zero"));
    }
    let scaled = u128::from(value)
        .checked_mul(u128::from(PLAYBACK_TIMELINE_HZ))
        .ok_or_else(|| corrupt_media(component, "timeline scaling overflow"))?
        / u128::from(timescale);
    u64::try_from(scaled).map_err(|_| corrupt_media(component, "timeline value exceeds u64"))
}

fn timeline_position(pts: i64, timescale: u32) -> Result<TimelinePosition, BackendError> {
    let pts = u64::try_from(pts)
        .map_err(|_| corrupt_media(BackendComponent::VideoDecoder, "negative video PTS"))?;
    Ok(TimelinePosition::new(rescale_to_timeline(
        pts,
        timescale,
        BackendComponent::VideoDecoder,
    )?))
}

fn timeline_duration(duration: u32, timescale: u32) -> Result<TimelineDuration, BackendError> {
    let ticks = rescale_to_timeline(
        u64::from(duration),
        timescale,
        BackendComponent::VideoDecoder,
    )?;
    TimelineDuration::new(ticks)
        .map_err(|error| corrupt_media(BackendComponent::VideoDecoder, error))
}

enum ActionProgress {
    Complete(WorkerCompletion),
    Pending,
}

struct SessionPipeline<P> {
    publisher: P,
    media: Option<SessionMedia>,
    final_metrics: Option<PlaybackMetrics>,
    anchor: Instant,
}

impl<P> SessionPipeline<P>
where
    P: FramePublisher<D3D11VideoSurface>,
{
    fn new(publisher: P, anchor: Instant) -> Self {
        Self {
            publisher,
            media: None,
            final_metrics: None,
            anchor,
        }
    }

    fn execute(&mut self, action: &WorkerAction) -> Result<ActionProgress, BackendError> {
        match action.kind() {
            WorkerActionKind::IndexOpen { path } => {
                self.close_media();
                self.publisher.clear(action.token())?;
                let media = SessionMedia::open(path, action.token())?;
                let completion = WorkerCompletion::Indexed {
                    duration: media.duration,
                    video_sample_count: media.video_sample_count,
                    default_audio_track_indices: media.default_audio_tracks(),
                };
                self.final_metrics = None;
                self.media = Some(media);
                Ok(ActionProgress::Complete(completion))
            }
            WorkerActionKind::Flush => {
                let media = self.require_media()?;
                media.flush(action.token())?;
                self.publisher.clear(action.token())?;
                Ok(ActionProgress::Complete(WorkerCompletion::Done))
            }
            WorkerActionKind::PlanSeek {
                requested,
                audio_track_indices,
                step_frames,
                accepted_at,
            } => {
                let media = self.require_media()?;
                let plan = media.prepare_seek(
                    *requested,
                    audio_track_indices,
                    *step_frames,
                    *accepted_at,
                    action.token(),
                )?;
                Ok(ActionProgress::Complete(WorkerCompletion::SeekPlanned(
                    plan,
                )))
            }
            WorkerActionKind::ReadVideo { sample_index } => {
                let media = self.require_media()?;
                media.loaded_video = Some(
                    media
                        .video
                        .read_encoded_sample(*sample_index, action.token().work())
                        .map_err(|error| corrupt_media(BackendComponent::VideoDecoder, error))?,
                );
                Ok(ActionProgress::Complete(WorkerCompletion::Done))
            }
            WorkerActionKind::ConvertVideo { sample_index } => {
                let media = self.require_media()?;
                let loaded = media.loaded_video.take().ok_or_else(|| {
                    retry_media(
                        BackendComponent::VideoDecoder,
                        "video conversion has no loaded sample",
                    )
                })?;
                if loaded.sample_index() != *sample_index {
                    return Err(retry_media(
                        BackendComponent::VideoDecoder,
                        "video conversion sample does not match worker action",
                    ));
                }
                media.converted_video = Some(
                    media
                        .video
                        .prepare_loaded_sample(loaded, action.token().work())
                        .map_err(|error| corrupt_media(BackendComponent::VideoDecoder, error))?,
                );
                Ok(ActionProgress::Complete(WorkerCompletion::Done))
            }
            WorkerActionKind::DecodeVideo { sample_index } => {
                let media = self.require_media()?;
                media.decode_worker_sample(*sample_index, action.token())
            }
            WorkerActionKind::ProduceAudio => {
                self.require_media()?.prime_audio(action.token())?;
                Ok(ActionProgress::Complete(WorkerCompletion::Done))
            }
            WorkerActionKind::PublishVideo { sample_index } => {
                let now = self.monotonic_now();
                let (media, publisher) = self.media_and_publisher()?;
                media.publish_worker_target(*sample_index, action.token(), publisher, now)
            }
            WorkerActionKind::SetTransport { playing } => {
                self.require_media()?
                    .set_transport(*playing, action.token())?;
                Ok(ActionProgress::Complete(WorkerCompletion::Done))
            }
            WorkerActionKind::SetVolume { volume } => {
                self.require_media()?.renderer.set_volume(*volume)?;
                Ok(ActionProgress::Complete(WorkerCompletion::Done))
            }
            WorkerActionKind::CloseBackends => {
                self.close_media();
                self.publisher.clear(action.token())?;
                Ok(ActionProgress::Complete(WorkerCompletion::Done))
            }
        }
    }

    fn service_ready(&mut self, worker: &mut PlaybackWorker) -> Result<bool, BackendError> {
        let snapshot = worker.snapshot();
        if !matches!(
            snapshot.phase,
            PlaybackPhase::Playing | PlaybackPhase::Paused
        ) {
            return Ok(false);
        }
        let now = self.monotonic_now();
        let token = worker.token();
        let (media, publisher) = self.media_and_publisher()?;
        if snapshot.phase == PlaybackPhase::Paused {
            let was_started = media.started;
            media.set_transport(false, token)?;
            return Ok(was_started);
        }
        media.set_transport(true, token)?;
        let position = media.service_playback(publisher, now)?;
        let playback_position = PlaybackTime::new(position.ticks(), PLAYBACK_TIMELINE_HZ)
            .map_err(|error| corrupt_media(BackendComponent::AudioRenderer, error))?;
        worker.report_position(token, playback_position);
        if media.eos.update(position) {
            media.set_transport(false, token)?;
            worker.report_ended(token, playback_position);
        }
        Ok(false)
    }

    fn metrics_snapshot(&self) -> Option<PlaybackMetrics> {
        self.media
            .as_ref()
            .map(SessionMedia::metrics_snapshot)
            .or_else(|| self.final_metrics.clone())
    }

    fn note_failure(&mut self, error: &BackendError) {
        if let Some(media) = self.media.as_mut() {
            media.note_failure(error);
        }
    }

    fn close_media(&mut self) {
        if let Some(mut media) = self.media.take() {
            self.final_metrics = Some(media.metrics_snapshot());
            media.close();
        }
    }

    fn require_media(&mut self) -> Result<&mut SessionMedia, BackendError> {
        self.media.as_mut().ok_or_else(|| {
            unavailable_media("playback session action requires indexed media resources")
        })
    }

    fn media_and_publisher(&mut self) -> Result<(&mut SessionMedia, &mut P), BackendError> {
        let media = self.media.as_mut().ok_or_else(|| {
            unavailable_media("playback session action requires indexed media resources")
        })?;
        Ok((media, &mut self.publisher))
    }

    fn monotonic_now(&self) -> MonotonicTime100ns {
        let ticks = self.anchor.elapsed().as_nanos() / 100;
        MonotonicTime100ns::new(u64::try_from(ticks).unwrap_or(u64::MAX))
    }
}

impl<P> Drop for SessionPipeline<P> {
    fn drop(&mut self) {
        if let Some(mut media) = self.media.take() {
            media.close();
        }
    }
}

impl SessionMedia {
    fn flush(&mut self, token: PipelineToken) -> Result<(), BackendError> {
        if self.started {
            if !self.recreate_audio {
                self.renderer.pause(self.backend_token)?;
            }
            self.started = false;
        }
        if self.recreate_decoder {
            self.decoder.close();
            self.decoder.configure(&self.video_config, token)?;
            self.recreate_decoder = false;
        } else {
            self.decoder.flush(token)?;
        }
        if self.recreate_audio {
            self.renderer.reopen(token)?;
            self.recreate_audio = false;
        } else {
            self.renderer.reset(token)?;
        }
        let raw = self.renderer.raw_clock()?;
        self.clock.rebase(raw, self.clock.position());
        self.clock
            .pause(raw)
            .map_err(|error| clock_backend_error("freeze flushed audio clock", error))?;
        self.backend_token = token;
        self.video.reset_for_generation(token.work());
        self.audio_reader.reset_generation(token.work());
        self.loaded_video = None;
        self.converted_video = None;
        self.audio_mixer.reset_at(self.clock.position().ticks());
        self.next_video_sample = 0;
        self.video_drain_sent = false;
        self.audio_finished = false;
        Ok(())
    }

    fn prepare_seek(
        &mut self,
        requested: PlaybackTime,
        audio_track_indices: &[usize],
        step_frames: Option<i32>,
        accepted_at: MonotonicTime100ns,
        token: PipelineToken,
    ) -> Result<WorkerSeekPlan, BackendError> {
        let exact_step = step_frames
            .map(|frames| self.video.resolve_step_target(requested, frames))
            .transpose()
            .map_err(|error| corrupt_media(BackendComponent::VideoDecoder, error))?;
        let target_request = exact_step.map_or(requested, |target| target.time);
        let plan = self
            .video
            .seek_plan(audio_track_indices, target_request)
            .map_err(|error| corrupt_media(BackendComponent::VideoDecoder, error))?;
        let worker_plan = WorkerSeekPlan::try_from(&plan)
            .map_err(|error| corrupt_media(BackendComponent::VideoDecoder, error))?;
        if exact_step.is_some_and(|target| target.sample_index != worker_plan.target_sample_index) {
            return Err(corrupt_media(
                BackendComponent::VideoDecoder,
                "frame step did not resolve to the exact source sample",
            ));
        }
        let target_tick = rescale_to_timeline(
            plan.target_time.ticks,
            plan.target_time.timescale,
            BackendComponent::VideoDecoder,
        )?;
        let target = TimelinePosition::new(target_tick);

        self.prepare_audio_seek(&plan, audio_track_indices, target_tick, token.work())?;
        let raw = self.renderer.raw_clock()?;
        self.clock.rebase(raw, target);
        self.clock
            .pause(raw)
            .map_err(|error| clock_backend_error("freeze settled seek clock", error))?;
        self.accumulated_metrics
            .accumulate_generation(self.scheduler.metrics());
        self.scheduler.begin_seek(
            token,
            SeekTarget::new(target, worker_plan.target_sample_index),
            accepted_at,
        );
        self.eos = EndOfStreamTracker::new(self.video_end);
        self.settled_position = plan.target_time;
        self.next_video_sample = plan.video_preroll.samples.start;
        self.video_drain_sent = false;
        self.audio_playback_start = target_tick;
        self.backend_token = token;
        Ok(worker_plan)
    }

    fn prepare_audio_seek(
        &mut self,
        plan: &SeekPlan,
        selected_tracks: &[usize],
        target_tick: u64,
        generation: WorkGeneration,
    ) -> Result<(), BackendError> {
        let specs: Vec<_> = selected_tracks
            .iter()
            .map(|track| {
                self.audio_specs.get(track).copied().ok_or_else(|| {
                    corrupt_media(
                        BackendComponent::AudioRenderer,
                        format!("selected audio track {track} has no Opus specification"),
                    )
                })
            })
            .collect::<Result<_, _>>()?;
        let reset_point = if plan.video_preroll.samples.end == 1 {
            AudioResetPoint::FileStart
        } else {
            AudioResetPoint::MidStream
        };
        self.audio_bank
            .select_tracks(&specs, generation, reset_point)
            .map_err(|error| corrupt_media(BackendComponent::AudioRenderer, error))?;
        self.audio_bank
            .reset_for_seek(generation, reset_point)
            .map_err(|error| corrupt_media(BackendComponent::AudioRenderer, error))?;

        let mut ranges = Vec::with_capacity(selected_tracks.len());
        let mut first_audio_sample = None;
        let mut audio_sample_count = None;
        for selected in &plan.audio_preroll {
            if first_audio_sample.is_some_and(|expected| expected != selected.samples.start) {
                return Err(corrupt_media(
                    BackendComponent::AudioRenderer,
                    "selected Clipline Opus seek ranges are not packet-aligned",
                ));
            }
            first_audio_sample.get_or_insert(selected.samples.start);
            let end = self.audio_reader.index().tracks[selected.track_index]
                .samples
                .len();
            if audio_sample_count.is_some_and(|expected| expected != end) {
                return Err(corrupt_media(
                    BackendComponent::AudioRenderer,
                    "selected Clipline Opus tracks have different packet counts",
                ));
            }
            audio_sample_count.get_or_insert(end);
            ranges.push(TrackSampleRange {
                track_index: selected.track_index,
                samples: selected.samples.start..end,
            });
        }
        self.audio_reader
            .reselect_ranges(
                ranges,
                PlaybackTime::new(self.video_end.ticks(), PLAYBACK_TIMELINE_HZ)
                    .map_err(|error| corrupt_media(BackendComponent::AudioRenderer, error))?,
                generation,
            )
            .map_err(|error| corrupt_media(BackendComponent::AudioRenderer, error))?;
        self.audio_mixer.reset_at(target_tick);
        self.audio_tracks = selected_tracks.to_vec();
        self.audio_sample_count = audio_sample_count.unwrap_or(0);
        self.next_audio_sample = first_audio_sample.unwrap_or(0);
        self.audio_finished = selected_tracks.is_empty();
        Ok(())
    }

    fn decode_worker_sample(
        &mut self,
        sample_index: usize,
        token: PipelineToken,
    ) -> Result<ActionProgress, BackendError> {
        let clock = self.sample_clock()?;
        self.receive_decoder(clock)?;
        let converted = self.converted_video.ok_or_else(|| {
            retry_media(
                BackendComponent::VideoDecoder,
                "video decode has no converted sample",
            )
        })?;
        if converted.sample_index() != sample_index {
            return Err(retry_media(
                BackendComponent::VideoDecoder,
                "video decode sample does not match worker action",
            ));
        }
        let (status, parameter_sets) = {
            let unit = self
                .video
                .converted_sample(converted, token.work())
                .map_err(|error| corrupt_media(BackendComponent::VideoDecoder, error))?;
            let status = self.decoder.submit(
                EncodedVideoPacket {
                    bytes: unit.bytes,
                    sample_index: unit.sample_index,
                    pts: timeline_position(unit.pts, self.video_timescale)?,
                    duration: timeline_duration(unit.duration, self.video_timescale)?,
                    is_sync: unit.is_sync,
                },
                token,
            )?;
            (status, unit.parameter_set_submission)
        };
        if status == SubmitStatus::Backpressured {
            return Ok(ActionProgress::Pending);
        }
        if let Some(submission) = parameter_sets {
            if !self.video.commit_parameter_sets(submission) {
                return Err(retry_media(
                    BackendComponent::VideoDecoder,
                    "decoder accepted stale H.264 parameter-set work",
                ));
            }
        }
        self.converted_video = None;
        self.next_video_sample = sample_index.checked_add(1).ok_or_else(|| {
            corrupt_media(BackendComponent::VideoDecoder, "sample cursor overflow")
        })?;
        Ok(ActionProgress::Complete(WorkerCompletion::Done))
    }

    fn publish_worker_target<P>(
        &mut self,
        sample_index: usize,
        token: PipelineToken,
        publisher: &mut P,
        now: MonotonicTime100ns,
    ) -> Result<ActionProgress, BackendError>
    where
        P: FramePublisher<D3D11VideoSurface>,
    {
        if token != self.backend_token {
            return Err(retry_media(
                BackendComponent::FramePublisher,
                "seek publication token does not match live backend token",
            ));
        }
        let clock = self.sample_clock()?;
        self.receive_decoder(clock)?;
        let settled_before = self.scheduler.metrics().settled_seeks;
        let occluded_before = self.scheduler.metrics().presentation_occluded_frames;
        let backpressured_before = self.scheduler.metrics().presentation_backpressured_frames;
        self.tick_scheduler(clock, publisher, now)?;
        if self.scheduler.metrics().settled_seeks > settled_before {
            return Ok(ActionProgress::Complete(WorkerCompletion::Published {
                position: self.settled_position,
            }));
        }
        if self.scheduler.metrics().presentation_occluded_frames > occluded_before {
            if !self
                .scheduler
                .settle_seek_after_occlusion(token, sample_index, now)
            {
                return Err(retry_media(
                    BackendComponent::FramePublisher,
                    "occluded publication did not match the pending seek target",
                ));
            }
            return Ok(ActionProgress::Complete(WorkerCompletion::Published {
                position: self.settled_position,
            }));
        }
        if self.scheduler.metrics().presentation_backpressured_frames > backpressured_before {
            return Err(retry_media(
                BackendComponent::FramePublisher,
                "frame publication was backpressured while settling a seek",
            ));
        }
        if self.next_video_sample > sample_index && self.scheduler.pending_frames() == 0 {
            self.receive_decoder(clock)?;
        }
        Ok(ActionProgress::Pending)
    }

    fn set_transport(&mut self, playing: bool, token: PipelineToken) -> Result<(), BackendError> {
        if playing {
            if self.started {
                return Ok(());
            }
            self.prime_audio(token)?;
            let raw = self.renderer.raw_clock()?;
            self.clock
                .resume(raw)
                .map_err(|error| clock_backend_error("resume audio clock", error))?;
            self.renderer.start(token)?;
            self.started = true;
        } else if self.started {
            self.renderer.pause(token)?;
            let raw = self.renderer.raw_clock()?;
            self.clock
                .pause(raw)
                .map_err(|error| clock_backend_error("pause audio clock", error))?;
            self.started = false;
        }
        self.backend_token = token;
        Ok(())
    }

    fn prime_audio(&mut self, token: PipelineToken) -> Result<(), BackendError> {
        for _ in 0..MAX_SESSION_PUMP_STEPS {
            let position = self.sample_clock()?;
            let writable = self.renderer.writable_frames()?;
            if writable == 0 || position >= self.video_end {
                return Ok(());
            }
            self.service_audio(position, token)?;
        }
        Err(retry_media(
            BackendComponent::AudioRenderer,
            "audio endpoint did not fill within the bounded prime loop",
        ))
    }

    fn service_playback<P>(
        &mut self,
        publisher: &mut P,
        now: MonotonicTime100ns,
    ) -> Result<TimelinePosition, BackendError>
    where
        P: FramePublisher<D3D11VideoSurface>,
    {
        let before = self.sample_clock()?;
        self.service_audio(before, self.backend_token)?;
        self.pump_steady_video(before)?;
        let pending_sample = self.scheduler.pending_sample_index();
        self.tick_scheduler(before, publisher, now)?;
        if pending_sample == self.video_sample_count.checked_sub(1)
            && self.scheduler.pending_frames() == 0
        {
            self.eos.mark_final_frame_consumed();
        }
        self.sample_clock()
    }

    fn service_audio(
        &mut self,
        clock: TimelinePosition,
        token: PipelineToken,
    ) -> Result<(), BackendError> {
        let writable = self.renderer.writable_frames()?;
        if writable == 0 || clock >= self.video_end {
            return Ok(());
        }
        let target = writable.min(MAX_AUDIO_WRITE_FRAMES);
        self.decode_audio_until(target)?;
        let queued = self.audio_mixer.queued_frames();
        let availability = if queued != 0 {
            AudioAvailability::Pcm { frames: queued }
        } else if self.audio_tracks.is_empty() {
            AudioAvailability::NoTracks
        } else if self.audio_finished {
            AudioAvailability::Ended
        } else {
            return Err(retry_media(
                BackendComponent::AudioRenderer,
                "Opus decoder made no progress before endpoint fill",
            ));
        };
        let plan = plan_audio_fill(clock, self.video_end, writable, availability);
        let frames = plan.pcm_frames.max(plan.silence_frames);
        if frames == 0 {
            return Ok(());
        }
        let samples = frames
            .checked_mul(2)
            .ok_or_else(|| corrupt_media(BackendComponent::AudioRenderer, "PCM size overflow"))?;
        if plan.pcm_frames != 0 {
            let drained = self
                .audio_mixer
                .drain_into(&mut self.audio_output[..samples])
                .map_err(|error| corrupt_media(BackendComponent::AudioRenderer, error))?;
            if drained != plan.pcm_frames {
                return Err(retry_media(
                    BackendComponent::AudioRenderer,
                    "audio mixer returned fewer frames than planned",
                ));
            }
        } else {
            self.audio_output[..samples].fill(0.0);
        }
        let written = self
            .renderer
            .write_stereo_frames(&self.audio_output[..samples], token)?;
        if written != frames {
            return Err(retry_media(
                BackendComponent::AudioRenderer,
                "WASAPI accepted fewer frames than its padding report",
            ));
        }
        Ok(())
    }

    fn decode_audio_until(&mut self, target_frames: usize) -> Result<(), BackendError> {
        while self.audio_mixer.queued_frames() < target_frames && !self.audio_finished {
            if self.audio_tracks.is_empty() || self.next_audio_sample >= self.audio_sample_count {
                self.audio_finished = true;
                break;
            }
            if self.audio_mixer.queued_frames()
                > MAX_AUDIO_QUEUE_FRAMES.saturating_sub(MAX_OPUS_FRAME_SAMPLES)
            {
                break;
            }

            self.audio_bank.clear_pending_frames();
            let mut audible_start = None;
            let mut frames = None;
            for &track_index in &self.audio_tracks {
                let packet = self
                    .audio_reader
                    .read_packet(
                        track_index,
                        self.next_audio_sample,
                        self.backend_token.work(),
                    )
                    .map_err(|error| corrupt_media(BackendComponent::AudioRenderer, error))?;
                if audible_start.is_some_and(|expected| expected != packet.audible_start_tick) {
                    return Err(corrupt_media(
                        BackendComponent::AudioRenderer,
                        "selected Opus packets are not timeline-aligned",
                    ));
                }
                audible_start.get_or_insert(packet.audible_start_tick);
                let indexed_frames = packet.indexed_duration_frames;
                let decoded = self.audio_bank.decode_indexed(
                    track_index,
                    packet.bytes,
                    indexed_frames,
                    self.backend_token.work(),
                );
                match decoded {
                    Ok(_) => {
                        let decoded_frames =
                            self.audio_bank
                                .pending_frames(track_index)
                                .map_err(|error| {
                                    corrupt_media(BackendComponent::AudioRenderer, error)
                                })?;
                        if frames.is_some_and(|expected| expected != decoded_frames) {
                            return Err(corrupt_media(
                                BackendComponent::AudioRenderer,
                                "selected Opus packets decoded unequal frame counts",
                            ));
                        }
                        frames.get_or_insert(decoded_frames);
                    }
                    Err(crate::AudioError::Decode { .. })
                    | Err(crate::AudioError::DecodedFrameTooLarge { .. })
                    | Err(crate::AudioError::DecodedFrameShorterThanIndex { .. })
                    | Err(crate::AudioError::DecodedChannelMismatch { .. }) => {}
                    Err(error) => {
                        return Err(corrupt_media(BackendComponent::AudioRenderer, error));
                    }
                }
            }

            let frames = frames.unwrap_or(0);
            let audible_start = audible_start.unwrap_or(self.audio_playback_start);
            if frames != 0 {
                self.audio_bank
                    .mix_pending_into(
                        &self.audio_tracks,
                        frames,
                        &mut self.audio_mix_scratch[..frames * 2],
                    )
                    .map_err(|error| corrupt_media(BackendComponent::AudioRenderer, error))?;
                let skipped =
                    usize::try_from(self.audio_playback_start.saturating_sub(audible_start))
                        .unwrap_or(usize::MAX)
                        .min(frames);
                let kept_start = audible_start.saturating_add(skipped as u64);
                let remaining = self.video_end.ticks().saturating_sub(kept_start);
                let kept = (frames - skipped).min(usize::try_from(remaining).unwrap_or(usize::MAX));
                if kept != 0 {
                    self.audio_mixer
                        .mix_at(
                            kept_start,
                            kept,
                            &[Some(
                                &self.audio_mix_scratch[skipped * 2..(skipped + kept) * 2],
                            )],
                        )
                        .map_err(|error| corrupt_media(BackendComponent::AudioRenderer, error))?;
                }
            }
            self.next_audio_sample = self.next_audio_sample.checked_add(1).ok_or_else(|| {
                corrupt_media(BackendComponent::AudioRenderer, "audio cursor overflow")
            })?;
        }
        Ok(())
    }

    fn pump_steady_video(&mut self, clock: TimelinePosition) -> Result<(), BackendError> {
        if self.scheduler.pending_frames() != 0 {
            return Ok(());
        }
        for _ in 0..MAX_SESSION_PUMP_STEPS {
            self.receive_decoder(clock)?;
            if self.scheduler.pending_frames() != 0 {
                return Ok(());
            }
            if self.next_video_sample < self.video_sample_count {
                let sample_index = self.next_video_sample;
                let (status, parameter_sets) = {
                    let unit = self
                        .video
                        .read_sample(sample_index, self.backend_token.work())
                        .map_err(|error| corrupt_media(BackendComponent::VideoDecoder, error))?;
                    let status = self.decoder.submit(
                        EncodedVideoPacket {
                            bytes: unit.bytes,
                            sample_index: unit.sample_index,
                            pts: timeline_position(unit.pts, self.video_timescale)?,
                            duration: timeline_duration(unit.duration, self.video_timescale)?,
                            is_sync: unit.is_sync,
                        },
                        self.backend_token,
                    )?;
                    (status, unit.parameter_set_submission)
                };
                if status == SubmitStatus::Accepted {
                    if let Some(submission) = parameter_sets {
                        if !self.video.commit_parameter_sets(submission) {
                            return Err(retry_media(
                                BackendComponent::VideoDecoder,
                                "decoder accepted stale H.264 parameter-set work",
                            ));
                        }
                    }
                    self.next_video_sample = sample_index.checked_add(1).ok_or_else(|| {
                        corrupt_media(BackendComponent::VideoDecoder, "video cursor overflow")
                    })?;
                }
            } else if !self.video_drain_sent {
                self.decoder.drain(self.backend_token)?;
                self.video_drain_sent = true;
            } else {
                return Ok(());
            }
        }
        Ok(())
    }

    fn receive_decoder(&mut self, clock: TimelinePosition) -> Result<(), BackendError> {
        for _ in 0..MAX_SESSION_PUMP_STEPS {
            let Some(frame) = self.decoder.receive()? else {
                return Ok(());
            };
            match self.scheduler.admit(frame, clock) {
                Ok(AdmitOutcome::Preroll | AdmitOutcome::Stale) => {}
                Ok(_) => return Ok(()),
                Err(frame) => {
                    drop(frame);
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    fn tick_scheduler<P>(
        &mut self,
        before: TimelinePosition,
        publisher: &mut P,
        now: MonotonicTime100ns,
    ) -> Result<bool, BackendError>
    where
        P: FramePublisher<D3D11VideoSurface>,
    {
        let renderer = &mut self.renderer;
        let clock = &mut self.clock;
        let scheduler = &mut self.scheduler;
        let mut backend_error = None;
        let tick = scheduler.tick(
            before,
            publisher,
            || match renderer.raw_clock() {
                Ok(raw) => clock.sample(raw),
                Err(error) => {
                    backend_error = Some(error);
                    Err(ClockError::TimelineOverflow)
                }
            },
            now,
        );
        if let Some(error) = backend_error {
            return Err(error);
        }
        tick.map_err(|error| match error {
            crate::SchedulerError::Backend(error) => error,
            crate::SchedulerError::Clock(error) => {
                clock_backend_error("sample post-publication audio clock", error)
            }
        })
    }

    fn sample_clock(&mut self) -> Result<TimelinePosition, BackendError> {
        let raw = self.renderer.raw_clock()?;
        self.clock
            .sample(raw)
            .map_err(|error| clock_backend_error("sample audio clock", error))
    }
}
