use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use clipline_library::cache::CloudMediaLease;
use clipline_library::ValidatedClipPath;
use clipline_playback::windows::{
    session_channel, D3D11PublisherTelemetry, D3D11VideoSurface, SessionClient, SessionExit,
    SessionTelemetry, SessionUpdatePayload, WindowsD3D11Publisher,
};
use clipline_playback::{
    BackendError, DecodedVideoFrame, FramePublisher, PipelineToken, PlaybackCommand, PlaybackEvent,
    PlaybackPhase, PlaybackTime, PublicationReceipt, PLAYBACK_TIMELINE_HZ,
};
use clipline_shell::{ShellCommand, ShellCommandSender};

use crate::controller::{
    ApplyUpdateOutcome, ControllerUpdate, ControllerUpdatePayload, PlaybackCommandPort,
    PlaybackController, UiPlaybackState,
};
use crate::cpu_frame::{CpuDiagnosticPublisher, CpuFrameTelemetry};
use crate::model::format_clock;
use crate::options::{write_marker, SpikeScenario};
use crate::CliplineSpike;

const UPDATE_POLL: Duration = Duration::from_millis(2);
const SCRUB_INTERVAL: Duration = Duration::from_millis(75);

/// The live adapter keeps fewer pending media resources than the playback
/// command inbox. This independent ownership bound prevents a burst of rows
/// from retaining arbitrary filesystem/cache leases while native open work is
/// still draining.
pub const MAX_PENDING_LIVE_MEDIA_OPENS: usize = 8;

/// Exact catalog request identity for a dynamic media open.
///
/// Tokens are monotonic within one `LiveSession`. A late completion that tries
/// to open with an older/equal token is rejected and its incoming lease is
/// dropped immediately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LiveMediaRequestToken(NonZeroU64);

impl LiveMediaRequestToken {
    pub fn new(value: u64) -> Result<Self, LiveMediaCommandError> {
        NonZeroU64::new(value)
            .map(Self)
            .ok_or(LiveMediaCommandError::ZeroRequestToken)
    }
}

/// Type-erased ownership transferred with an accepted native media open.
///
/// The player never inspects the lease. Dropping it is the release operation,
/// which lets local active-file guards and cached-cloud protection share the
/// same bounded handoff without exposing either implementation to Slint.
pub struct LiveMediaLease(Option<Box<dyn Send + 'static>>);

impl LiveMediaLease {
    #[must_use]
    pub fn new<T>(lease: T) -> Self
    where
        T: Send + 'static,
    {
        Self(Some(Box::new(lease)))
    }
}

impl fmt::Debug for LiveMediaLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LiveMediaLease")
            .field("retained", &self.0.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveMediaSourceKind {
    Local,
    CachedCloud,
}

/// A source that crossed the Library or Cloud validation boundary before it
/// reached the native player. Construction intentionally requires either a
/// `ValidatedClipPath` or a cache-owned `CloudMediaLease`.
#[derive(Debug)]
pub struct ValidatedLiveMediaSource {
    path: PathBuf,
    kind: LiveMediaSourceKind,
    _lease: LiveMediaLease,
}

impl ValidatedLiveMediaSource {
    #[must_use]
    pub fn local(source: ValidatedClipPath, lease: LiveMediaLease) -> Self {
        Self {
            path: source.canonical_path().to_path_buf(),
            kind: LiveMediaSourceKind::Local,
            _lease: lease,
        }
    }

    #[must_use]
    pub fn cached_cloud(lease: CloudMediaLease) -> Self {
        let path = lease.path().to_path_buf();
        Self {
            path,
            kind: LiveMediaSourceKind::CachedCloud,
            _lease: LiveMediaLease::new(lease),
        }
    }

    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    #[must_use]
    pub const fn kind(&self) -> LiveMediaSourceKind {
        self.kind
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveMediaOpenOutcome {
    Accepted { playback_open_generation: u64 },
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveMediaCommandError {
    ZeroRequestToken,
    Closing,
    PendingOpenFull { capacity: usize },
    PlaybackGenerationExhausted,
    Session(String),
    OwnershipUnavailable,
}

impl fmt::Display for LiveMediaCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroRequestToken => {
                formatter.write_str("live media request token cannot be zero")
            }
            Self::Closing => formatter.write_str("live media session is closing"),
            Self::PendingOpenFull { capacity } => write!(
                formatter,
                "live media pending-open queue is full (capacity {capacity})"
            ),
            Self::PlaybackGenerationExhausted => {
                formatter.write_str("live media playback generation exhausted")
            }
            Self::Session(message) => write!(formatter, "native playback session: {message}"),
            Self::OwnershipUnavailable => {
                formatter.write_str("live media ownership state is unavailable")
            }
        }
    }
}

impl std::error::Error for LiveMediaCommandError {}

struct PendingMediaOpen {
    _request: Option<LiveMediaRequestToken>,
    source: Option<ValidatedLiveMediaSource>,
}

#[derive(Default)]
struct LiveMediaOwnership {
    latest_request: Option<LiveMediaRequestToken>,
    next_playback_open_generation: u64,
    pending: BTreeMap<u64, PendingMediaOpen>,
    last_completed_generation: u64,
    active_generation: Option<u64>,
    active: Option<ValidatedLiveMediaSource>,
    closing: bool,
}

/// Bounded dynamic Open/Replace/Close port for one native playback runtime.
///
/// The update pump must call `accept_session_update` before forwarding an
/// update to the UI controller. A ready replacement snapshot is the proof that
/// native `IndexOpen` already released the previous file/decoder/audio stack;
/// this port then releases the previous source lease before readiness can be
/// published to Slint.
#[derive(Clone)]
pub struct LiveMediaCommandPort {
    client: Arc<SessionClient>,
    ownership: Arc<Mutex<LiveMediaOwnership>>,
}

impl LiveMediaCommandPort {
    #[must_use]
    pub fn new_dynamic(client: Arc<SessionClient>) -> Self {
        Self {
            client,
            ownership: Arc::new(Mutex::new(LiveMediaOwnership::default())),
        }
    }

    pub fn open(
        &self,
        request: LiveMediaRequestToken,
        source: ValidatedLiveMediaSource,
    ) -> Result<LiveMediaOpenOutcome, LiveMediaCommandError> {
        let mut ownership = self
            .ownership
            .lock()
            .map_err(|_| LiveMediaCommandError::OwnershipUnavailable)?;
        if ownership.closing {
            return Err(LiveMediaCommandError::Closing);
        }
        if ownership
            .latest_request
            .is_some_and(|latest| request <= latest)
        {
            return Ok(LiveMediaOpenOutcome::Stale);
        }
        if ownership.pending.len() >= MAX_PENDING_LIVE_MEDIA_OPENS {
            return Err(LiveMediaCommandError::PendingOpenFull {
                capacity: MAX_PENDING_LIVE_MEDIA_OPENS,
            });
        }
        let generation = ownership
            .next_playback_open_generation
            .checked_add(1)
            .ok_or(LiveMediaCommandError::PlaybackGenerationExhausted)?;
        self.client
            .try_send(PlaybackCommand::Open {
                path: source.path.clone(),
            })
            .map_err(|error| LiveMediaCommandError::Session(error.to_string()))?;
        ownership.next_playback_open_generation = generation;
        ownership.latest_request = Some(request);
        ownership.pending.insert(
            generation,
            PendingMediaOpen {
                _request: Some(request),
                source: Some(source),
            },
        );
        Ok(LiveMediaOpenOutcome::Accepted {
            playback_open_generation: generation,
        })
    }

    /// Sends the command-line fixture through the same bounded runtime without
    /// manufacturing a production source lease. This is deliberately private:
    /// dynamic product opens must use `open` and a validated source.
    fn open_harness_fixture(&self, path: PathBuf) -> Result<(), LiveMediaCommandError> {
        let mut ownership = self
            .ownership
            .lock()
            .map_err(|_| LiveMediaCommandError::OwnershipUnavailable)?;
        let generation = ownership
            .next_playback_open_generation
            .checked_add(1)
            .ok_or(LiveMediaCommandError::PlaybackGenerationExhausted)?;
        self.client
            .try_send(PlaybackCommand::Open { path })
            .map_err(|error| LiveMediaCommandError::Session(error.to_string()))?;
        ownership.next_playback_open_generation = generation;
        ownership.pending.insert(
            generation,
            PendingMediaOpen {
                _request: None,
                source: None,
            },
        );
        Ok(())
    }

    /// Returns `false` for an already accepted close. Playback `Close` is a
    /// terminal runtime fence, so its caller must subsequently shut down/drop
    /// this `LiveSession`; a later product Open starts a fresh dynamic session.
    /// Once accepted, new opens fail closed and all retained leases are
    /// released by the terminal snapshot or, as a backstop, after the playback
    /// thread joins.
    pub fn close(&self) -> Result<bool, LiveMediaCommandError> {
        let mut ownership = self
            .ownership
            .lock()
            .map_err(|_| LiveMediaCommandError::OwnershipUnavailable)?;
        if ownership.closing {
            return Ok(false);
        }
        let generation = ownership
            .next_playback_open_generation
            .checked_add(1)
            .ok_or(LiveMediaCommandError::PlaybackGenerationExhausted)?;
        self.client
            .try_send(PlaybackCommand::Close)
            .map_err(|error| LiveMediaCommandError::Session(error.to_string()))?;
        ownership.next_playback_open_generation = generation;
        ownership.closing = true;
        Ok(true)
    }

    /// Request an orderly terminal close, or disconnect the runtime when the
    /// bounded inbox cannot accept that fence. The fallback is deliberately
    /// infallible so UI-thread teardown can always proceed to `join`.
    #[must_use]
    pub fn close_or_disconnect(&self) -> bool {
        match self.close() {
            Ok(_) => true,
            Err(_) => {
                self.disconnect_runtime();
                false
            }
        }
    }

    /// Consumes playback ownership transitions before their matching state can
    /// be scheduled on the Slint event loop.
    pub fn accept_session_update(&self, update: &clipline_playback::windows::SessionUpdate) {
        let clipline_playback::windows::SessionUpdatePayload::Snapshot(snapshot) = &update.payload
        else {
            return;
        };
        let mut released = Vec::new();
        if let Ok(mut ownership) = self.ownership.lock() {
            let open = snapshot.generation.open;
            match snapshot.phase {
                PlaybackPhase::Paused | PlaybackPhase::Playing | PlaybackPhase::Ended => {
                    if open <= ownership.last_completed_generation {
                        return;
                    }
                    if ownership
                        .active_generation
                        .is_some_and(|active| open <= active)
                    {
                        return;
                    }
                    if let Some(active) = ownership.active.take() {
                        released.push(active);
                    }
                    let completed = ownership
                        .pending
                        .range(..=open)
                        .map(|(key, _)| *key)
                        .collect::<Vec<_>>();
                    let mut accepted = None;
                    for generation in completed {
                        if let Some(mut pending) = ownership.pending.remove(&generation) {
                            if generation == open {
                                accepted = pending.source.take();
                            }
                            if let Some(source) = pending.source.take() {
                                released.push(source);
                            }
                        }
                    }
                    ownership.last_completed_generation = open;
                    ownership.active_generation = Some(open);
                    ownership.active = accepted;
                }
                PlaybackPhase::Failed => {
                    if open < ownership.last_completed_generation {
                        return;
                    }
                    ownership.last_completed_generation = open;
                    if ownership
                        .active_generation
                        .is_none_or(|active| open >= active)
                    {
                        if let Some(active) = ownership.active.take() {
                            released.push(active);
                        }
                        ownership.active_generation = None;
                    }
                    release_pending_through(&mut ownership, open, &mut released);
                }
                PlaybackPhase::Closed => {
                    if open < ownership.last_completed_generation {
                        return;
                    }
                    ownership.last_completed_generation = open;
                    if ownership
                        .active_generation
                        .is_some_and(|active| open >= active)
                    {
                        if let Some(active) = ownership.active.take() {
                            released.push(active);
                        }
                        ownership.active_generation = None;
                    }
                    // The runtime publishes its initial Closed generation
                    // before it can consume a concurrently queued first Open.
                    // Release only resources owned by this exact-or-older
                    // generation; future pending opens remain protected.
                    release_pending_through(&mut ownership, open, &mut released);
                }
                PlaybackPhase::Opening | PlaybackPhase::Seeking => {}
            }
        }
        drop(released);
    }

    /// Backstop used only after the native runtime has returned and therefore
    /// cannot retain a file, decoder, or audio endpoint.
    pub fn release_all_after_backend_shutdown(&self) {
        let mut released = Vec::new();
        if let Ok(mut ownership) = self.ownership.lock() {
            ownership.closing = true;
            release_all_media(&mut ownership, &mut released);
        }
        drop(released);
    }

    /// Fail-safe terminal fence used only when the bounded command inbox
    /// cannot accept `Close`. Disconnecting wakes the runtime and makes it
    /// release native media before `LiveSession` releases source leases.
    fn disconnect_runtime(&self) {
        if let Ok(mut ownership) = self.ownership.lock() {
            ownership.closing = true;
        }
        self.client.disconnect();
    }
}

fn release_pending_through(
    ownership: &mut LiveMediaOwnership,
    generation: u64,
    released: &mut Vec<ValidatedLiveMediaSource>,
) {
    let completed = ownership
        .pending
        .range(..=generation)
        .map(|(key, _)| *key)
        .collect::<Vec<_>>();
    for key in completed {
        if let Some(mut pending) = ownership.pending.remove(&key) {
            if let Some(source) = pending.source.take() {
                released.push(source);
            }
        }
    }
}

fn release_all_media(
    ownership: &mut LiveMediaOwnership,
    released: &mut Vec<ValidatedLiveMediaSource>,
) {
    if let Some(active) = ownership.active.take() {
        released.push(active);
    }
    ownership.active_generation = None;
    let pending = std::mem::take(&mut ownership.pending);
    released.extend(pending.into_values().filter_map(|pending| pending.source));
}

#[derive(Clone)]
pub struct SessionCommandPort {
    client: Arc<SessionClient>,
}

impl SessionCommandPort {
    #[must_use]
    pub fn new(client: Arc<SessionClient>) -> Self {
        Self { client }
    }
}

impl PlaybackCommandPort for SessionCommandPort {
    fn send(&self, command: PlaybackCommand) -> Result<(), String> {
        if matches!(
            command,
            PlaybackCommand::Open { .. } | PlaybackCommand::Close
        ) {
            return Err(
                "Open and Close are reserved for the lease-owning live media command port"
                    .to_owned(),
            );
        }
        self.client
            .try_send(command)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

pub enum SpikePublisher {
    D3d(WindowsD3D11Publisher),
    Cpu(CpuDiagnosticPublisher),
}

impl FramePublisher<D3D11VideoSurface> for SpikePublisher {
    fn publish(
        &mut self,
        frame: DecodedVideoFrame<D3D11VideoSurface>,
    ) -> Result<PublicationReceipt, BackendError> {
        match self {
            Self::D3d(publisher) => publisher.publish(frame),
            Self::Cpu(publisher) => publisher.publish(frame),
        }
    }

    fn clear(&mut self, token: PipelineToken) -> Result<(), BackendError> {
        match self {
            Self::D3d(publisher) => publisher.clear(token),
            Self::Cpu(publisher) => publisher.clear(token),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentationTelemetry {
    D3d(D3D11PublisherTelemetry),
    Cpu {
        frames: CpuFrameTelemetry,
        readback_frames: u64,
        max_copy_time_100ns: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSessionReport {
    pub exit: SessionExit,
    pub telemetry: Option<SessionTelemetry>,
    pub presentation: PresentationTelemetry,
}

pub struct LiveSession {
    controller: Arc<Mutex<PlaybackController<SessionCommandPort>>>,
    media_commands: LiveMediaCommandPort,
    drain_updates: Arc<AtomicBool>,
    stop_updates: Arc<AtomicBool>,
    playback: Option<
        JoinHandle<
            Result<
                clipline_playback::windows::SessionReport<SpikePublisher>,
                clipline_playback::windows::SessionRunError,
            >,
        >,
    >,
    updates: Option<JoinHandle<()>>,
}

impl LiveSession {
    /// Starts the command-line fixture harness used by the measurement
    /// scenarios. Product Library/Cloud opens use `start_dynamic` followed by
    /// the returned `media_commands` port.
    pub fn start(
        publisher: SpikePublisher,
        window: slint::Weak<CliplineSpike>,
        fixture: PathBuf,
        scenario: SpikeScenario,
        marker_path: Option<PathBuf>,
        exit_after_ready: bool,
        shell_commands: ShellCommandSender,
    ) -> Result<Self, String> {
        Self::start_inner(
            publisher,
            window,
            Some(fixture),
            scenario,
            marker_path,
            exit_after_ready,
            shell_commands,
        )
    }

    pub fn start_dynamic(
        publisher: SpikePublisher,
        window: slint::Weak<CliplineSpike>,
        scenario: SpikeScenario,
        marker_path: Option<PathBuf>,
        exit_after_ready: bool,
        shell_commands: ShellCommandSender,
    ) -> Result<Self, String> {
        Self::start_inner(
            publisher,
            window,
            None,
            scenario,
            marker_path,
            exit_after_ready,
            shell_commands,
        )
    }

    fn start_inner(
        publisher: SpikePublisher,
        window: slint::Weak<CliplineSpike>,
        fixture: Option<PathBuf>,
        scenario: SpikeScenario,
        marker_path: Option<PathBuf>,
        exit_after_ready: bool,
        shell_commands: ShellCommandSender,
    ) -> Result<Self, String> {
        let (client, runtime) = session_channel();
        let client = Arc::new(client);
        let media_commands = LiveMediaCommandPort::new_dynamic(Arc::clone(&client));
        let port = SessionCommandPort::new(Arc::clone(&client));
        let controller = Arc::new(Mutex::new(PlaybackController::new(port)));
        let playback = thread::Builder::new()
            .name("clipline-slint-playback".to_owned())
            .spawn(move || runtime.run(publisher))
            .map_err(|error| format!("spawn native playback thread: {error}"))?;
        if let Some(fixture) = fixture {
            media_commands
                .open_harness_fixture(fixture)
                .map_err(|error| format!("open native playback fixture: {error}"))?;
        }
        let drain_updates = Arc::new(AtomicBool::new(false));
        let stop_updates = Arc::new(AtomicBool::new(false));
        let updates = spawn_update_pump(
            Arc::clone(&client),
            Arc::clone(&controller),
            UpdatePumpConfig {
                drain_only: Arc::clone(&drain_updates),
                stop: Arc::clone(&stop_updates),
                window,
                scenario,
                marker_path,
                exit_after_ready,
                shell_commands,
                media_commands: media_commands.clone(),
            },
        )?;
        Ok(Self {
            controller,
            media_commands,
            drain_updates,
            stop_updates,
            playback: Some(playback),
            updates: Some(updates),
        })
    }

    pub fn controller(&self) -> Arc<Mutex<PlaybackController<SessionCommandPort>>> {
        Arc::clone(&self.controller)
    }

    #[must_use]
    pub fn media_commands(&self) -> LiveMediaCommandPort {
        self.media_commands.clone()
    }

    pub fn shutdown(mut self) -> Result<LiveSessionReport, String> {
        self.shutdown_inner()
    }

    fn shutdown_inner(&mut self) -> Result<LiveSessionReport, String> {
        self.drain_updates.store(true, Ordering::Release);
        let _ = self.media_commands.close_or_disconnect();
        let playback = self
            .playback
            .take()
            .ok_or_else(|| "native playback thread was already joined".to_owned())?;
        let playback_result = playback.join();
        self.stop_updates.store(true, Ordering::Release);
        let updates_result = self.updates.take().map(JoinHandle::join);
        self.media_commands.release_all_after_backend_shutdown();
        if let Some(updates_result) = updates_result {
            updates_result.map_err(|_| "Slint update pump panicked".to_owned())?;
        }
        let mut report = playback_result
            .map_err(|_| "native playback thread panicked".to_owned())?
            .map_err(|error| format!("native playback session failed: {error}"))?;
        let presentation = match &mut report.publisher {
            SpikePublisher::D3d(publisher) => {
                let telemetry = publisher.telemetry();
                publisher.close();
                PresentationTelemetry::D3d(telemetry)
            }
            SpikePublisher::Cpu(publisher) => {
                let readback = publisher.readback_telemetry();
                PresentationTelemetry::Cpu {
                    frames: publisher.frame_telemetry(),
                    readback_frames: readback.frames_read,
                    max_copy_time_100ns: readback.max_copy_time_100ns,
                }
            }
        };
        Ok(LiveSessionReport {
            exit: report.exit,
            telemetry: report.telemetry,
            presentation,
        })
    }
}

impl Drop for LiveSession {
    fn drop(&mut self) {
        if self.playback.is_some() || self.updates.is_some() {
            let _ = self.shutdown_inner();
        }
    }
}

struct UpdatePumpConfig {
    drain_only: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    window: slint::Weak<CliplineSpike>,
    scenario: SpikeScenario,
    marker_path: Option<PathBuf>,
    exit_after_ready: bool,
    shell_commands: ShellCommandSender,
    media_commands: LiveMediaCommandPort,
}

fn spawn_update_pump(
    client: Arc<SessionClient>,
    controller: Arc<Mutex<PlaybackController<SessionCommandPort>>>,
    config: UpdatePumpConfig,
) -> Result<JoinHandle<()>, String> {
    let UpdatePumpConfig {
        drain_only,
        stop,
        window,
        scenario,
        marker_path,
        exit_after_ready,
        shell_commands,
        media_commands,
    } = config;
    thread::Builder::new()
        .name("clipline-slint-updates".to_owned())
        .spawn(move || {
            let latest_ui_sequence = Arc::new(AtomicU64::new(0));
            let mut opened = false;
            let mut playing_requested = false;
            let mut settled_seeks = 0_u64;
            let mut next_scrub = Instant::now();
            let mut scrub_near_end = false;
            let mut marker_written = false;
            while !stop.load(Ordering::Acquire) {
                let mut progressed = false;
                while let Some(update) = client.try_recv_update() {
                    progressed = true;
                    // The ownership port observes a native-ready replacement
                    // before the same snapshot can reach the UI controller.
                    media_commands.accept_session_update(&update);
                    let sequence = update.sequence;
                    let event = match &update.payload {
                        SessionUpdatePayload::Event(event) => Some(event.clone()),
                        _ => None,
                    };
                    let payload = match update.payload {
                        SessionUpdatePayload::Snapshot(snapshot) => {
                            ControllerUpdatePayload::Snapshot(snapshot)
                        }
                        SessionUpdatePayload::Event(event) => ControllerUpdatePayload::Event(event),
                        SessionUpdatePayload::Metrics(_) => ControllerUpdatePayload::Metrics,
                    };
                    let state = if let Ok(mut controller) = controller.lock() {
                        (controller.apply_update(ControllerUpdate {
                            sequence,
                            token: update.token,
                            payload,
                        }) == ApplyUpdateOutcome::Applied)
                            .then(|| controller.ui_state().clone())
                    } else {
                        None
                    };
                    if !drain_only.load(Ordering::Acquire) {
                        if let Some(state) = state {
                            latest_ui_sequence.store(sequence, Ordering::Release);
                            let gate = Arc::clone(&latest_ui_sequence);
                            let weak = window.clone();
                            if slint::invoke_from_event_loop(move || {
                                if gate.load(Ordering::Acquire) == sequence {
                                    if let Some(window) = weak.upgrade() {
                                        apply_ui_state(&window, &state);
                                    }
                                }
                            })
                            .is_err()
                            {
                                stop.store(true, Ordering::Release);
                                break;
                            }
                        }
                    }
                    if !drain_only.load(Ordering::Acquire) {
                        if let Some(event) = event {
                            match event {
                                PlaybackEvent::Opened { .. } => opened = true,
                                PlaybackEvent::Closed { .. } => opened = false,
                                PlaybackEvent::SeekSettled { .. } => {
                                    settled_seeks = settled_seeks.saturating_add(1);
                                }
                                PlaybackEvent::Error { message, .. } => {
                                    if let Some(path) = marker_path.as_ref() {
                                        let _ = write_marker(path, "error", &message);
                                    }
                                    marker_written = true;
                                    if exit_after_ready {
                                        let _ = shell_commands.try_send(ShellCommand::Quit);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }

                if !drain_only.load(Ordering::Acquire) {
                    if opened
                        && !playing_requested
                        && scenario == SpikeScenario::ReviewPlaying
                        && client.try_send(PlaybackCommand::Play).is_ok()
                    {
                        playing_requested = true;
                    }
                    if opened
                        && scenario == SpikeScenario::ScrubStorm
                        && Instant::now() >= next_scrub
                    {
                        let ticks = if scrub_near_end {
                            4 * u64::from(PLAYBACK_TIMELINE_HZ)
                        } else {
                            u64::from(PLAYBACK_TIMELINE_HZ) / 2
                        };
                        let _ = client.try_send(PlaybackCommand::Seek {
                            position: PlaybackTime {
                                ticks,
                                timescale: PLAYBACK_TIMELINE_HZ,
                            },
                        });
                        scrub_near_end = !scrub_near_end;
                        next_scrub = Instant::now() + SCRUB_INTERVAL;
                    }
                }

                if !marker_written && !drain_only.load(Ordering::Acquire) {
                    let ready = match scenario {
                        SpikeScenario::Interactive | SpikeScenario::ReviewIdle => opened,
                        SpikeScenario::ReviewPlaying => {
                            controller.lock().ok().is_some_and(|controller| {
                                controller.ui_state().playing
                                    && controller.ui_state().position.ticks > 0
                            })
                        }
                        SpikeScenario::ScrubStorm => settled_seeks >= 10,
                        SpikeScenario::RevealClose100 => false,
                    };
                    if ready {
                        if let Some(path) = marker_path.as_ref() {
                            let detail = format!("{} native Slint state ready", scenario.label());
                            let _ = write_marker(path, "ready", &detail);
                        }
                        marker_written = true;
                        if exit_after_ready {
                            let _ = shell_commands.try_send(ShellCommand::Quit);
                        }
                    }
                }
                if !progressed {
                    thread::sleep(UPDATE_POLL);
                }
            }
        })
        .map_err(|error| format!("spawn Slint update pump: {error}"))
}

fn apply_ui_state(window: &CliplineSpike, state: &UiPlaybackState) {
    window.set_playing(state.playing);
    window.set_volume(state.volume);
    window.set_current_time(format_clock(to_timeline_ticks(state.position)).into());
    window.set_duration_time(
        state
            .duration
            .map(to_timeline_ticks)
            .map(format_clock)
            .unwrap_or_else(|| "--:--.---".to_owned())
            .into(),
    );
    if state.audio_track_indices.len() >= 2 {
        window.set_track_one_id(i32::try_from(state.audio_track_indices[0]).unwrap_or(i32::MAX));
        window.set_track_two_id(i32::try_from(state.audio_track_indices[1]).unwrap_or(i32::MAX));
    }
    let track_one = usize::try_from(window.get_track_one_id()).ok();
    let track_two = usize::try_from(window.get_track_two_id()).ok();
    window.set_track_one_selected(
        track_one.is_some_and(|track| state.audio_track_indices.contains(&track)),
    );
    window.set_track_two_selected(
        track_two.is_some_and(|track| state.audio_track_indices.contains(&track)),
    );
    window.set_status_text(state.status.clone().into());
}

fn to_timeline_ticks(time: PlaybackTime) -> u64 {
    let scaled = u128::from(time.ticks).saturating_mul(u128::from(PLAYBACK_TIMELINE_HZ));
    u64::try_from(scaled / u128::from(time.timescale)).unwrap_or(u64::MAX)
}
