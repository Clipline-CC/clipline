//! Screen-capture engine, marker sources, and audio-source builders.
use super::*;

pub(super) trait TimedFrameSource {
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
pub(super) enum LiveBackend {
    Wgc(WgcCapture),
    Dxgi(DxgiDuplicationCapture),
}

impl LiveBackend {
    pub(super) fn diagnostic_label(&self) -> &'static str {
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
pub(super) struct CadencedCapture<C> {
    pub(super) inner: C,
    pub(super) frame_interval: Duration,
    frame_interval_s: f64,
    pub(super) last_data: Option<FrameData>,
    last_emit_pts_s: Option<f64>,
    next_pts_s: Option<f64>,
    last_emit_wall: Instant,
    retry_deadline: Option<Instant>,
}

impl<C> CadencedCapture<C> {
    pub(super) fn new(inner: C, fps: u32, seed: Frame) -> Self {
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

pub(super) type LiveCapture = CadencedCapture<LiveBackend>;
pub(super) type LiveRecorder = Recorder<LiveCapture, Box<dyn Encoder>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MarkerSourceKind {
    Plugin,
    LegacyLeaguePoller,
}

#[derive(Default)]
pub(super) struct PlayerSummaryState {
    in_match: bool,
    active_replay: Option<PlayerSummary>,
    full_session: Option<PlayerSummary>,
}

impl PlayerSummaryState {
    pub(super) fn match_started(&mut self) {
        self.in_match = true;
        self.active_replay = None;
        self.full_session = None;
    }

    pub(super) fn update(&mut self, summary: PlayerSummary) {
        if self.in_match {
            self.active_replay = Some(summary.clone());
        }
        if self.in_match || self.full_session.is_some() {
            self.full_session = Some(summary);
        }
    }

    pub(super) fn match_ended(&mut self) {
        self.in_match = false;
        self.active_replay = None;
    }

    pub(super) fn active_replay_summary(&self) -> Option<&PlayerSummary> {
        self.active_replay.as_ref()
    }

    pub(super) fn full_session_summary(&self) -> Option<&PlayerSummary> {
        self.active_replay.as_ref().or(self.full_session.as_ref())
    }
}

pub(super) fn marker_source_kind(opts: &ServiceOptions) -> MarkerSourceKind {
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

pub(super) fn spawn_marker_source(opts: &ServiceOptions, recording_t0: Instant) -> Receiver<PollerMsg> {
    let league_game = opts.active_game.as_ref().filter(|game| {
        game.identity.plugin_id() == Some(crate::game_plugins::LEAGUE_OF_LEGENDS_ID)
    });
    let context = crate::game_plugins::GameEventSourceContext {
        lol_url: opts.lol_url.clone(),
        recording_t0,
        league_game_executable: league_game.and_then(|game| game.exe_path.clone()),
        league_process_id: league_game.and_then(|game| game.process_id),
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
            context.league_process_id,
        ),
    }
}

/// First-frame wait: the frame that fixes the capture size. Matches WGC's own
/// 5 s budget; an idle desktop can legitimately take this long to update.
pub(super) const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(5);

/// Build the capture engine and pull its first frame (which fixes the capture
/// size). DXGI Desktop Duplication is attempted only when the user explicitly
/// selected it for a display/region source; any DXGI failure at construction
/// or on the first frame is logged as a diagnostic and silently falls back to
/// WGC, so recording always starts (the user chose silent fallback over a
/// warning).
pub(super) fn open_screen_capture(
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

pub(super) fn audio_sources_from_options(
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

pub(super) fn add_output_audio_sources(
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

pub(super) fn split_output_process_candidates(
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

pub(super) fn add_mixed_output_audio_source(
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

pub(super) fn audio_track(id: &str, track_index: u32, label: &str, kind: &str) -> ClipAudioTrack {
    ClipAudioTrack {
        id: id.to_string(),
        track_index,
        label: label.to_string(),
        kind: Some(kind.to_string()),
    }
}
