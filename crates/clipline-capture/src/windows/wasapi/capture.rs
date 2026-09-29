use super::*;

/// Owns one successful `IAudioCaptureClient::GetBuffer` packet until it is
/// released back to WASAPI.
struct WasapiPacket {
    capture: IAudioCaptureClient,
    frames: u32,
    released: bool,
}

impl WasapiPacket {
    fn new(capture: &IAudioCaptureClient, frames: u32) -> Self {
        Self {
            capture: capture.clone(),
            frames,
            released: false,
        }
    }

    fn release(mut self) -> windows::core::Result<()> {
        self.released = true;
        // SAFETY: this guard is created only after a successful GetBuffer and
        // owns the matching frame count. Marking it released before the call
        // prevents Drop from attempting a second release if the API fails.
        unsafe { self.capture.ReleaseBuffer(self.frames) }
    }
}

impl Drop for WasapiPacket {
    fn drop(&mut self) {
        if !self.released {
            self.released = true;
            // SAFETY: this is the matching release for the successful
            // GetBuffer that created the guard. Drop makes validation errors
            // and unwinding release the packet exactly once.
            let _ = unsafe { self.capture.ReleaseBuffer(self.frames) };
        }
    }
}

/// `drain_device` distinguishes a dead endpoint (recover by re-activation)
/// from genuine capture-contract failures (stay fatal).
enum DrainFailure {
    Recoverable(HRESULT),
    Fatal(CaptureError),
}

impl From<windows::core::Error> for DrainFailure {
    fn from(error: windows::core::Error) -> Self {
        if wasapi_error_recoverable(error.code()) {
            Self::Recoverable(error.code())
        } else {
            Self::Fatal(CaptureError::DeviceLost(format!("WASAPI: {error}")))
        }
    }
}

struct WasapiPcmCapture {
    device: Option<ActivatedDevice>,
    clock: RelativeClock,
    channels: u16,
    sample_format: SampleFormat,
    mode: EndpointMode,
    diagnostic_source: String,
    target: EndpointTarget,
    volume: f32,
    level: AudioLevelAccumulator,
    resampler: Option<StereoResampler>,
    discontinuity_fade: DiscontinuityFade,
    packet_timeline: DevicePacketTimeline,
    reactivation: DeviceReactivation,
    last_device_hresult: i32,
    last_device_packet_at: Instant,
    assembler: LoopbackAssembler,
    queue: std::collections::VecDeque<PcmFrame>,
    discontinuity_diagnostics: DiagnosticRateLimiter,
    late_audio_diagnostics: DiagnosticRateLimiter,
    device_diagnostics: DiagnosticRateLimiter,
}

pub struct WasapiLoopback {
    pcm: WasapiPcmCapture,
    opus: OpusFrameEncoder,
    queue: Vec<AudioPacket>,
}

impl WasapiPcmCapture {
    fn start_output(
        clock: RelativeClock,
        device_id: Option<&str>,
        volume: f64,
    ) -> Result<Self, CaptureError> {
        Self::start(
            EndpointTarget::OutputLoopback {
                device_id: device_id.map(str::to_owned),
            },
            clock,
            volume,
        )
    }

    fn start_output_resilient(
        clock: RelativeClock,
        device_id: Option<&str>,
        volume: f64,
    ) -> Result<Self, CaptureError> {
        Self::start_resilient(
            EndpointTarget::OutputLoopback {
                device_id: device_id.map(str::to_owned),
            },
            clock,
            volume,
        )
    }

    fn start_microphone(
        clock: RelativeClock,
        device_id: Option<&str>,
        volume: f64,
        channels: WasapiChannelMode,
    ) -> Result<Self, CaptureError> {
        Self::start(
            EndpointTarget::Microphone {
                device_id: device_id.map(str::to_owned),
                channels,
            },
            clock,
            volume,
        )
    }

    fn start(target: EndpointTarget, clock: RelativeClock, volume: f64) -> Result<Self, CaptureError> {
        let device = target.activate()?;
        Ok(Self::from_target(target, Some(device), clock, volume))
    }

    fn start_resilient(
        target: EndpointTarget,
        clock: RelativeClock,
        volume: f64,
    ) -> Result<Self, CaptureError> {
        let device = match target.activate() {
            Ok(device) => Some(device),
            Err(CaptureError::DeviceLost(_)) => None,
            Err(error) => return Err(error),
        };
        Ok(Self::from_target(target, device, clock, volume))
    }

    fn from_target(
        target: EndpointTarget,
        device: Option<ActivatedDevice>,
        clock: RelativeClock,
        volume: f64,
    ) -> Self {
        // Anchor the audio timeline at the clock origin (recording
        // start): the gap fill turns any lead-in before the first
        // device buffer into silence, keeping the muxed track aligned
        // with video (both tracks start at t=0 in the file).
        let mut assembler = LoopbackAssembler::new();
        assembler.push_chunk(0.0, &[]);
        let mode = target.mode();
        let mix = device.as_ref().map(|device| device.mix);
        let mut reactivation = DeviceReactivation::new(DEVICE_REACTIVATION_RETRY_INTERVAL);
        if device.is_none() {
            reactivation.note_lost(Instant::now());
        }
        Self {
            device,
            clock,
            channels: mix.map_or(2, |mix| mix.channels),
            sample_format: mix.map_or(SampleFormat::Float32, |mix| mix.sample_format),
            mode,
            diagnostic_source: mode.diagnostic_label().into(),
            target,
            volume: (volume.clamp(0.0, 2.0)) as f32,
            level: AudioLevelAccumulator::default(),
            resampler: mix.and_then(|mix| {
                (mix.sample_rate != OPUS_SAMPLE_RATE)
                    .then(|| StereoResampler::new(mix.sample_rate, OPUS_SAMPLE_RATE))
            }),
            discontinuity_fade: DiscontinuityFade::new(),
            packet_timeline: DevicePacketTimeline::new(),
            reactivation,
            last_device_hresult: 0,
            last_device_packet_at: Instant::now(),
            assembler,
            queue: std::collections::VecDeque::new(),
            discontinuity_diagnostics: DiagnosticRateLimiter::new(Duration::from_secs(30)),
            late_audio_diagnostics: DiagnosticRateLimiter::new(Duration::from_secs(30)),
            device_diagnostics: DiagnosticRateLimiter::new(Duration::from_secs(30)),
        }
    }

    /// Swap in a freshly activated endpoint after device loss. The
    /// assembler and queues survive: synthesized silence covered the
    /// outage, and the next live packet re-anchors on its QPC timestamp.
    fn install_device(&mut self, device: ActivatedDevice) {
        let mix = device.mix;
        if let Some(previous) = self.device.take() {
            previous.stop();
        }
        self.device = Some(device);
        self.channels = mix.channels;
        self.sample_format = mix.sample_format;
        self.resampler = (mix.sample_rate != OPUS_SAMPLE_RATE)
            .then(|| StereoResampler::new(mix.sample_rate, OPUS_SAMPLE_RATE));
        self.discontinuity_fade.restart();
        self.packet_timeline.require_timestamp_anchor();
        self.last_device_packet_at = Instant::now();
    }

    /// Mark the endpoint dead after a recoverable WASAPI failure. The poll
    /// loop keeps running on synthesized silence until re-activation works.
    fn note_device_lost(&mut self, code: HRESULT) {
        let now = Instant::now();
        let first_failure = self.reactivation.note_lost(now);
        self.last_device_hresult = code.0;
        if first_failure {
            // Prime the limiter so the immediate report is not duplicated.
            let _ = self.device_diagnostics.observe(now);
            emit_diagnostic(CaptureDiagnostic::WasapiDeviceLost {
                source: self.diagnostic_source.clone(),
                hresult: code.0,
                suppressed_since_last: 0,
            });
        } else if let Some(suppressed_since_last) = self.device_diagnostics.observe(now) {
            emit_diagnostic(CaptureDiagnostic::WasapiDeviceLost {
                source: self.diagnostic_source.clone(),
                hresult: code.0,
                suppressed_since_last,
            });
        }
    }

    fn retry_device_if_due(&mut self, now: Instant) {
        if !self.reactivation.retry_due(now) {
            return;
        }
        match self.target.activate() {
            Ok(device) => {
                let recovered_at = Instant::now();
                let outage = self.reactivation.note_recovered(recovered_at);
                self.install_device(device);
                emit_diagnostic(CaptureDiagnostic::WasapiDeviceRecovered {
                    source: self.diagnostic_source.clone(),
                    outage_ms: outage.map_or(0, |outage| outage.as_millis() as u64),
                });
            }
            Err(_) => {
                let failed_at = Instant::now();
                self.reactivation.note_retry_failed(failed_at);
                if let Some(suppressed_since_last) = self.device_diagnostics.observe(failed_at) {
                    emit_diagnostic(CaptureDiagnostic::WasapiDeviceLost {
                        source: self.diagnostic_source.clone(),
                        hresult: self.last_device_hresult,
                        suppressed_since_last,
                    });
                }
            }
        }
    }

    pub fn take_level(&mut self) -> AudioLevel {
        self.level.take()
    }

    fn decode_samples(&self, data: *const u8, frames: u32) -> Result<Vec<f32>, CaptureError> {
        let sample_count = (frames as usize)
            .checked_mul(self.channels as usize)
            .ok_or_else(|| CaptureError::DeviceLost("WASAPI sample count overflow".into()))?;
        let byte_len = sample_count
            .checked_mul(self.sample_format.bytes_per_sample())
            .ok_or_else(|| CaptureError::DeviceLost("WASAPI buffer size overflow".into()))?;
        if byte_len == 0 {
            return Ok(Vec::new());
        }
        if data.is_null() {
            return Err(CaptureError::DeviceLost(
                "WASAPI returned a null non-silent buffer".into(),
            ));
        }
        // SAFETY: GetBuffer guarantees `byte_len` readable bytes until
        // ReleaseBuffer. A u8 slice has alignment one; typed decoding below
        // copies fixed-size little-endian arrays and never assumes alignment.
        let bytes = unsafe { std::slice::from_raw_parts(data, byte_len) };
        decode_sample_bytes(bytes, self.sample_format, sample_count)
            .map_err(|message| CaptureError::DeviceLost(message.into()))
    }

    fn stereo_samples(&mut self, samples: &[f32]) -> Vec<f32> {
        let mut stereo = match self.mode {
            EndpointMode::OutputLoopback
            | EndpointMode::InputCapture(WasapiChannelMode::Stereo) => {
                extract_stereo(samples, self.channels)
            }
            EndpointMode::InputCapture(WasapiChannelMode::Mono) => {
                extract_mono_centered(samples, self.channels)
            }
        };
        if let Some(resampler) = &mut self.resampler {
            stereo = resampler.resample(&stereo);
        }
        apply_gain(&mut stereo, self.volume);
        stereo
    }

    fn push_timed_stereo(&mut self, pts_s: f64, stereo: &[f32]) {
        let outcome = self.assembler.push_chunk(pts_s, stereo);
        if let Some(correction_s) = outcome.late_reanchor_s {
            if let Some(suppressed_since_last) = self.late_audio_diagnostics.observe(Instant::now())
            {
                emit_diagnostic(CaptureDiagnostic::WasapiLateAudioReanchored {
                    source: self.diagnostic_source.clone(),
                    correction_ms: (correction_s * 1_000.0).round() as u64,
                    total_correction_ms: (outcome.total_correction_s * 1_000.0).round() as u64,
                    chunk_ms: (outcome.chunk_duration_s * 1_000.0).round() as u64,
                    suppressed_since_last,
                });
            }
        }
    }

    /// Drain everything the device has buffered into the assembler. A
    /// recoverable endpoint loss marks the device dead and returns `Ok`:
    /// the caller's silence fill covers the outage until re-activation.
    fn drain_device(&mut self) -> Result<(), CaptureError> {
        match self.drain_available_packets() {
            Ok(()) => Ok(()),
            Err(DrainFailure::Recoverable(code)) => {
                self.note_device_lost(code);
                Ok(())
            }
            Err(DrainFailure::Fatal(error)) => Err(error),
        }
    }

    fn drain_available_packets(&mut self) -> Result<(), DrainFailure> {
        let Some(capture) = self.device.as_ref().map(|device| device.capture.clone()) else {
            return Ok(());
        };
        // SAFETY: GetBuffer/ReleaseBuffer pairs per the capture-client
        // contract; the data pointer is valid for `frames` frames until
        // ReleaseBuffer.
        unsafe {
            while capture.GetNextPacketSize()? > 0 {
                self.last_device_packet_at = Instant::now();
                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                let mut qpc_100ns = 0u64;
                capture.GetBuffer(
                    &mut data,
                    &mut frames,
                    &mut flags,
                    None,
                    Some(&mut qpc_100ns),
                )?;
                let packet = WasapiPacket::new(&capture, frames);
                let timestamp_valid = wasapi_timestamp_valid(flags);
                let data_discontinuous = wasapi_data_discontinuous(flags);
                let pts_s = timestamp_valid.then(|| self.clock.pts_s(qpc_100ns as i64));
                let sample_count = (frames as usize)
                    .checked_mul(self.channels as usize)
                    .ok_or_else(|| CaptureError::DeviceLost("WASAPI sample count overflow".into()));
                let samples = if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 {
                    sample_count.map(|count| vec![0.0; count])
                } else {
                    self.decode_samples(data as *const u8, frames)
                };
                packet.release()?;
                let samples = samples.map_err(DrainFailure::Fatal)?;
                let mut stereo = self.stereo_samples(&samples);
                if data_discontinuous {
                    self.discontinuity_fade.restart();
                    self.packet_timeline.require_timestamp_anchor();
                }
                self.discontinuity_fade.apply(&mut stereo);
                self.level.add(&stereo);
                match self.packet_timeline.placement(pts_s) {
                    DevicePacketPlacement::Timestamped(anchor_pts_s) => {
                        self.push_timed_stereo(anchor_pts_s, &stereo);
                    }
                    DevicePacketPlacement::Contiguous => {
                        self.assembler.push_contiguous_chunk(&stereo);
                    }
                }
                if data_discontinuous {
                    if let Some(suppressed_since_last) =
                        self.discontinuity_diagnostics.observe(Instant::now())
                    {
                        emit_diagnostic(CaptureDiagnostic::WasapiDataDiscontinuity {
                            source: self.diagnostic_source.clone(),
                            suppressed_since_last,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    fn collect_frames(
        &mut self,
        until_pts_s: f64,
        synthesize_silence: bool,
    ) -> Result<Vec<PcmFrame>, CaptureError> {
        self.retry_device_if_due(Instant::now());
        if self.reactivation.is_live() {
            self.drain_device()?;
        }
        if synthesize_silence {
            if let Some(horizon_pts_s) = audio_poll_silence_horizon(until_pts_s) {
                let idle_s = self.last_device_packet_at.elapsed().as_secs_f64();
                if self.packet_timeline.note_synthesized_silence(idle_s) {
                    self.assembler.advance_with_silence(horizon_pts_s);
                }
            }
        }
        while let Some(frame) = self.assembler.pop_frame() {
            self.queue.push_back(frame);
        }
        let split = self
            .queue
            .iter()
            .position(|(pts_s, _)| pts_s + FRAME_DURATION_S > until_pts_s + 1e-9)
            .unwrap_or(self.queue.len());
        Ok(self.queue.drain(..split).collect())
    }

    fn poll_frames(&mut self, until_pts_s: f64) -> Result<Vec<PcmFrame>, CaptureError> {
        self.collect_frames(until_pts_s, true)
    }

    fn finish_frames(&mut self, until_pts_s: f64) -> Result<Vec<PcmFrame>, CaptureError> {
        self.collect_frames(until_pts_s, false)
    }
}

impl Drop for WasapiPcmCapture {
    fn drop(&mut self) {
        if let Some(device) = self.device.take() {
            device.stop();
        }
    }
}

impl WasapiLoopback {
    /// Start capturing the default render endpoint in loopback. `clock`
    /// maps the buffers' QPC positions onto the recording timeline — pass
    /// the same origin the video capture uses.
    pub fn start(clock: RelativeClock) -> Result<Self, CaptureError> {
        Self::start_output(clock, None, 1.0)
    }

    pub fn start_output(
        clock: RelativeClock,
        device_id: Option<&str>,
        volume: f64,
    ) -> Result<Self, CaptureError> {
        Self::from_pcm(WasapiPcmCapture::start_output(clock, device_id, volume)?)
    }

    pub fn start_output_resilient(
        clock: RelativeClock,
        device_id: Option<&str>,
        volume: f64,
    ) -> Result<Self, CaptureError> {
        Self::from_pcm(WasapiPcmCapture::start_output_resilient(
            clock, device_id, volume,
        )?)
    }

    pub fn start_microphone(
        clock: RelativeClock,
        device_id: Option<&str>,
        volume: f64,
        channels: WasapiChannelMode,
    ) -> Result<Self, CaptureError> {
        Self::from_pcm(WasapiPcmCapture::start_microphone(
            clock, device_id, volume, channels,
        )?)
    }

    fn from_pcm(pcm: WasapiPcmCapture) -> Result<Self, CaptureError> {
        Ok(Self {
            pcm,
            opus: OpusFrameEncoder::new().map_err(|e| CaptureError::Init(format!("opus: {e}")))?,
            queue: Vec::new(),
        })
    }

    pub fn take_level(&mut self) -> AudioLevel {
        self.pcm.take_level()
    }

    pub fn is_live(&self) -> bool {
        self.pcm.reactivation.is_live()
    }

    pub fn with_diagnostic_source(mut self, source: impl Into<String>) -> Self {
        self.pcm.diagnostic_source = source.into();
        self
    }

    pub fn poll_monitor_chunk(&mut self) -> Result<WasapiMonitorChunk, CaptureError> {
        let samples = self
            .pcm
            .poll_frames(f64::MAX)?
            .into_iter()
            .flat_map(|(_, frame)| frame)
            .collect();
        Ok(WasapiMonitorChunk {
            level: self.pcm.take_level(),
            samples,
        })
    }

    fn encode_frames(&mut self, frames: Vec<PcmFrame>) -> Result<(), CaptureError> {
        for (pts_s, frame) in frames {
            let data = self
                .opus
                .encode_frame(&frame)
                .map_err(|e| CaptureError::DeviceLost(format!("opus encode: {e}")))?;
            self.queue.push(AudioPacket {
                data,
                pts_s,
                duration_s: FRAME_DURATION_S,
            });
        }
        Ok(())
    }

    fn take_packets_until(&mut self, until_pts_s: f64) -> Vec<AudioPacket> {
        let split = self
            .queue
            .iter()
            .position(|packet| packet.pts_s + packet.duration_s > until_pts_s + 1e-9)
            .unwrap_or(self.queue.len());
        self.queue.drain(..split).collect()
    }
}

impl AudioSource for WasapiLoopback {
    fn poll_packets(&mut self, until_pts_s: f64) -> Result<Vec<AudioPacket>, CaptureError> {
        let frames = self.pcm.poll_frames(until_pts_s)?;
        self.encode_frames(frames)?;
        Ok(self.take_packets_until(until_pts_s))
    }

    fn finish_packets(&mut self, until_pts_s: f64) -> Result<Vec<AudioPacket>, CaptureError> {
        std::thread::sleep(Duration::from_secs_f64(TERMINAL_AUDIO_DRAIN_S));
        let frames = self.pcm.finish_frames(until_pts_s)?;
        self.encode_frames(frames)?;
        Ok(self.take_packets_until(until_pts_s))
    }

    fn track_config(&self) -> AudioTrackConfig {
        self.opus.track_config()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::RelativeClock;
    use crate::traits::AudioSource;
    use windows::Win32::Foundation::E_FAIL;

    /// Real loopback against the default render endpoint. CI-skipped (no
    /// audio endpoint on runners); lenient about an idle/silent desktop —
    /// the assembler's gap fill makes silence a valid outcome.
    #[test]
    fn captures_system_loopback_audio() {
        if std::env::var_os("CI").is_some() {
            eprintln!("SKIP: audio endpoint test");
            return;
        }
        let clock = RelativeClock::new(crate::windows::qpc_now_ticks_100ns().unwrap());
        let mut src = match WasapiLoopback::start(clock) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("SKIP: loopback unavailable: {e}");
                return;
            }
        };
        let cfg = src.track_config();
        assert_eq!((cfg.channels, cfg.sample_rate), (2, 48_000));
        assert!(cfg.pre_skip > 0);
        std::thread::sleep(std::time::Duration::from_millis(300));
        let packets = src.poll_packets(f64::MAX).expect("poll");
        for w in packets.windows(2) {
            assert!(
                (w[1].pts_s - w[0].pts_s - 0.02).abs() < 1e-6,
                "20 ms cadence"
            );
        }

        for p in &packets {
            assert!(!p.data.is_empty());
        }
        eprintln!("captured {} opus packets in 300 ms", packets.len());
    }

    #[test]
    fn drain_failure_maps_recoverable_hresults() {
        let recoverable = DrainFailure::from(windows::core::Error::from_hresult(
            AUDCLNT_E_DEVICE_INVALIDATED,
        ));
        assert!(matches!(
            recoverable,
            DrainFailure::Recoverable(code) if code == AUDCLNT_E_DEVICE_INVALIDATED
        ));
        let fatal = DrainFailure::from(windows::core::Error::from_hresult(E_FAIL));
        assert!(matches!(fatal, DrainFailure::Fatal(_)));
    }

    #[test]
    fn unavailable_explicit_output_starts_dormant_without_default_fallback() {
        if std::env::var_os("CI").is_some() {
            eprintln!("SKIP: audio endpoint test");
            return;
        }
        let clock = RelativeClock::new(crate::windows::qpc_now_ticks_100ns().unwrap());
        let mut source = WasapiLoopback::start_output_resilient(
            clock,
            Some("clipline-missing-playback-endpoint"),
            1.0,
        )
        .expect("missing endpoint becomes a dormant source");

        assert!(!source.is_live(), "missing endpoint must not capture default");
        std::thread::sleep(Duration::from_millis(120));
        assert!(
            !source.poll_packets(0.2).unwrap().is_empty(),
            "dormant source must preserve its timeline with encoded silence"
        );
    }

    /// Simulated endpoint invalidation: polls must keep succeeding (the
    /// outage rides on the silence-fill path) and the retry must swap in a
    /// freshly activated endpoint once the retry interval elapses.
    #[test]
    fn device_loss_recovers_via_reactivation() {
        if std::env::var_os("CI").is_some() {
            eprintln!("SKIP: audio endpoint test");
            return;
        }
        let clock = RelativeClock::new(crate::windows::qpc_now_ticks_100ns().unwrap());
        let mut pcm = match WasapiPcmCapture::start_output(clock, None, 1.0) {
            Ok(pcm) => pcm,
            Err(e) => {
                eprintln!("SKIP: loopback unavailable: {e}");
                return;
            }
        };
        pcm.poll_frames(f64::MAX).expect("baseline poll");
        assert!(pcm.reactivation.is_live());

        pcm.note_device_lost(AUDCLNT_E_DEVICE_INVALIDATED);
        assert!(!pcm.reactivation.is_live());
        pcm.poll_frames(1.0)
            .expect("poll during outage must not error");
        assert!(
            !pcm.reactivation.is_live(),
            "retry interval has not elapsed"
        );

        std::thread::sleep(DEVICE_REACTIVATION_RETRY_INTERVAL + Duration::from_millis(200));
        pcm.poll_frames(2.0).expect("retry poll");
        assert!(
            pcm.reactivation.is_live(),
            "retry must re-activate the endpoint"
        );

        // The fresh endpoint keeps draining without error.
        std::thread::sleep(Duration::from_millis(300));
        pcm.poll_frames(f64::MAX).expect("post-recovery poll");
    }
}
