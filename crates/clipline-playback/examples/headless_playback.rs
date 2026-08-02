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
    use std::time::{Duration, Instant};

    use clipline_mp4::{IndexedMovie, PlaybackTime, PlaybackTrackConfig, TrackSampleRange};
    use clipline_playback::windows::{
        D3D11VideoSurface, DecoderPreference, WasapiInitializationPath, WindowsH264Decoder,
        WindowsWasapiRenderer,
    };
    use clipline_playback::{
        plan_audio_fill, plan_video_sample_buffers, AdmitOutcome, AudioAvailability, AudioRenderer,
        AudioResetPoint, AudioTrackSpec, DecodedVideoFrame, EncodedVideoPacket, EndOfStreamTracker,
        FramePublisher, FrameScheduler, IndexedAudioPacketReader, MonotonicTime100ns,
        OpusDecoderBank, PipelineToken, PlaybackMetrics, PublicationReceipt, RebasedAudioClock,
        SeekTarget, SubmitStatus, TimelineAudioMixer, TimelineDuration, TimelinePosition,
        VideoAcceleration, VideoDecoder, VideoSampleTransport, WorkGeneration,
        MAX_AUDIO_QUEUE_FRAMES, MAX_AUDIO_WRITE_FRAMES, MAX_OPUS_FRAME_SAMPLES,
        PLAYBACK_TIMELINE_HZ,
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
            if self.decoder_acceleration.is_empty() {
                self.decoder_acceleration = run.decoder_acceleration;
            } else if self.decoder_acceleration != run.decoder_acceleration {
                self.decoder_acceleration = "mixed".to_owned();
            }
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
    struct SchedulerTotals {
        decoded_eligible_frames: u64,
        presented_frames: u64,
        late_frames: u64,
        scheduler_dropped_frames: u64,
        presentation_backpressured_frames: u64,
        presentation_occluded_frames: u64,
        late_or_dropped_frames: u64,
        max_av_error_ticks: u64,
        av_error_p95_ms: Option<u16>,
        av_error_histogram_overflowed: bool,
    }

    impl SchedulerTotals {
        fn absorb(&mut self, metrics: &PlaybackMetrics) {
            self.decoded_eligible_frames = self
                .decoded_eligible_frames
                .saturating_add(metrics.decoded_eligible_frames);
            self.presented_frames = self
                .presented_frames
                .saturating_add(metrics.presented_frames);
            self.late_frames = self.late_frames.saturating_add(metrics.late_frames);
            self.scheduler_dropped_frames = self
                .scheduler_dropped_frames
                .saturating_add(metrics.scheduler_dropped_frames);
            self.presentation_backpressured_frames = self
                .presentation_backpressured_frames
                .saturating_add(metrics.presentation_backpressured_frames);
            self.presentation_occluded_frames = self
                .presentation_occluded_frames
                .saturating_add(metrics.presentation_occluded_frames);
            self.late_or_dropped_frames = self
                .late_or_dropped_frames
                .saturating_add(metrics.late_or_dropped_frames);
            self.max_av_error_ticks = self.max_av_error_ticks.max(metrics.max_av_error_ticks);
            self.av_error_p95_ms = merge_fail_closed_percentile(
                self.av_error_p95_ms,
                metrics.av_error_histogram.percentile_millis(95),
                &mut self.av_error_histogram_overflowed,
                metrics.av_error_histogram.overflow() != 0,
            );
        }
    }

    struct DropPublisher {
        presented: u64,
        last_sample_index: Option<usize>,
    }

    impl DropPublisher {
        const fn new() -> Self {
            Self {
                presented: 0,
                last_sample_index: None,
            }
        }
    }

    impl FramePublisher<D3D11VideoSurface> for DropPublisher {
        fn publish(
            &mut self,
            frame: DecodedVideoFrame<D3D11VideoSurface>,
        ) -> Result<PublicationReceipt, clipline_playback::BackendError> {
            self.presented = self.presented.saturating_add(1);
            self.last_sample_index = Some(frame.sample_index());
            drop(frame);
            Ok(PublicationReceipt::Presented)
        }

        fn clear(&mut self, _token: PipelineToken) -> Result<(), clipline_playback::BackendError> {
            self.last_sample_index = None;
            Ok(())
        }
    }

    struct HeadlessSession {
        video: VideoSampleTransport<std::fs::File>,
        decoder: WindowsH264Decoder,
        renderer: WindowsWasapiRenderer,
        audio_reader: IndexedAudioPacketReader<std::fs::File>,
        audio_bank: OpusDecoderBank,
        audio_mixer: TimelineAudioMixer,
        scheduler: FrameScheduler<D3D11VideoSurface>,
        publisher: DropPublisher,
        clock: RebasedAudioClock,
        eos: EndOfStreamTracker,
        token: PipelineToken,
        generation: WorkGeneration,
        video_timescale: u32,
        video_sample_count: usize,
        next_video_sample: usize,
        video_drain_sent: bool,
        audio_tracks: Vec<usize>,
        audio_sample_count: usize,
        next_audio_sample: usize,
        audio_finished: bool,
        audio_playback_start: u64,
        video_end: TimelinePosition,
        audio_mix_scratch: Vec<f32>,
        audio_output: Vec<f32>,
        started: bool,
        run_anchor: Instant,
        requested_seeks: u64,
        seek_latencies_ms: Vec<u16>,
        last_recorded_settled_seeks: u64,
        seek_target_sample: usize,
        scheduler_totals: SchedulerTotals,
        last_observed_underruns: u64,
        audio_midstream_underruns: u64,
        audio_terminal_playout_episodes: u64,
        decoder_acceleration: String,
    }

    impl HeadlessSession {
        fn open(path: &Path) -> AppResult<Self> {
            let generation = WorkGeneration::new(1, 0);
            let token = PipelineToken::new(generation, 0);
            let video_movie = IndexedMovie::open(path)?;
            let video_track_index = find_video_track(video_movie.index())?;
            let video_track = &video_movie.index().tracks[video_track_index];
            let video_plan = plan_video_sample_buffers(video_track, Default::default())?;
            let video_timescale = video_track.timescale;
            let video_sample_count = video_track.samples.len();
            let video_end = video_track
                .samples
                .last()
                .ok_or_else(|| invalid("H.264 track has no samples"))
                .and_then(|sample| {
                    let pts = u64::try_from(sample.pts)
                        .map_err(|_| invalid("fixture has a negative final video PTS"))?;
                    let end = pts
                        .checked_add(u64::from(sample.duration))
                        .ok_or_else(|| invalid("final video interval overflow"))?;
                    Ok(TimelinePosition::new(rescale_to_timeline(
                        end,
                        video_timescale,
                    )?))
                })?;
            let video = VideoSampleTransport::new(video_movie, video_track_index, generation)?;

            let audio_movie = IndexedMovie::open(path)?;
            let audio_tracks = find_audio_tracks(audio_movie.index());
            let mut audio_specs = Vec::with_capacity(audio_tracks.len());
            let mut audio_ranges = Vec::with_capacity(audio_tracks.len());
            let mut audio_sample_count = None;
            for &track_index in &audio_tracks {
                let track = &audio_movie.index().tracks[track_index];
                let PlaybackTrackConfig::Opus {
                    channels,
                    sample_rate,
                    pre_skip,
                } = track.config
                else {
                    unreachable!("audio track search returned a non-Opus track");
                };
                if let Some(expected) = audio_sample_count {
                    if expected != track.samples.len() {
                        return Err(invalid(
                            "headless fixture requires selected Opus tracks with aligned packet counts",
                        ));
                    }
                } else {
                    audio_sample_count = Some(track.samples.len());
                }
                audio_specs.push(AudioTrackSpec::new(
                    track_index,
                    channels,
                    sample_rate,
                    pre_skip,
                )?);
                audio_ranges.push(TrackSampleRange {
                    track_index,
                    samples: 0..track.samples.len(),
                });
            }
            let timeline_end = PlaybackTime::new(video_end.ticks(), PLAYBACK_TIMELINE_HZ)?;
            let audio_reader =
                IndexedAudioPacketReader::new(audio_movie, audio_ranges, timeline_end, generation)?;
            let mut audio_bank = OpusDecoderBank::new();
            audio_bank.select_tracks(&audio_specs, generation, AudioResetPoint::FileStart)?;
            let audio_mixer = TimelineAudioMixer::new(MAX_AUDIO_QUEUE_FRAMES, 0)?;

            let mut decoder = WindowsH264Decoder::new(DecoderPreference::PreferHardware)?;
            decoder.configure(&video_plan.config, token)?;
            let decoder_info = decoder
                .info()
                .ok_or_else(|| invalid("configured H.264 decoder did not report its format"))?;
            let decoder_acceleration = match decoder_info.acceleration {
                VideoAcceleration::Hardware => "hardware",
                VideoAcceleration::Software => "software",
            }
            .to_owned();

            let mut renderer = WindowsWasapiRenderer::open_default()?;
            renderer.reset(token)?;
            let raw = renderer.raw_clock()?;
            let clock = RebasedAudioClock::new(raw, TimelinePosition::new(0));
            let seek_target = SeekTarget::new(TimelinePosition::new(0), 0);

            Ok(Self {
                video,
                decoder,
                renderer,
                audio_reader,
                audio_bank,
                audio_mixer,
                scheduler: FrameScheduler::new(token, seek_target),
                publisher: DropPublisher::new(),
                clock,
                eos: EndOfStreamTracker::new(video_end),
                token,
                generation,
                video_timescale,
                video_sample_count,
                next_video_sample: 0,
                video_drain_sent: false,
                audio_tracks,
                audio_sample_count: audio_sample_count.unwrap_or(0),
                next_audio_sample: 0,
                audio_finished: false,
                audio_playback_start: 0,
                video_end,
                audio_mix_scratch: vec![0.0; MAX_OPUS_FRAME_SAMPLES * 2],
                audio_output: vec![0.0; MAX_AUDIO_WRITE_FRAMES * 2],
                started: false,
                run_anchor: Instant::now(),
                requested_seeks: 0,
                seek_latencies_ms: Vec::new(),
                last_recorded_settled_seeks: 0,
                seek_target_sample: 0,
                scheduler_totals: SchedulerTotals::default(),
                last_observed_underruns: 0,
                audio_midstream_underruns: 0,
                audio_terminal_playout_episodes: 0,
                decoder_acceleration,
            })
        }

        fn seek(&mut self, requested: PlaybackTime) -> AppResult<()> {
            if self.started {
                self.renderer.pause(self.token)?;
                self.started = false;
            }
            self.observe_renderer_underruns(self.clock.position());
            let next_seek = self
                .generation
                .seek
                .checked_add(1)
                .ok_or_else(|| invalid("seek generation overflow"))?;
            self.generation = WorkGeneration::new(self.generation.open, next_seek);
            self.token = PipelineToken::new(self.generation, next_seek);
            let plan = self.video.seek_plan(&self.audio_tracks, requested)?;
            let target_sample = plan
                .video_preroll
                .samples
                .end
                .checked_sub(1)
                .ok_or_else(|| invalid("seek plan has no target video sample"))?;
            let target_tick =
                rescale_to_timeline(plan.target_time.ticks, plan.target_time.timescale)?;
            let target = TimelinePosition::new(target_tick);

            self.snapshot_scheduler_quality();
            self.decoder.flush(self.token)?;
            self.video.reset_for_generation(self.generation);
            self.renderer.reset(self.token)?;
            let raw = self.renderer.raw_clock()?;
            self.clock.rebase(raw, target);
            self.scheduler.begin_seek(
                self.token,
                SeekTarget::new(target, target_sample),
                self.monotonic_now(),
            );
            self.seek_target_sample = target_sample;
            self.last_recorded_settled_seeks = 0;
            self.publisher.clear(self.token)?;
            self.eos = EndOfStreamTracker::new(self.video_end);
            self.next_video_sample = plan.video_preroll.samples.start;
            self.video_drain_sent = false;

            let mut ranges = Vec::with_capacity(self.audio_tracks.len());
            let mut first_audio_sample = None;
            for selected in &plan.audio_preroll {
                if let Some(expected) = first_audio_sample {
                    if expected != selected.samples.start {
                        return Err(invalid(
                            "headless fixture requires aligned audio seek ranges",
                        ));
                    }
                } else {
                    first_audio_sample = Some(selected.samples.start);
                }
                let end = self.audio_reader.index().tracks[selected.track_index]
                    .samples
                    .len();
                ranges.push(TrackSampleRange {
                    track_index: selected.track_index,
                    samples: selected.samples.start..end,
                });
            }
            self.audio_reader.reselect_ranges(
                ranges,
                PlaybackTime::new(self.video_end.ticks(), PLAYBACK_TIMELINE_HZ)?,
                self.generation,
            )?;
            self.audio_bank
                .reset_for_seek(self.generation, AudioResetPoint::MidStream)?;
            self.audio_mixer.reset_at(target_tick);
            self.next_audio_sample = first_audio_sample.unwrap_or(0);
            self.audio_finished = self.audio_tracks.is_empty();
            self.audio_playback_start = target_tick;
            self.requested_seeks = self.requested_seeks.saturating_add(1);
            Ok(())
        }

        fn play(&mut self, wall_limit: Option<Duration>) -> AppResult<bool> {
            self.prime()?;
            self.renderer.start(self.token)?;
            self.started = true;
            let started_at = Instant::now();
            let media_timeout_ticks = self
                .video_end
                .ticks()
                .saturating_sub(self.clock.position().ticks());
            let media_timeout = Duration::from_secs_f64(
                media_timeout_ticks as f64 / f64::from(PLAYBACK_TIMELINE_HZ),
            ) + PLAYBACK_TIMEOUT_HEADROOM;
            let deadline = started_at + wall_limit.unwrap_or(media_timeout).min(media_timeout);

            loop {
                let before = self.sample_clock()?;
                self.service_audio(before)?;
                self.pump_video(before)?;
                self.tick_scheduler(before)?;
                if self.publisher.last_sample_index == self.video_sample_count.checked_sub(1) {
                    self.eos.mark_final_frame_consumed();
                }
                let after = self.sample_clock()?;
                self.record_seek_latency();
                if self.eos.update(after) {
                    self.renderer.pause(self.token)?;
                    self.started = false;
                    return Ok(true);
                }
                if Instant::now() >= deadline {
                    if wall_limit.is_some() {
                        self.renderer.pause(self.token)?;
                        self.started = false;
                        return Ok(false);
                    }
                    return Err(invalid(
                        "native playback did not reach EOF before its deadline",
                    ));
                }
                std::thread::sleep(LOOP_SLEEP);
            }
        }

        fn prime(&mut self) -> AppResult<()> {
            let deadline = Instant::now() + DEVICE_STALL_TIMEOUT;
            loop {
                let clock = self.sample_clock()?;
                self.service_audio(clock)?;
                self.pump_video(clock)?;
                self.tick_scheduler(clock)?;
                let writable_frames = self.renderer.writable_frames()?;
                let audio_full = writable_frames == 0 || self.clock.position() >= self.video_end;
                let target_published = self
                    .publisher
                    .last_sample_index
                    .is_some_and(|sample| sample >= self.scheduler_target_sample());
                if audio_full && target_published {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err(invalid(format!(
                        "native playback could not prime audio/video in time: target sample {}, \
                         last published {:?}, pending {}, next video {}, writable audio {}, clock {}",
                        self.scheduler_target_sample(),
                        self.publisher.last_sample_index,
                        self.scheduler.pending_frames(),
                        self.next_video_sample,
                        writable_frames,
                        clock.ticks(),
                    )));
                }
                std::thread::yield_now();
            }
        }

        fn scheduler_target_sample(&self) -> usize {
            self.seek_target_sample
        }

        fn sample_clock(&mut self) -> AppResult<TimelinePosition> {
            let raw = self.renderer.raw_clock()?;
            Ok(self.clock.sample(raw)?)
        }

        fn service_audio(&mut self, clock: TimelinePosition) -> AppResult<()> {
            let writable = self.renderer.writable_frames()?;
            self.observe_renderer_underruns(clock);
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
                return Err(invalid(
                    "audio decoder made no progress before endpoint fill",
                ));
            };
            let plan = plan_audio_fill(clock, self.video_end, writable, availability);
            if plan.pcm_frames != 0 {
                let samples = plan.pcm_frames * 2;
                let drained = self
                    .audio_mixer
                    .drain_into(&mut self.audio_output[..samples])?;
                if drained != plan.pcm_frames {
                    return Err(invalid("audio mixer returned fewer frames than planned"));
                }
                let written = self
                    .renderer
                    .write_stereo_frames(&self.audio_output[..samples], self.token)?;
                if written != drained {
                    return Err(invalid(
                        "WASAPI accepted fewer frames than its padding report",
                    ));
                }
            } else if plan.silence_frames != 0 {
                let samples = plan.silence_frames * 2;
                self.audio_output[..samples].fill(0.0);
                let written = self
                    .renderer
                    .write_stereo_frames(&self.audio_output[..samples], self.token)?;
                if written != plan.silence_frames {
                    return Err(invalid("WASAPI accepted fewer silence frames than planned"));
                }
            }
            Ok(())
        }

        fn observe_renderer_underruns(&mut self, clock: TimelinePosition) {
            let telemetry = self.renderer.telemetry();
            let new_episodes = telemetry
                .underruns
                .saturating_sub(self.last_observed_underruns);
            if new_episodes == 0 {
                return;
            }
            if is_terminal_playout(clock, self.video_end, telemetry.buffer_frames) {
                self.audio_terminal_playout_episodes = self
                    .audio_terminal_playout_episodes
                    .saturating_add(new_episodes);
            } else {
                self.audio_midstream_underruns =
                    self.audio_midstream_underruns.saturating_add(new_episodes);
            }
            self.last_observed_underruns = telemetry.underruns;
        }

        fn decode_audio_until(&mut self, target_frames: usize) -> AppResult<()> {
            while self.audio_mixer.queued_frames() < target_frames && !self.audio_finished {
                if self.audio_tracks.is_empty() {
                    self.audio_finished = true;
                    break;
                }
                if self.next_audio_sample >= self.audio_sample_count {
                    self.audio_mixer.finish_at(self.video_end.ticks())?;
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
                    let packet = self.audio_reader.read_packet(
                        track_index,
                        self.next_audio_sample,
                        self.generation,
                    )?;
                    if let Some(expected) = audible_start {
                        if expected != packet.audible_start_tick {
                            return Err(invalid("selected Opus packets are not timeline-aligned"));
                        }
                    } else {
                        audible_start = Some(packet.audible_start_tick);
                    }
                    self.audio_bank.decode_indexed(
                        track_index,
                        packet.bytes,
                        packet.indexed_duration_frames,
                        self.generation,
                    )?;
                    let decoded = self.audio_bank.pending_frames(track_index)?;
                    if let Some(expected) = frames {
                        if expected != decoded {
                            return Err(invalid(
                                "selected Opus packets decoded unequal frame counts",
                            ));
                        }
                    } else {
                        frames = Some(decoded);
                    }
                }

                let frames = frames.unwrap_or(0);
                let audible_start = audible_start.unwrap_or(self.audio_playback_start);
                self.audio_bank.mix_pending_into(
                    &self.audio_tracks,
                    frames,
                    &mut self.audio_mix_scratch[..frames * 2],
                )?;
                let skipped =
                    usize::try_from(self.audio_playback_start.saturating_sub(audible_start))
                        .unwrap_or(usize::MAX)
                        .min(frames);
                let kept_start = audible_start.saturating_add(skipped as u64);
                let timeline_remaining = self.video_end.ticks().saturating_sub(kept_start);
                let kept = (frames - skipped)
                    .min(usize::try_from(timeline_remaining).unwrap_or(usize::MAX));
                if kept != 0 {
                    self.audio_mixer.mix_at(
                        kept_start,
                        kept,
                        &[Some(&self.audio_mix_scratch[skipped * 2..frames * 2])],
                    )?;
                }
                self.next_audio_sample = self
                    .next_audio_sample
                    .checked_add(1)
                    .ok_or_else(|| invalid("audio sample cursor overflow"))?;
            }
            Ok(())
        }

        fn pump_video(&mut self, clock: TimelinePosition) -> AppResult<()> {
            if self.scheduler.pending_frames() != 0 {
                return Ok(());
            }
            for _ in 0..64 {
                if let Some(frame) = self.decoder.receive()? {
                    match self.scheduler.admit(frame, clock) {
                        Ok(AdmitOutcome::Preroll | AdmitOutcome::Stale) => continue,
                        Ok(_) => return Ok(()),
                        Err(frame) => {
                            drop(frame);
                            return Err(invalid(
                                "scheduler rejected a second future presentation surface",
                            ));
                        }
                    }
                }
                if self.next_video_sample < self.video_sample_count {
                    let submission = {
                        let unit = self
                            .video
                            .read_sample(self.next_video_sample, self.generation)?;
                        let status = self.decoder.submit(
                            EncodedVideoPacket {
                                bytes: unit.bytes,
                                sample_index: unit.sample_index,
                                pts: timeline_position(unit.pts, self.video_timescale)?,
                                duration: timeline_duration(unit.duration, self.video_timescale)?,
                                is_sync: unit.is_sync,
                            },
                            self.token,
                        )?;
                        (status, unit.parameter_set_submission)
                    };
                    if submission.0 == SubmitStatus::Accepted {
                        if let Some(parameter_sets) = submission.1 {
                            if !self.video.commit_parameter_sets(parameter_sets) {
                                return Err(invalid(
                                    "decoder accepted stale H.264 parameter-set submission",
                                ));
                            }
                        }
                        self.next_video_sample = self
                            .next_video_sample
                            .checked_add(1)
                            .ok_or_else(|| invalid("video sample cursor overflow"))?;
                    }
                } else if !self.video_drain_sent {
                    self.decoder.drain(self.token)?;
                    self.video_drain_sent = true;
                } else {
                    return Ok(());
                }
            }
            Ok(())
        }

        fn tick_scheduler(&mut self, before: TimelinePosition) -> AppResult<bool> {
            let now = self.monotonic_now();
            let renderer = &mut self.renderer;
            let clock = &mut self.clock;
            let scheduler = &mut self.scheduler;
            let publisher = &mut self.publisher;
            let mut backend_error = None;
            let tick = scheduler.tick(
                before,
                publisher,
                || match renderer.raw_clock() {
                    Ok(raw) => clock.sample(raw),
                    Err(error) => {
                        backend_error = Some(error);
                        Err(clipline_playback::ClockError::TimelineOverflow)
                    }
                },
                now,
            );
            if let Some(error) = backend_error {
                return Err(Box::new(error));
            }
            Ok(tick?)
        }

        fn record_seek_latency(&mut self) {
            let metrics = self.scheduler.metrics();
            if metrics.settled_seeks > self.last_recorded_settled_seeks {
                if let Some(latency) = metrics.latest_seek_latency_100ns {
                    let millis = latency.saturating_add(9_999) / 10_000;
                    self.seek_latencies_ms
                        .push(u16::try_from(millis).unwrap_or(u16::MAX));
                }
                self.last_recorded_settled_seeks = metrics.settled_seeks;
            }
        }

        fn snapshot_scheduler_quality(&mut self) {
            self.scheduler_totals.absorb(self.scheduler.metrics());
        }

        fn monotonic_now(&self) -> MonotonicTime100ns {
            let ticks = self.run_anchor.elapsed().as_nanos() / 100;
            MonotonicTime100ns::new(u64::try_from(ticks).unwrap_or(u64::MAX))
        }

        fn finish(mut self, ended: bool) -> AppResult<RunMetrics> {
            if self.started {
                self.renderer.pause(self.token)?;
                self.started = false;
            }
            self.observe_renderer_underruns(self.clock.position());
            let stale_results = self.scheduler.metrics().stale_results;
            self.snapshot_scheduler_quality();
            if self.scheduler_totals.presented_frames != self.publisher.presented {
                return Err(invalid(
                    "scheduler and publication backend disagree on presented frames",
                ));
            }
            let decoder_info = self.decoder.ownership_telemetry();
            let renderer_info = self.renderer.telemetry();
            let video_buffers = self.video.buffer_telemetry();
            let audio_packets = self.audio_reader.telemetry();
            let audio_decode = self.audio_bank.stats();
            let audio_mix = self.audio_mixer.stats();
            let seek_settle_p95_ms = percentile_u16(&mut self.seek_latencies_ms, 95);
            self.decoder.close();
            self.renderer.close();

            Ok(RunMetrics {
                ended,
                requested_seeks: self.requested_seeks,
                settled_seeks: self.seek_latencies_ms.len() as u64,
                decoded_frames: decoder_info.presentable_frames,
                decoded_eligible_frames: self.scheduler_totals.decoded_eligible_frames,
                presented_frames: self.scheduler_totals.presented_frames,
                late_frames: self.scheduler_totals.late_frames,
                scheduler_dropped_frames: self.scheduler_totals.scheduler_dropped_frames,
                presentation_backpressured_frames: self
                    .scheduler_totals
                    .presentation_backpressured_frames,
                presentation_occluded_frames: self.scheduler_totals.presentation_occluded_frames,
                late_or_dropped_frames: self.scheduler_totals.late_or_dropped_frames,
                stale_results,
                max_av_error_ticks: self.scheduler_totals.max_av_error_ticks,
                av_error_p95_ms: self.scheduler_totals.av_error_p95_ms,
                av_error_histogram_overflowed: self.scheduler_totals.av_error_histogram_overflowed,
                seek_settle_p95_ms,
                audio_underruns: renderer_info.underruns,
                audio_midstream_underruns: self.audio_midstream_underruns,
                audio_terminal_playout_episodes: self.audio_terminal_playout_episodes,
                audio_underrun_frames_estimate: renderer_info.underrun_frames,
                audio_mixed_frames: audio_mix.mixed_frames,
                audio_silent_frames: audio_mix.silent_frames,
                audio_corrupt_packets: audio_decode.corrupt_packets,
                audio_endpoint_id: renderer_info.endpoint_id().to_owned(),
                audio_device_format: renderer_info.device_format().to_owned(),
                audio_device_sample_rate: renderer_info.device_sample_rate,
                audio_device_channels: renderer_info.device_channels,
                audio_device_bits_per_sample: renderer_info.device_bits_per_sample,
                audio_device_valid_bits_per_sample: renderer_info.device_valid_bits_per_sample,
                audio_device_channel_mask: renderer_info.device_channel_mask,
                audio_device_buffer_duration_100ns: renderer_info.device_buffer_duration_100ns,
                audio_conversion_active: renderer_info.conversion_active,
                audio_initialization_path: initialization_path_label(
                    renderer_info.initialization_path,
                )
                .to_owned(),
                audio_engine_period_frames: renderer_info.engine_period_frames,
                audio_clock_frequency: renderer_info.clock_frequency,
                audio_recovery_count: renderer_info.recovery_count,
                video_encoded_buffer_capacity: video_buffers.encoded_capacity,
                video_converted_buffer_capacity: video_buffers.converted_capacity,
                video_encoded_high_water: video_buffers.encoded_high_water,
                video_converted_high_water: video_buffers.converted_high_water,
                audio_packet_capacity: audio_packets.packet_capacity,
                audio_packet_high_water: audio_packets.packet_high_water,
                audio_queue_high_water_frames: audio_mix.queue_high_water_frames,
                endpoint_buffer_frames: renderer_info.buffer_frames,
                endpoint_epoch: renderer_info.endpoint_epoch,
                decoder_acceleration: self.decoder_acceleration,
                decoder_presentable_frames: decoder_info.presentable_frames,
                decoder_output_copies: decoder_info.output_copies,
                decoder_samples_received: decoder_info.mft_samples_received,
                decoder_samples_released: decoder_info.mft_samples_released,
            })
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

    fn pace_cycle(start: Instant, duration: Duration, cycle: usize, total_cycles: usize) {
        let fraction = cycle as f64 / total_cycles as f64;
        sleep_until(start + duration.mul_f64(fraction));
    }

    fn sleep_until(deadline: Instant) {
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            if remaining.is_zero() {
                break;
            }
            std::thread::sleep(remaining.min(Duration::from_millis(50)));
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

    fn find_video_track(index: &clipline_mp4::MovieIndex) -> AppResult<usize> {
        index
            .tracks
            .iter()
            .position(|track| matches!(track.config, PlaybackTrackConfig::H264 { .. }))
            .ok_or_else(|| invalid("fixture has no H.264 track"))
    }

    fn find_audio_tracks(index: &clipline_mp4::MovieIndex) -> Vec<usize> {
        index
            .tracks
            .iter()
            .enumerate()
            .filter_map(|(track_index, track)| {
                matches!(track.config, PlaybackTrackConfig::Opus { .. }).then_some(track_index)
            })
            .collect()
    }

    fn rescale_to_timeline(ticks: u64, timescale: u32) -> AppResult<u64> {
        if timescale == 0 {
            return Err(invalid("media timescale must be non-zero"));
        }
        let scaled = u128::from(ticks)
            .checked_mul(u128::from(PLAYBACK_TIMELINE_HZ))
            .and_then(|value| value.checked_div(u128::from(timescale)))
            .and_then(|value| u64::try_from(value).ok())
            .ok_or_else(|| invalid("media timestamp conversion overflow"))?;
        Ok(scaled)
    }

    fn timeline_position(pts: i64, timescale: u32) -> AppResult<TimelinePosition> {
        let pts = u64::try_from(pts).map_err(|_| invalid("negative video PTS is unsupported"))?;
        Ok(TimelinePosition::new(rescale_to_timeline(pts, timescale)?))
    }

    fn timeline_duration(duration: u32, timescale: u32) -> AppResult<TimelineDuration> {
        Ok(TimelineDuration::new(rescale_to_timeline(
            u64::from(duration),
            timescale,
        )?)?)
    }

    fn percentile_u16(values: &mut [u16], percentile: usize) -> Option<u16> {
        if values.is_empty() || !(1..=100).contains(&percentile) {
            return None;
        }
        values.sort_unstable();
        let rank = values.len().saturating_mul(percentile).saturating_add(99) / 100;
        values.get(rank.saturating_sub(1)).copied()
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

    fn is_terminal_playout(
        clock: TimelinePosition,
        video_end: TimelinePosition,
        endpoint_buffer_frames: usize,
    ) -> bool {
        video_end.ticks().saturating_sub(clock.ticks())
            <= u64::try_from(endpoint_buffer_frames).unwrap_or(u64::MAX)
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

    #[allow(dead_code)]
    fn _assert_metrics_type(_: &PlaybackMetrics) {}

    #[cfg(test)]
    mod tests {
        use super::{is_terminal_playout, merge_fail_closed_percentile, TimelinePosition};

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

        #[test]
        fn endpoint_empty_is_terminal_only_inside_the_final_buffer_window() {
            let end = TimelinePosition::new(240_000);
            assert!(!is_terminal_playout(
                TimelinePosition::new(238_943),
                end,
                1_056,
            ));
            assert!(is_terminal_playout(
                TimelinePosition::new(238_944),
                end,
                1_056,
            ));
            assert!(is_terminal_playout(
                TimelinePosition::new(240_001),
                end,
                1_056,
            ));
        }
    }
}
