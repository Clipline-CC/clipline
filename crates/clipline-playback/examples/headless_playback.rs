#[cfg(not(windows))]
fn main() {
    eprintln!("headless native playback is available only on Windows");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() {
    if let Err(error) = windows_app::run() {
        eprintln!("headless playback failed: {error}");
        std::process::exit(1);
    }
}

#[cfg(windows)]
mod windows_app {
    use std::error::Error;
    use std::fs::{self, OpenOptions};
    use std::io::{self, ErrorKind, Write};
    use std::path::{Path, PathBuf};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use clipline_playback::windows::{
        session_channel, D3D11VideoSurface, SessionClient, SessionExit, SessionReport,
        SessionRunError, SessionUpdatePayload, WasapiInitializationPath,
    };
    use clipline_playback::{
        DecodedVideoFrame, FramePublisher, PipelineToken, PlaybackCommand, PlaybackEvent,
        PlaybackPhase, PlaybackTime, PublicationReceipt, VideoAcceleration,
    };
    use serde::Serialize;

    type AppResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

    const LOOP_SLEEP: Duration = Duration::from_millis(1);
    const DEVICE_STALL_TIMEOUT: Duration = Duration::from_secs(10);
    const PLAYBACK_TIMEOUT_HEADROOM: Duration = Duration::from_secs(10);
    const SEEK_PREVIEW_DURATION: Duration = Duration::from_millis(180);
    const CYCLE_PREVIEW_DURATION: Duration = Duration::from_millis(60);
    const DEFAULT_CYCLES: usize = 100;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "kebab-case")]
    enum Scenario {
        Playback,
        SeekStorm,
        Cycle100,
    }

    impl Scenario {
        fn parse(value: &str) -> AppResult<Self> {
            match value {
                "playback" => Ok(Self::Playback),
                "seek-storm" => Ok(Self::SeekStorm),
                "cycle-100" => Ok(Self::Cycle100),
                _ => Err(invalid(format!(
                    "unsupported scenario {value:?}; expected playback, seek-storm, or cycle-100"
                ))),
            }
        }

        const fn label(self) -> &'static str {
            match self {
                Self::Playback => "playback",
                Self::SeekStorm => "seek-storm",
                Self::Cycle100 => "cycle-100",
            }
        }
    }

    struct Cli {
        fixture: PathBuf,
        scenario: Scenario,
        telemetry: PathBuf,
        run_seconds: Option<u64>,
    }

    impl Cli {
        fn parse() -> AppResult<Self> {
            let mut fixture = None;
            let mut scenario = None;
            let mut telemetry = None;
            let mut run_seconds = None;
            let mut args = std::env::args_os().skip(1);
            while let Some(flag) = args.next() {
                let flag = flag.to_string_lossy();
                match flag.as_ref() {
                    "--fixture" => fixture = Some(PathBuf::from(required_value(&mut args, &flag)?)),
                    "--scenario" => {
                        let value = required_value(&mut args, &flag)?;
                        scenario = Some(Scenario::parse(&value.to_string_lossy())?);
                    }
                    "--telemetry" => {
                        telemetry = Some(PathBuf::from(required_value(&mut args, &flag)?));
                    }
                    "--run-seconds" => {
                        let value = required_value(&mut args, &flag)?;
                        run_seconds =
                            Some(value.to_string_lossy().parse::<u64>().map_err(|_| {
                                invalid("--run-seconds must be an unsigned integer")
                            })?);
                    }
                    "--help" | "-h" => {
                        println!(
                            "Usage: headless_playback --fixture <mp4> --scenario \
                             <playback|seek-storm|cycle-100> --telemetry <json> \
                             [--run-seconds <seconds>]"
                        );
                        std::process::exit(0);
                    }
                    _ => return Err(invalid(format!("unknown argument {flag:?}"))),
                }
            }
            let fixture = fixture.ok_or_else(|| invalid("missing --fixture"))?;
            let scenario = scenario.ok_or_else(|| invalid("missing --scenario"))?;
            let telemetry = telemetry.ok_or_else(|| invalid("missing --telemetry"))?;
            if !fixture.is_file() {
                return Err(invalid(format!(
                    "fixture does not exist: {}",
                    fixture.display()
                )));
            }
            Ok(Self {
                fixture,
                scenario,
                telemetry,
                run_seconds,
            })
        }
    }

    fn required_value(
        args: &mut impl Iterator<Item = std::ffi::OsString>,
        flag: &str,
    ) -> AppResult<std::ffi::OsString> {
        args.next()
            .ok_or_else(|| invalid(format!("{flag} requires a value")))
    }

    #[derive(Default, Serialize)]
    #[serde(rename_all = "camelCase")]
    struct AggregateMetrics {
        process_id: u32,
        cycles: u64,
        completed_playbacks: u64,
        requested_seeks: u64,
        settled_seeks: u64,
        exact_seek_targets: u64,
        seek_target_mismatches: u64,
        decoded_frames: u64,
        decoded_eligible_frames: u64,
        presented_frames: u64,
        late_frames: u64,
        scheduler_dropped_frames: u64,
        presentation_backpressured_frames: u64,
        presentation_occluded_frames: u64,
        late_or_dropped_frames: u64,
        late_drop_ratio: f64,
        stale_results: u64,
        max_av_error_ticks: u64,
        av_error_p95_ms: Option<u16>,
        av_error_histogram_overflowed: bool,
        seek_settle_p95_ms: Option<u16>,
        audio_underruns: u64,
        cycles_with_audio_underrun: u64,
        max_audio_underruns_per_cycle: u64,
        audio_midstream_underruns: u64,
        cycles_with_midstream_underrun: u64,
        max_midstream_underruns_per_cycle: u64,
        audio_terminal_playout_episodes: u64,
        audio_underrun_frames_estimate: u64,
        audio_mixed_frames: u64,
        audio_silent_frames: u64,
        audio_corrupt_packets: u64,
        audio_endpoint_id: String,
        audio_device_format: String,
        audio_device_sample_rate: u32,
        audio_device_channels: u16,
        audio_device_bits_per_sample: u16,
        audio_device_valid_bits_per_sample: Option<u16>,
        audio_device_channel_mask: Option<u32>,
        audio_device_buffer_duration_100ns: u64,
        audio_conversion_active: bool,
        audio_initialization_path: String,
        audio_engine_period_frames: usize,
        audio_clock_frequency: u64,
        audio_recovery_count: u64,
        video_encoded_buffer_capacity: usize,
        video_converted_buffer_capacity: usize,
        video_encoded_high_water: usize,
        video_converted_high_water: usize,
        audio_packet_capacity: usize,
        audio_packet_high_water: usize,
        audio_queue_high_water_frames: usize,
        first_cycle_audio_queue_high_water_frames: Option<usize>,
        last_cycle_audio_queue_high_water_frames: Option<usize>,
        first_cycle_video_encoded_buffer_capacity: Option<usize>,
        last_cycle_video_encoded_buffer_capacity: Option<usize>,
        first_cycle_audio_packet_capacity: Option<usize>,
        last_cycle_audio_packet_capacity: Option<usize>,
        endpoint_buffer_frames: usize,
        endpoint_epoch: u64,
        decoder_acceleration: String,
        decoder_presentable_frames: u64,
        decoder_output_copies: u64,
        decoder_samples_received: u64,
        decoder_samples_released: u64,
        file_release_verified: bool,
        elapsed_ms: u64,
    }

    impl AggregateMetrics {
        fn absorb(&mut self, run: RunMetrics) {
            let is_first_cycle = self.cycles == 0;
            self.cycles = self.cycles.saturating_add(1);
            self.completed_playbacks = self
                .completed_playbacks
                .saturating_add(u64::from(run.ended));
            self.requested_seeks = self.requested_seeks.saturating_add(run.requested_seeks);
            self.settled_seeks = self.settled_seeks.saturating_add(run.settled_seeks);
            self.exact_seek_targets = self.exact_seek_targets.saturating_add(run.settled_seeks);
            self.seek_target_mismatches = self
                .seek_target_mismatches
                .saturating_add(run.requested_seeks.saturating_sub(run.settled_seeks));
            self.decoded_frames = self.decoded_frames.saturating_add(run.decoded_frames);
            self.decoded_eligible_frames = self
                .decoded_eligible_frames
                .saturating_add(run.decoded_eligible_frames);
            self.presented_frames = self.presented_frames.saturating_add(run.presented_frames);
            self.late_frames = self.late_frames.saturating_add(run.late_frames);
            self.scheduler_dropped_frames = self
                .scheduler_dropped_frames
                .saturating_add(run.scheduler_dropped_frames);
            self.presentation_backpressured_frames = self
                .presentation_backpressured_frames
                .saturating_add(run.presentation_backpressured_frames);
            self.presentation_occluded_frames = self
                .presentation_occluded_frames
                .saturating_add(run.presentation_occluded_frames);
            self.late_or_dropped_frames = self
                .late_or_dropped_frames
                .saturating_add(run.late_or_dropped_frames);
            self.stale_results = self.stale_results.saturating_add(run.stale_results);
            self.max_av_error_ticks = self.max_av_error_ticks.max(run.max_av_error_ticks);
            self.av_error_p95_ms = merge_fail_closed_percentile(
                self.av_error_p95_ms,
                run.av_error_p95_ms,
                &mut self.av_error_histogram_overflowed,
                run.av_error_histogram_overflowed,
            );
            self.seek_settle_p95_ms = max_option(self.seek_settle_p95_ms, run.seek_settle_p95_ms);
            self.audio_underruns = self.audio_underruns.saturating_add(run.audio_underruns);
            self.cycles_with_audio_underrun = self
                .cycles_with_audio_underrun
                .saturating_add(u64::from(run.audio_underruns != 0));
            self.max_audio_underruns_per_cycle =
                self.max_audio_underruns_per_cycle.max(run.audio_underruns);
            self.audio_midstream_underruns = self
                .audio_midstream_underruns
                .saturating_add(run.audio_midstream_underruns);
            self.cycles_with_midstream_underrun = self
                .cycles_with_midstream_underrun
                .saturating_add(u64::from(run.audio_midstream_underruns != 0));
            self.max_midstream_underruns_per_cycle = self
                .max_midstream_underruns_per_cycle
                .max(run.audio_midstream_underruns);
            self.audio_terminal_playout_episodes = self
                .audio_terminal_playout_episodes
                .saturating_add(run.audio_terminal_playout_episodes);
            self.audio_underrun_frames_estimate = self
                .audio_underrun_frames_estimate
                .saturating_add(run.audio_underrun_frames_estimate);
            self.audio_mixed_frames = self
                .audio_mixed_frames
                .saturating_add(run.audio_mixed_frames);
            self.audio_silent_frames = self
                .audio_silent_frames
                .saturating_add(run.audio_silent_frames);
            self.audio_corrupt_packets = self
                .audio_corrupt_packets
                .saturating_add(run.audio_corrupt_packets);
            merge_descriptor(&mut self.audio_endpoint_id, run.audio_endpoint_id);
            merge_descriptor(&mut self.audio_device_format, run.audio_device_format);
            self.audio_device_sample_rate = self
                .audio_device_sample_rate
                .max(run.audio_device_sample_rate);
            self.audio_device_channels = self.audio_device_channels.max(run.audio_device_channels);
            self.audio_device_bits_per_sample = self
                .audio_device_bits_per_sample
                .max(run.audio_device_bits_per_sample);
            self.audio_device_valid_bits_per_sample = max_option(
                self.audio_device_valid_bits_per_sample,
                run.audio_device_valid_bits_per_sample,
            );
            self.audio_device_channel_mask = max_option(
                self.audio_device_channel_mask,
                run.audio_device_channel_mask,
            );
            self.audio_device_buffer_duration_100ns = self
                .audio_device_buffer_duration_100ns
                .max(run.audio_device_buffer_duration_100ns);
            self.audio_conversion_active |= run.audio_conversion_active;
            merge_descriptor(
                &mut self.audio_initialization_path,
                run.audio_initialization_path,
            );
            self.audio_engine_period_frames = self
                .audio_engine_period_frames
                .max(run.audio_engine_period_frames);
            self.audio_clock_frequency = self.audio_clock_frequency.max(run.audio_clock_frequency);
            self.audio_recovery_count = self
                .audio_recovery_count
                .saturating_add(run.audio_recovery_count);
            self.video_encoded_buffer_capacity = self
                .video_encoded_buffer_capacity
                .max(run.video_encoded_buffer_capacity);
            self.video_converted_buffer_capacity = self
                .video_converted_buffer_capacity
                .max(run.video_converted_buffer_capacity);
            self.video_encoded_high_water = self
                .video_encoded_high_water
                .max(run.video_encoded_high_water);
            self.video_converted_high_water = self
                .video_converted_high_water
                .max(run.video_converted_high_water);
            self.audio_packet_capacity = self.audio_packet_capacity.max(run.audio_packet_capacity);
            self.audio_packet_high_water = self
                .audio_packet_high_water
                .max(run.audio_packet_high_water);
            self.audio_queue_high_water_frames = self
                .audio_queue_high_water_frames
                .max(run.audio_queue_high_water_frames);
            if is_first_cycle {
                self.first_cycle_audio_queue_high_water_frames =
                    Some(run.audio_queue_high_water_frames);
                self.first_cycle_video_encoded_buffer_capacity =
                    Some(run.video_encoded_buffer_capacity);
                self.first_cycle_audio_packet_capacity = Some(run.audio_packet_capacity);
            }
            self.last_cycle_audio_queue_high_water_frames = Some(run.audio_queue_high_water_frames);
            self.last_cycle_video_encoded_buffer_capacity = Some(run.video_encoded_buffer_capacity);
            self.last_cycle_audio_packet_capacity = Some(run.audio_packet_capacity);
            self.endpoint_buffer_frames =
                self.endpoint_buffer_frames.max(run.endpoint_buffer_frames);
            self.endpoint_epoch = self.endpoint_epoch.max(run.endpoint_epoch);
            merge_descriptor(&mut self.decoder_acceleration, run.decoder_acceleration);
            self.decoder_presentable_frames = self
                .decoder_presentable_frames
                .saturating_add(run.decoder_presentable_frames);
            self.decoder_output_copies = self
                .decoder_output_copies
                .saturating_add(run.decoder_output_copies);
            self.decoder_samples_received = self
                .decoder_samples_received
                .saturating_add(run.decoder_samples_received);
            self.decoder_samples_released = self
                .decoder_samples_released
                .saturating_add(run.decoder_samples_released);
            self.late_drop_ratio = if self.decoded_eligible_frames == 0 {
                0.0
            } else {
                self.late_or_dropped_frames as f64 / self.decoded_eligible_frames as f64
            };
        }
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct TelemetryDocument {
        schema_version: u32,
        scenario: &'static str,
        status: &'static str,
        source_fixture: String,
        metrics: AggregateMetrics,
    }

    struct RunMetrics {
        ended: bool,
        requested_seeks: u64,
        settled_seeks: u64,
        decoded_frames: u64,
        decoded_eligible_frames: u64,
        presented_frames: u64,
        late_frames: u64,
        scheduler_dropped_frames: u64,
        presentation_backpressured_frames: u64,
        presentation_occluded_frames: u64,
        late_or_dropped_frames: u64,
        stale_results: u64,
        max_av_error_ticks: u64,
        av_error_p95_ms: Option<u16>,
        av_error_histogram_overflowed: bool,
        seek_settle_p95_ms: Option<u16>,
        audio_underruns: u64,
        audio_midstream_underruns: u64,
        audio_terminal_playout_episodes: u64,
        audio_underrun_frames_estimate: u64,
        audio_mixed_frames: u64,
        audio_silent_frames: u64,
        audio_corrupt_packets: u64,
        audio_endpoint_id: String,
        audio_device_format: String,
        audio_device_sample_rate: u32,
        audio_device_channels: u16,
        audio_device_bits_per_sample: u16,
        audio_device_valid_bits_per_sample: Option<u16>,
        audio_device_channel_mask: Option<u32>,
        audio_device_buffer_duration_100ns: u64,
        audio_conversion_active: bool,
        audio_initialization_path: String,
        audio_engine_period_frames: usize,
        audio_clock_frequency: u64,
        audio_recovery_count: u64,
        video_encoded_buffer_capacity: usize,
        video_converted_buffer_capacity: usize,
        video_encoded_high_water: usize,
        video_converted_high_water: usize,
        audio_packet_capacity: usize,
        audio_packet_high_water: usize,
        audio_queue_high_water_frames: usize,
        endpoint_buffer_frames: usize,
        endpoint_epoch: u64,
        decoder_acceleration: String,
        decoder_presentable_frames: u64,
        decoder_output_copies: u64,
        decoder_samples_received: u64,
        decoder_samples_released: u64,
    }

    #[derive(Default)]
    struct DropPublisher {
        presented: u64,
    }

    impl FramePublisher<D3D11VideoSurface> for DropPublisher {
        fn publish(
            &mut self,
            frame: DecodedVideoFrame<D3D11VideoSurface>,
        ) -> Result<PublicationReceipt, clipline_playback::BackendError> {
            self.presented = self.presented.saturating_add(1);
            drop(frame);
            Ok(PublicationReceipt::Presented)
        }

        fn clear(&mut self, _token: PipelineToken) -> Result<(), clipline_playback::BackendError> {
            Ok(())
        }
    }

    struct HeadlessSession {
        client: SessionClient,
        playback: Option<JoinHandle<Result<SessionReport<DropPublisher>, SessionRunError>>>,
        duration: PlaybackTime,
        requested_seeks: u64,
        ended: bool,
    }

    impl HeadlessSession {
        fn open(path: &Path) -> AppResult<Self> {
            let (client, runtime) = session_channel();
            let playback = thread::Builder::new()
                .name("clipline-headless-session".to_owned())
                .spawn(move || runtime.run(DropPublisher::default()))?;
            client.try_send(PlaybackCommand::Open {
                path: path.to_owned(),
            })?;
            let mut session = Self {
                client,
                playback: Some(playback),
                duration: PlaybackTime::new(0, 1)?,
                requested_seeks: 0,
                ended: false,
            };
            session.duration = session.wait_for_open()?;
            Ok(session)
        }

        fn seek(&mut self, requested: PlaybackTime) -> AppResult<()> {
            self.drain_before_command()?;
            self.client.try_send(PlaybackCommand::Seek {
                position: requested,
            })?;
            let deadline = Instant::now() + DEVICE_STALL_TIMEOUT;
            loop {
                while let Some(update) = self.client.try_recv_update() {
                    match update.payload {
                        SessionUpdatePayload::Event(PlaybackEvent::SeekSettled { .. }) => {
                            self.requested_seeks = self.requested_seeks.saturating_add(1);
                            return Ok(());
                        }
                        SessionUpdatePayload::Event(PlaybackEvent::Error { message, .. }) => {
                            return Err(invalid(format!("seek failed: {message}")));
                        }
                        SessionUpdatePayload::Event(PlaybackEvent::Ended { .. }) => {
                            self.ended = true;
                        }
                        _ => {}
                    }
                }
                self.ensure_running("seek")?;
                if Instant::now() >= deadline {
                    return Err(invalid("seek did not settle before the device timeout"));
                }
                thread::sleep(LOOP_SLEEP);
            }
        }

        fn play(&mut self, wall_limit: Option<Duration>) -> AppResult<bool> {
            self.drain_before_command()?;
            self.client.try_send(PlaybackCommand::Play)?;
            let media_timeout = playback_duration(self.duration) + PLAYBACK_TIMEOUT_HEADROOM;
            let deadline = Instant::now() + wall_limit.unwrap_or(media_timeout).min(media_timeout);
            loop {
                while let Some(update) = self.client.try_recv_update() {
                    match update.payload {
                        SessionUpdatePayload::Event(PlaybackEvent::Ended { .. }) => {
                            self.ended = true;
                            return Ok(true);
                        }
                        SessionUpdatePayload::Event(PlaybackEvent::Error { message, .. }) => {
                            return Err(invalid(format!("playback failed: {message}")));
                        }
                        _ => {}
                    }
                }
                self.ensure_running("playback")?;
                if Instant::now() >= deadline {
                    if wall_limit.is_some() {
                        self.pause_and_wait()?;
                        return Ok(false);
                    }
                    return Err(invalid(
                        "playback did not reach EOF before the media timeout",
                    ));
                }
                thread::sleep(LOOP_SLEEP);
            }
        }

        fn finish(mut self, ended: bool) -> AppResult<RunMetrics> {
            self.client.try_send(PlaybackCommand::Close)?;
            let report = self
                .playback
                .take()
                .expect("headless playback thread is joined exactly once")
                .join()
                .map_err(|_| invalid("headless playback thread panicked"))??;
            if report.exit != SessionExit::Closed {
                return Err(invalid(format!(
                    "headless playback exited unexpectedly: {:?}",
                    report.exit
                )));
            }
            let telemetry = report
                .telemetry
                .ok_or_else(|| invalid("opened session returned no final telemetry"))?;
            let metrics = telemetry.metrics;
            if report.publisher.presented != metrics.presented_frames {
                return Err(invalid(
                    "scheduler and publication backend disagree on presented frames",
                ));
            }
            let decoder_info = telemetry.decoder_info.ok_or_else(|| {
                invalid("configured H.264 decoder did not report its final format")
            })?;
            let decoder_acceleration = match decoder_info.acceleration {
                VideoAcceleration::Hardware => "hardware",
                VideoAcceleration::Software => "software",
            }
            .to_owned();
            let renderer = telemetry.renderer;
            let video_buffers = telemetry.video_buffers;
            let audio_packets = telemetry.audio_packets;
            let audio_decode = telemetry.audio_decode;
            let audio_mix = telemetry.audio_mix;
            let ownership = telemetry.decoder_ownership;

            Ok(RunMetrics {
                ended: ended || self.ended,
                requested_seeks: self.requested_seeks,
                settled_seeks: metrics.settled_seeks,
                decoded_frames: ownership.presentable_frames,
                decoded_eligible_frames: metrics.decoded_eligible_frames,
                presented_frames: metrics.presented_frames,
                late_frames: metrics.late_frames,
                scheduler_dropped_frames: metrics.scheduler_dropped_frames,
                presentation_backpressured_frames: metrics.presentation_backpressured_frames,
                presentation_occluded_frames: metrics.presentation_occluded_frames,
                late_or_dropped_frames: metrics.late_or_dropped_frames,
                stale_results: metrics.stale_results,
                max_av_error_ticks: metrics.max_av_error_ticks,
                av_error_p95_ms: metrics.av_error_histogram.percentile_millis(95),
                av_error_histogram_overflowed: metrics.av_error_histogram.overflow() != 0,
                seek_settle_p95_ms: metrics.seek_latency_histogram.percentile_millis(95),
                audio_underruns: renderer.underruns,
                audio_midstream_underruns: telemetry.audio_midstream_underruns,
                audio_terminal_playout_episodes: telemetry.audio_terminal_playout_episodes,
                audio_underrun_frames_estimate: renderer.underrun_frames,
                audio_mixed_frames: audio_mix.mixed_frames,
                audio_silent_frames: audio_mix.silent_frames,
                audio_corrupt_packets: audio_decode.corrupt_packets,
                audio_endpoint_id: renderer.endpoint_id().to_owned(),
                audio_device_format: renderer.device_format().to_owned(),
                audio_device_sample_rate: renderer.device_sample_rate,
                audio_device_channels: renderer.device_channels,
                audio_device_bits_per_sample: renderer.device_bits_per_sample,
                audio_device_valid_bits_per_sample: renderer.device_valid_bits_per_sample,
                audio_device_channel_mask: renderer.device_channel_mask,
                audio_device_buffer_duration_100ns: renderer.device_buffer_duration_100ns,
                audio_conversion_active: renderer.conversion_active,
                audio_initialization_path: initialization_path_label(renderer.initialization_path)
                    .to_owned(),
                audio_engine_period_frames: renderer.engine_period_frames,
                audio_clock_frequency: renderer.clock_frequency,
                audio_recovery_count: renderer.recovery_count,
                video_encoded_buffer_capacity: video_buffers.encoded_capacity,
                video_converted_buffer_capacity: video_buffers.converted_capacity,
                video_encoded_high_water: video_buffers.encoded_high_water,
                video_converted_high_water: video_buffers.converted_high_water,
                audio_packet_capacity: audio_packets.packet_capacity,
                audio_packet_high_water: audio_packets.packet_high_water,
                audio_queue_high_water_frames: audio_mix.queue_high_water_frames,
                endpoint_buffer_frames: renderer.buffer_frames,
                endpoint_epoch: renderer.endpoint_epoch,
                decoder_acceleration,
                decoder_presentable_frames: ownership.presentable_frames,
                decoder_output_copies: ownership.output_copies,
                decoder_samples_received: ownership.mft_samples_received,
                decoder_samples_released: ownership.mft_samples_released,
            })
        }

        fn wait_for_open(&mut self) -> AppResult<PlaybackTime> {
            let deadline = Instant::now() + DEVICE_STALL_TIMEOUT;
            loop {
                while let Some(update) = self.client.try_recv_update() {
                    match update.payload {
                        SessionUpdatePayload::Event(PlaybackEvent::Opened { duration, .. }) => {
                            return Ok(duration);
                        }
                        SessionUpdatePayload::Event(PlaybackEvent::Error { message, .. }) => {
                            return Err(invalid(format!("open failed: {message}")));
                        }
                        _ => {}
                    }
                }
                self.ensure_running("open")?;
                if Instant::now() >= deadline {
                    return Err(invalid("media did not open before the device timeout"));
                }
                thread::sleep(LOOP_SLEEP);
            }
        }

        fn pause_and_wait(&mut self) -> AppResult<()> {
            self.client.try_send(PlaybackCommand::Pause)?;
            let deadline = Instant::now() + DEVICE_STALL_TIMEOUT;
            loop {
                while let Some(update) = self.client.try_recv_update() {
                    match update.payload {
                        SessionUpdatePayload::Snapshot(snapshot)
                            if matches!(
                                snapshot.phase,
                                PlaybackPhase::Paused | PlaybackPhase::Ended
                            ) =>
                        {
                            return Ok(());
                        }
                        SessionUpdatePayload::Event(PlaybackEvent::Ended { .. }) => {
                            self.ended = true;
                            return Ok(());
                        }
                        SessionUpdatePayload::Event(PlaybackEvent::Error { message, .. }) => {
                            return Err(invalid(format!("pause failed: {message}")));
                        }
                        _ => {}
                    }
                }
                self.ensure_running("pause")?;
                if Instant::now() >= deadline {
                    return Err(invalid("pause did not settle before the device timeout"));
                }
                thread::sleep(LOOP_SLEEP);
            }
        }

        fn drain_before_command(&mut self) -> AppResult<()> {
            while let Some(update) = self.client.try_recv_update() {
                match update.payload {
                    SessionUpdatePayload::Event(PlaybackEvent::Error { message, .. }) => {
                        return Err(invalid(format!("playback session failed: {message}")));
                    }
                    SessionUpdatePayload::Event(PlaybackEvent::Ended { .. }) => {
                        self.ended = true;
                    }
                    _ => {}
                }
            }
            self.ensure_running("command")
        }

        fn ensure_running(&self, operation: &str) -> AppResult<()> {
            if self.playback.as_ref().is_some_and(JoinHandle::is_finished) {
                Err(invalid(format!(
                    "headless playback thread exited during {operation}"
                )))
            } else {
                Ok(())
            }
        }
    }

    impl Drop for HeadlessSession {
        fn drop(&mut self) {
            if let Some(playback) = self.playback.take() {
                let _ = self.client.try_send(PlaybackCommand::Close);
                let _ = playback.join();
            }
        }
    }

    struct WorkingFixture {
        source: PathBuf,
        active: PathBuf,
        renamed: PathBuf,
    }

    impl WorkingFixture {
        fn copy(source: &Path) -> AppResult<Self> {
            let directory = std::env::temp_dir()
                .join(format!("clipline-headless-playback-{}", std::process::id()));
            fs::create_dir_all(&directory)?;
            let active = directory.join("active.mp4");
            let renamed = directory.join("closed.mp4");
            let _ = fs::remove_file(&active);
            let _ = fs::remove_file(&renamed);
            fs::copy(source, &active)?;
            Ok(Self {
                source: source.to_owned(),
                active,
                renamed,
            })
        }

        fn verify_release(&self) -> AppResult<()> {
            fs::rename(&self.active, &self.renamed)?;
            fs::rename(&self.renamed, &self.active)?;
            Ok(())
        }
    }

    impl Drop for WorkingFixture {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.active);
            let _ = fs::remove_file(&self.renamed);
            if let Some(directory) = self.active.parent() {
                let _ = fs::remove_dir(directory);
            }
        }
    }

    pub fn run() -> AppResult<()> {
        let mut cli = Cli::parse()?;
        if !cli.fixture.is_absolute() {
            cli.fixture = std::env::current_dir()?.join(&cli.fixture);
        }
        let overall_start = Instant::now();
        let fixture = WorkingFixture::copy(&cli.fixture)?;
        let mut aggregate = AggregateMetrics {
            process_id: std::process::id(),
            ..AggregateMetrics::default()
        };

        match cli.scenario {
            Scenario::Playback => {
                let duration = cli.run_seconds.map(Duration::from_secs);
                loop {
                    let mut session = HeadlessSession::open(&fixture.active)?;
                    let ended = session.play(None)?;
                    aggregate.absorb(session.finish(ended)?);
                    if duration.is_none_or(|duration| overall_start.elapsed() >= duration) {
                        break;
                    }
                }
            }
            Scenario::SeekStorm => {
                let duration = cli.run_seconds.map(Duration::from_secs);
                loop {
                    let mut session = HeadlessSession::open(&fixture.active)?;
                    for target_ms in [4_500, 250, 3_750, 750, 4_000] {
                        session.seek(PlaybackTime::new(target_ms, 1_000)?)?;
                        let _ = session.play(Some(SEEK_PREVIEW_DURATION))?;
                    }
                    let ended = session.play(None)?;
                    aggregate.absorb(session.finish(ended)?);
                    if duration.is_none_or(|duration| overall_start.elapsed() >= duration) {
                        break;
                    }
                }
            }
            Scenario::Cycle100 => {
                let duration = cli.run_seconds.map(Duration::from_secs);
                for cycle in 0..DEFAULT_CYCLES {
                    if let Some(duration) = duration {
                        pace_cycle(overall_start, duration, cycle, DEFAULT_CYCLES);
                    }
                    let mut session = HeadlessSession::open(&fixture.active)?;
                    let _ = session.play(Some(CYCLE_PREVIEW_DURATION))?;
                    let target_ms = 250 + (cycle as u64 % 9) * 500;
                    session.seek(PlaybackTime::new(target_ms, 1_000)?)?;
                    let ended = session.play(Some(CYCLE_PREVIEW_DURATION))?;
                    aggregate.absorb(session.finish(ended)?);
                }
                if let Some(duration) = duration {
                    sleep_until(overall_start + duration);
                }
            }
        }

        fixture.verify_release()?;
        aggregate.file_release_verified = true;
        aggregate.elapsed_ms =
            u64::try_from(overall_start.elapsed().as_millis()).unwrap_or(u64::MAX);
        let document = TelemetryDocument {
            schema_version: 1,
            scenario: cli.scenario.label(),
            status: "ok",
            source_fixture: fixture.source.display().to_string(),
            metrics: aggregate,
        };
        let json = serde_json::to_vec_pretty(&document)?;
        write_telemetry_atomically(&cli.telemetry, &json)?;
        Ok(())
    }

    fn playback_duration(duration: PlaybackTime) -> Duration {
        Duration::from_secs_f64(duration.ticks as f64 / f64::from(duration.timescale))
    }

    fn pace_cycle(start: Instant, duration: Duration, cycle: usize, total_cycles: usize) {
        let fraction = cycle as f64 / total_cycles as f64;
        sleep_until(start + duration.mul_f64(fraction));
    }

    fn sleep_until(deadline: Instant) {
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            if remaining.is_zero() {
                break;
            }
            thread::sleep(remaining.min(Duration::from_millis(50)));
        }
    }

    fn write_telemetry_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty());
        if let Some(parent) = parent {
            fs::create_dir_all(parent)?;
        }
        let file_name = path
            .file_name()
            .ok_or_else(|| {
                io::Error::new(ErrorKind::InvalidInput, "telemetry path has no file name")
            })?
            .to_string_lossy();
        let temporary_name = format!(".{file_name}.{}.tmp", std::process::id());
        let temporary = parent
            .unwrap_or_else(|| Path::new("."))
            .join(temporary_name);
        let _ = fs::remove_file(&temporary);

        let result = (|| {
            let mut output = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)?;
            output.write_all(bytes)?;
            output.sync_all()?;
            drop(output);
            if path.exists() {
                return Err(io::Error::new(
                    ErrorKind::AlreadyExists,
                    "telemetry target already exists",
                ));
            }
            fs::rename(&temporary, path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn max_option<T: Copy + Ord>(left: Option<T>, right: Option<T>) -> Option<T> {
        match (left, right) {
            (Some(left), Some(right)) => Some(left.max(right)),
            (Some(value), None) | (None, Some(value)) => Some(value),
            (None, None) => None,
        }
    }

    fn merge_descriptor(current: &mut String, next: String) {
        if current.is_empty() {
            *current = next;
        } else if *current != next {
            *current = "mixed".to_owned();
        }
    }

    const fn initialization_path_label(path: WasapiInitializationPath) -> &'static str {
        match path {
            WasapiInitializationPath::AudioClient3 => "audio-client-3",
            WasapiInitializationPath::LegacySharedAutoConvert => "legacy-shared-auto-convert",
        }
    }

    fn merge_fail_closed_percentile(
        current: Option<u16>,
        next: Option<u16>,
        overflowed: &mut bool,
        next_overflowed: bool,
    ) -> Option<u16> {
        *overflowed |= next_overflowed;
        if *overflowed {
            None
        } else {
            max_option(current, next)
        }
    }

    fn invalid(message: impl Into<String>) -> Box<dyn Error + Send + Sync> {
        Box::new(io::Error::new(ErrorKind::InvalidData, message.into()))
    }

    #[cfg(test)]
    mod tests {
        use super::merge_fail_closed_percentile;

        #[test]
        fn percentile_aggregation_stays_failed_closed_after_any_overflow() {
            let mut overflowed = false;
            let first = merge_fail_closed_percentile(None, Some(12), &mut overflowed, false);
            assert_eq!(first, Some(12));

            let rejected = merge_fail_closed_percentile(first, None, &mut overflowed, true);
            assert_eq!(rejected, None);
            assert!(overflowed);

            let still_rejected =
                merge_fail_closed_percentile(rejected, Some(3), &mut overflowed, false);
            assert_eq!(still_rejected, None);
        }
    }
}
