//! Bounded shared-mode WASAPI playback.
//!
//! The renderer accepts only Clipline's 48 kHz stereo-f32 mix format and lets
//! the Windows shared audio engine perform at most one endpoint conversion.
//! It never owns a producer queue: callers write directly into the bounded
//! endpoint buffer and receive immediate backpressure when that buffer is full.

use std::ffi::c_void;
use std::ptr::{addr_of, copy_nonoverlapping};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioClient, IAudioClient3, IAudioClock, IAudioRenderClient,
    IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_E_RESOURCES_INVALIDATED, AUDCLNT_E_SERVICE_NOT_RUNNING,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};
use windows_core::{Error as WindowsError, Interface, PWSTR};

use super::com::ComApartment;
use crate::{
    AudioRenderer, AudioRendererInfo, AudioSampleFormat, BackendComponent, BackendError,
    BackendErrorKind, PipelineToken, RawAudioClock, RecoveryDisposition, MAX_AUDIO_WRITE_FRAMES,
    PLAYBACK_TIMELINE_HZ,
};

const LOGICAL_CHANNELS: u16 = 2;
const BITS_PER_SAMPLE: u16 = 32;
const BYTES_PER_FRAME: u16 = LOGICAL_CHANNELS * (BITS_PER_SAMPLE / 8);
const HUNDRED_NS_PER_SECOND: u64 = 10_000_000;
const LEGACY_BUFFER_DURATION_100NS: i64 = 1_000_000;
const MAX_ENDPOINT_BUFFER_FRAMES: usize = PLAYBACK_TIMELINE_HZ as usize;
const DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(2);

static NEXT_ENDPOINT_EPOCH: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WasapiInitializationPath {
    AudioClient3,
    LegacySharedAutoConvert,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasapiRendererTelemetry {
    endpoint_id: String,
    device_format: String,
    pub device_sample_rate: u32,
    pub device_channels: u16,
    pub device_bits_per_sample: u16,
    pub device_valid_bits_per_sample: Option<u16>,
    pub device_channel_mask: Option<u32>,
    pub device_buffer_duration_100ns: u64,
    pub conversion_active: bool,
    pub initialization_path: WasapiInitializationPath,
    pub endpoint_epoch: u64,
    pub buffer_frames: usize,
    pub engine_period_frames: usize,
    pub clock_frequency: u64,
    pub underruns: u64,
    pub underrun_frames: u64,
    pub frames_written: u64,
    pub max_frames_written_per_call: usize,
    pub recovery_count: u64,
}

impl WasapiRendererTelemetry {
    pub fn endpoint_id(&self) -> &str {
        &self.endpoint_id
    }

    pub fn device_format(&self) -> &str {
        &self.device_format
    }
}

struct Endpoint {
    client: IAudioClient,
    render: IAudioRenderClient,
    clock: IAudioClock,
    volume: ISimpleAudioVolume,
    buffer_frames: usize,
    clock_frequency: u64,
    started: bool,
    submitted_since_empty: bool,
    underrun_latched: bool,
}

impl Endpoint {
    fn stop_best_effort(&mut self) {
        if self.started {
            // SAFETY: the client is initialized and owned by this thread.
            let _ = unsafe { self.client.Stop() };
            self.started = false;
        }
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.stop_best_effort();
    }
}

struct ActivatedEndpoint {
    endpoint: Endpoint,
    endpoint_id: String,
    device_format: String,
    device_sample_rate: u32,
    device_channels: u16,
    device_bits_per_sample: u16,
    device_valid_bits_per_sample: Option<u16>,
    device_channel_mask: Option<u32>,
    device_buffer_duration_100ns: u64,
    conversion_active: bool,
    initialization_path: WasapiInitializationPath,
    engine_period_frames: usize,
}

/// Thread-affine, safe owner of one default shared-mode render endpoint.
pub struct WindowsWasapiRenderer {
    endpoint: Option<Endpoint>,
    telemetry: WasapiRendererTelemetry,
    active_token: Option<PipelineToken>,
    volume: f32,
    _apartment: ComApartment,
}

impl WindowsWasapiRenderer {
    pub fn open_default() -> Result<Self, BackendError> {
        let apartment = ComApartment::multithreaded()
            .map_err(|error| unavailable_error("initialize the playback COM apartment", &error))?;
        let epoch = allocate_endpoint_epoch()?;
        let activated = activate_default_endpoint()
            .map_err(|error| operation_error("open the default render endpoint", &error))?;
        let telemetry = telemetry_for_activation(&activated, epoch, None, 0);

        Ok(Self {
            endpoint: Some(activated.endpoint),
            telemetry,
            active_token: None,
            volume: 1.0,
            _apartment: apartment,
        })
    }

    pub fn telemetry(&self) -> WasapiRendererTelemetry {
        self.telemetry.clone()
    }

    /// Check whether Windows moved the healthy default render role to a
    /// different endpoint. Invalidation HRESULTs alone do not cover this
    /// user-visible device-switch case.
    pub fn default_endpoint_changed(&self) -> Result<bool, BackendError> {
        current_default_render_endpoint_id()
            .map(|current| current != self.telemetry.endpoint_id)
            .map_err(|error| operation_error("query the default render endpoint", &error))
    }

    /// Recreates only the audio endpoint and assigns it a process-unique epoch.
    /// Timeline rebasing remains the neutral scheduler's responsibility.
    pub fn reopen(&mut self, token: PipelineToken) -> Result<(), BackendError> {
        self.endpoint.take();
        self.active_token = None;
        let epoch = allocate_endpoint_epoch()?;
        let activated = activate_default_endpoint()
            .map_err(|error| operation_error("reopen the default render endpoint", &error))?;

        // Apply the last accepted volume before making the replacement visible.
        // SAFETY: the endpoint is initialized and the volume service is valid.
        unsafe {
            activated
                .endpoint
                .volume
                .SetMasterVolume(self.volume, std::ptr::null())
        }
        .map_err(|error| operation_error("restore playback volume", &error))?;

        let recovery_count = self
            .telemetry
            .recovery_count
            .checked_add(1)
            .ok_or_else(|| fatal_error("WASAPI recovery counter overflow"))?;
        self.telemetry =
            telemetry_for_activation(&activated, epoch, Some(&self.telemetry), recovery_count);
        self.endpoint = Some(activated.endpoint);
        self.active_token = Some(token);
        Ok(())
    }

    /// Synchronous bounded drain for terminal tools and device tests.
    /// The playback worker uses non-blocking padding checks instead.
    pub fn drain(&mut self, token: PipelineToken, timeout: Duration) -> Result<bool, BackendError> {
        self.require_token(token)?;
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| fatal_error("WASAPI drain timeout overflow"))?;
        loop {
            let padding_result = {
                let endpoint = self.endpoint_ref()?;
                // SAFETY: the initialized client remains alive for this call.
                unsafe { endpoint.client.GetCurrentPadding() }
            };
            let padding = self.finish_endpoint_result(padding_result, "drain endpoint padding")?;
            if padding == 0 {
                return Ok(true);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            std::thread::sleep(DRAIN_POLL_INTERVAL.min(deadline.duration_since(now)));
        }
    }

    fn endpoint_ref(&self) -> Result<&Endpoint, BackendError> {
        self.endpoint
            .as_ref()
            .ok_or_else(|| unavailable_message("WASAPI render endpoint is closed"))
    }

    fn require_token(&self, token: PipelineToken) -> Result<(), BackendError> {
        if self.active_token == Some(token) {
            Ok(())
        } else {
            Err(stale_error("stale playback token for WASAPI endpoint"))
        }
    }

    fn finish_endpoint_result<T>(
        &mut self,
        result: windows_core::Result<T>,
        operation: &'static str,
    ) -> Result<T, BackendError> {
        match result {
            Ok(value) => Ok(value),
            Err(error) => {
                let classified = operation_error(operation, &error);
                // A failed call may have partially changed client state. Drop
                // every COM reference before returning either a recoverable or
                // terminal typed error; only `reopen` can make it usable again.
                self.endpoint.take();
                self.active_token = None;
                Err(classified)
            }
        }
    }

    fn record_empty_padding(&mut self, padding: usize) -> Result<(), BackendError> {
        let Some(endpoint) = self.endpoint.as_mut() else {
            return Err(unavailable_message("WASAPI render endpoint is closed"));
        };
        if endpoint.started
            && padding == 0
            && endpoint.submitted_since_empty
            && !endpoint.underrun_latched
        {
            endpoint.underrun_latched = true;
            endpoint.submitted_since_empty = false;
            let underruns = self
                .telemetry
                .underruns
                .checked_add(1)
                .ok_or_else(|| fatal_error("WASAPI underrun counter overflow"))?;
            let underrun_frames = self
                .telemetry
                .underrun_frames
                .checked_add(self.telemetry.engine_period_frames as u64)
                .ok_or_else(|| fatal_error("WASAPI underrun frame counter overflow"))?;
            self.telemetry.underruns = underruns;
            self.telemetry.underrun_frames = underrun_frames;
        }
        Ok(())
    }
}

impl AudioRenderer for WindowsWasapiRenderer {
    fn info(&self) -> AudioRendererInfo {
        AudioRendererInfo {
            sample_rate: PLAYBACK_TIMELINE_HZ,
            channels: LOGICAL_CHANNELS,
            sample_format: AudioSampleFormat::F32,
            buffer_frames: self.telemetry.buffer_frames,
            endpoint_epoch: self.telemetry.endpoint_epoch,
        }
    }

    fn reset(&mut self, token: PipelineToken) -> Result<(), BackendError> {
        let result = {
            let endpoint = self.endpoint_ref()?;
            // SAFETY: Stop/Reset are the documented seek/flush sequence for
            // an initialized shared-mode client with no outstanding buffer.
            unsafe {
                endpoint
                    .client
                    .Stop()
                    .and_then(|()| endpoint.client.Reset())
            }
        };
        self.finish_endpoint_result(result, "reset render endpoint")?;
        let endpoint = self
            .endpoint
            .as_mut()
            .expect("successful endpoint operation retains the endpoint");
        endpoint.started = false;
        endpoint.submitted_since_empty = false;
        endpoint.underrun_latched = false;
        self.active_token = Some(token);
        Ok(())
    }

    fn start(&mut self, token: PipelineToken) -> Result<(), BackendError> {
        self.require_token(token)?;
        if self.endpoint_ref()?.started {
            return Ok(());
        }
        let result = {
            let endpoint = self.endpoint_ref()?;
            // SAFETY: client is initialized and currently stopped.
            unsafe { endpoint.client.Start() }
        };
        self.finish_endpoint_result(result, "start render endpoint")?;
        self.endpoint
            .as_mut()
            .expect("successful endpoint operation retains the endpoint")
            .started = true;
        Ok(())
    }

    fn pause(&mut self, token: PipelineToken) -> Result<(), BackendError> {
        self.require_token(token)?;
        if !self.endpoint_ref()?.started {
            return Ok(());
        }
        let result = {
            let endpoint = self.endpoint_ref()?;
            // SAFETY: client is initialized; Stop is idempotent for a stopped
            // shared-mode stream and preserves its raw clock position.
            unsafe { endpoint.client.Stop() }
        };
        self.finish_endpoint_result(result, "pause render endpoint")?;
        self.endpoint
            .as_mut()
            .expect("successful endpoint operation retains the endpoint")
            .started = false;
        Ok(())
    }

    fn set_volume(&mut self, volume: f32) -> Result<(), BackendError> {
        if !volume.is_finite() || !(0.0..=1.0).contains(&volume) {
            return Err(fatal_error(
                "playback volume must be finite and between zero and one",
            ));
        }
        let result = {
            let endpoint = self.endpoint_ref()?;
            // SAFETY: volume is validated and the session volume service is
            // owned by the initialized endpoint.
            unsafe { endpoint.volume.SetMasterVolume(volume, std::ptr::null()) }
        };
        self.finish_endpoint_result(result, "set playback volume")?;
        self.volume = volume;
        Ok(())
    }

    fn writable_frames(&mut self) -> Result<usize, BackendError> {
        let padding_result = {
            let endpoint = self.endpoint_ref()?;
            // SAFETY: the initialized client remains alive for this call.
            unsafe { endpoint.client.GetCurrentPadding() }
        };
        let padding = self.finish_endpoint_result(padding_result, "query render padding")? as usize;
        let buffer_frames = self.endpoint_ref()?.buffer_frames;
        if padding > buffer_frames {
            return Err(fatal_error("WASAPI padding exceeded the negotiated buffer"));
        }
        self.record_empty_padding(padding)?;
        Ok(buffer_frames - padding)
    }

    fn write_stereo_frames(
        &mut self,
        pcm: &[f32],
        token: PipelineToken,
    ) -> Result<usize, BackendError> {
        self.require_token(token)?;
        if !pcm.len().is_multiple_of(LOGICAL_CHANNELS as usize) {
            return Err(corrupt_error(
                "WASAPI PCM must contain complete stereo frames",
            ));
        }
        if pcm.iter().any(|sample| !sample.is_finite()) {
            return Err(corrupt_error("WASAPI PCM samples must be finite"));
        }

        let requested_frames = pcm.len() / LOGICAL_CHANNELS as usize;
        if requested_frames == 0 {
            return Ok(0);
        }
        let writable = self.writable_frames()?;
        let accepted = requested_frames.min(writable).min(MAX_AUDIO_WRITE_FRAMES);
        if accepted == 0 {
            return Ok(0);
        }

        let render = self.endpoint_ref()?.render.clone();
        let packet_result = RenderPacket::acquire(render, accepted as u32)
            .and_then(|packet| packet.copy_from(&pcm[..accepted * LOGICAL_CHANNELS as usize]));
        self.finish_endpoint_result(packet_result, "write playback samples")?;

        let frames_written = self
            .telemetry
            .frames_written
            .checked_add(accepted as u64)
            .ok_or_else(|| fatal_error("WASAPI written-frame counter overflow"))?;
        let max_frames_written_per_call = self.telemetry.max_frames_written_per_call.max(accepted);
        let endpoint = self
            .endpoint
            .as_mut()
            .expect("successful endpoint operation retains the endpoint");
        endpoint.submitted_since_empty = true;
        endpoint.underrun_latched = false;
        self.telemetry.frames_written = frames_written;
        self.telemetry.max_frames_written_per_call = max_frames_written_per_call;
        Ok(accepted)
    }

    fn raw_clock(&mut self) -> Result<RawAudioClock, BackendError> {
        let (clock, frequency) = {
            let endpoint = self.endpoint_ref()?;
            (endpoint.clock.clone(), endpoint.clock_frequency)
        };
        let mut position = 0u64;
        // SAFETY: position points to writable storage and the clock interface
        // remains alive for this call. QPC is not required by the neutral seam.
        let result = unsafe { clock.GetPosition(&mut position, None) };
        self.finish_endpoint_result(result, "sample render clock")?;
        RawAudioClock::new(position, frequency, self.telemetry.endpoint_epoch)
            .map_err(|error| fatal_error(&error.to_string()))
    }

    fn close(&mut self) {
        self.endpoint.take();
        self.active_token = None;
    }
}

struct RenderPacket {
    render: IAudioRenderClient,
    bytes: *mut u8,
    frames: u32,
    released: bool,
}

impl RenderPacket {
    fn acquire(render: IAudioRenderClient, frames: u32) -> windows_core::Result<Self> {
        // SAFETY: the caller has already bounded frames by available padding.
        let bytes = unsafe { render.GetBuffer(frames)? };
        Ok(Self {
            render,
            bytes,
            frames,
            released: false,
        })
    }

    fn copy_from(mut self, pcm: &[f32]) -> windows_core::Result<()> {
        debug_assert_eq!(pcm.len(), self.frames as usize * LOGICAL_CHANNELS as usize);
        // SAFETY: GetBuffer returned storage for exactly `frames` logical
        // stereo-f32 frames; the validated slice has the matching length and
        // does not overlap endpoint storage.
        unsafe {
            copy_nonoverlapping(pcm.as_ptr(), self.bytes.cast::<f32>(), pcm.len());
        }
        self.released = true;
        // SAFETY: exactly matches the successful GetBuffer owned by this guard.
        unsafe { self.render.ReleaseBuffer(self.frames, 0) }
    }
}

impl Drop for RenderPacket {
    fn drop(&mut self) {
        if !self.released {
            self.released = true;
            // SAFETY: balances the successful GetBuffer if validation or
            // unwinding prevents normal publication. SILENT avoids exposing
            // uninitialized endpoint memory.
            let _ = unsafe {
                self.render
                    .ReleaseBuffer(self.frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)
            };
        }
    }
}

struct CoTaskWaveFormat(*mut WAVEFORMATEX);

struct DeviceMixFormat {
    label: String,
    sample_rate: u32,
    channels: u16,
    bits_per_sample: u16,
    valid_bits_per_sample: Option<u16>,
    channel_mask: Option<u32>,
    is_logical_float: bool,
}

impl CoTaskWaveFormat {
    fn new(format: *mut WAVEFORMATEX) -> windows_core::Result<Self> {
        if format.is_null() {
            Err(validation_windows_error(
                "GetMixFormat returned a null format",
            ))
        } else {
            Ok(Self(format))
        }
    }

    fn describe(&self) -> DeviceMixFormat {
        // SAFETY: GetMixFormat returned a complete WAVEFORMATEX allocation.
        // All fields are copied with read_unaligned because windows-rs models
        // the structure packed. An extensible cast is made only when cbSize
        // proves that the task allocation contains its 22-byte extension.
        unsafe {
            let tag = addr_of!((*self.0).wFormatTag).read_unaligned();
            let sample_rate = addr_of!((*self.0).nSamplesPerSec).read_unaligned();
            let channels = addr_of!((*self.0).nChannels).read_unaligned();
            let bits_per_sample = addr_of!((*self.0).wBitsPerSample).read_unaligned();
            let extension_size = addr_of!((*self.0).cbSize).read_unaligned();
            let mut valid_bits_per_sample = None;
            let mut channel_mask = None;
            let mut subtype = None;

            if u32::from(tag) == WAVE_FORMAT_EXTENSIBLE
                && extension_size as usize
                    >= std::mem::size_of::<WAVEFORMATEXTENSIBLE>()
                        - std::mem::size_of::<WAVEFORMATEX>()
            {
                let extensible = self.0.cast::<WAVEFORMATEXTENSIBLE>();
                valid_bits_per_sample =
                    Some(addr_of!((*extensible).Samples.wValidBitsPerSample).read_unaligned());
                channel_mask = Some(addr_of!((*extensible).dwChannelMask).read_unaligned());
                subtype = Some(addr_of!((*extensible).SubFormat).read_unaligned());
            }

            let label = match (u32::from(tag), subtype) {
                (WAVE_FORMAT_IEEE_FLOAT, _) => "ieee-float".to_owned(),
                (WAVE_FORMAT_EXTENSIBLE, Some(KSDATAFORMAT_SUBTYPE_IEEE_FLOAT)) => {
                    "extensible-ieee-float".to_owned()
                }
                (WAVE_FORMAT_EXTENSIBLE, Some(KSDATAFORMAT_SUBTYPE_PCM)) => {
                    "extensible-pcm".to_owned()
                }
                (WAVE_FORMAT_EXTENSIBLE, Some(subtype)) => {
                    format!("extensible-{subtype:?}")
                }
                (WAVE_FORMAT_EXTENSIBLE, None) => "invalid-extensible".to_owned(),
                (tag, _) => format!("wave-tag-{tag}"),
            };
            let is_float = u32::from(tag) == WAVE_FORMAT_IEEE_FLOAT
                || subtype == Some(KSDATAFORMAT_SUBTYPE_IEEE_FLOAT);

            DeviceMixFormat {
                label,
                sample_rate,
                channels,
                bits_per_sample,
                valid_bits_per_sample,
                channel_mask,
                is_logical_float: is_float && bits_per_sample == BITS_PER_SAMPLE,
            }
        }
    }
}

impl Drop for CoTaskWaveFormat {
    fn drop(&mut self) {
        // SAFETY: GetMixFormat transfers this CoTaskMem allocation to caller.
        unsafe { CoTaskMemFree(Some(self.0.cast::<c_void>())) };
    }
}

struct CoTaskString(PWSTR);

impl CoTaskString {
    fn new(value: PWSTR) -> windows_core::Result<Self> {
        if value.is_null() {
            Err(validation_windows_error(
                "IMMDevice::GetId returned a null string",
            ))
        } else {
            Ok(Self(value))
        }
    }

    fn to_string(&self) -> windows_core::Result<String> {
        // SAFETY: IMMDevice::GetId returns a null-terminated task allocation.
        unsafe { self.0.to_string() }
            .map_err(|_| validation_windows_error("IMMDevice::GetId returned invalid UTF-16"))
    }
}

impl Drop for CoTaskString {
    fn drop(&mut self) {
        // SAFETY: IMMDevice::GetId transfers this CoTaskMem allocation.
        unsafe { CoTaskMemFree(Some(self.0 .0.cast::<c_void>())) };
    }
}

fn logical_format() -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16,
        nChannels: LOGICAL_CHANNELS,
        nSamplesPerSec: PLAYBACK_TIMELINE_HZ,
        nAvgBytesPerSec: PLAYBACK_TIMELINE_HZ * u32::from(BYTES_PER_FRAME),
        nBlockAlign: BYTES_PER_FRAME,
        wBitsPerSample: BITS_PER_SAMPLE,
        cbSize: 0,
    }
}

fn activate_default_endpoint() -> windows_core::Result<ActivatedEndpoint> {
    // SAFETY: standard MMDevice activation chain. Every returned interface and
    // task allocation is owned by an RAII wrapper before another fallible call.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let endpoint_id = CoTaskString::new(device.GetId()?)?.to_string()?;
        let client3: IAudioClient3 = device.Activate(CLSCTX_ALL, None)?;
        let client: IAudioClient = client3.cast()?;

        let mix_format = CoTaskWaveFormat::new(client.GetMixFormat()?)?;
        let device_format = mix_format.describe();
        drop(mix_format);
        let format = logical_format();
        let (client, initialization_path, engine_period_frames) =
            match initialize_audio_client3(&client3, &format) {
                Ok(period) => (
                    client,
                    WasapiInitializationPath::AudioClient3,
                    period as usize,
                ),
                Err(error) if !is_endpoint_invalidation(error.code().0) => {
                    // AUTOCONVERTPCM/SRC are valid only on the legacy shared
                    // Initialize call. This bounded fallback keeps 44.1/96/
                    // 192 kHz endpoints usable while Clipline stays 48 kHz.
                    // Use a fresh activation because a failed modern
                    // Initialize call may leave its IAudioClient un-retryable.
                    drop(client);
                    drop(client3);
                    let legacy_client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
                    legacy_client.Initialize(
                        AUDCLNT_SHAREMODE_SHARED,
                        AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                        LEGACY_BUFFER_DURATION_100NS,
                        0,
                        &format,
                        None,
                    )?;
                    let engine_period_frames = legacy_engine_period_frames(&legacy_client)?;
                    (
                        legacy_client,
                        WasapiInitializationPath::LegacySharedAutoConvert,
                        engine_period_frames,
                    )
                }
                Err(error) => return Err(error),
            };
        let conversion_active = initialization_path
            == WasapiInitializationPath::LegacySharedAutoConvert
            || device_format.sample_rate != PLAYBACK_TIMELINE_HZ
            || device_format.channels != LOGICAL_CHANNELS
            || !device_format.is_logical_float;

        let buffer_frames = client.GetBufferSize()? as usize;
        if buffer_frames == 0 || buffer_frames > MAX_ENDPOINT_BUFFER_FRAMES {
            return Err(validation_windows_error(
                "WASAPI returned an empty or over-one-second endpoint buffer",
            ));
        }
        let device_buffer_duration_100ns = (buffer_frames as u64)
            .checked_mul(HUNDRED_NS_PER_SECOND)
            .and_then(|ticks| ticks.checked_div(u64::from(PLAYBACK_TIMELINE_HZ)))
            .filter(|duration| *duration != 0)
            .ok_or_else(|| validation_windows_error("WASAPI buffer duration overflow"))?;

        let render: IAudioRenderClient = client.GetService()?;
        let clock: IAudioClock = client.GetService()?;
        let volume: ISimpleAudioVolume = client.GetService()?;
        let clock_frequency = clock.GetFrequency()?;
        if clock_frequency == 0 {
            return Err(validation_windows_error(
                "IAudioClock returned a zero frequency",
            ));
        }

        Ok(ActivatedEndpoint {
            endpoint: Endpoint {
                client,
                render,
                clock,
                volume,
                buffer_frames,
                clock_frequency,
                started: false,
                submitted_since_empty: false,
                underrun_latched: false,
            },
            endpoint_id,
            device_format: device_format.label,
            device_sample_rate: device_format.sample_rate,
            device_channels: device_format.channels,
            device_bits_per_sample: device_format.bits_per_sample,
            device_valid_bits_per_sample: device_format.valid_bits_per_sample,
            device_channel_mask: device_format.channel_mask,
            device_buffer_duration_100ns,
            conversion_active,
            initialization_path,
            engine_period_frames,
        })
    }
}

fn current_default_render_endpoint_id() -> windows_core::Result<String> {
    // SAFETY: standard MMDevice enumeration and owned task-string conversion.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        CoTaskString::new(device.GetId()?)?.to_string()
    }
}

fn initialize_audio_client3(
    client: &IAudioClient3,
    format: &WAVEFORMATEX,
) -> windows_core::Result<u32> {
    let mut default_period = 0u32;
    let mut fundamental_period = 0u32;
    let mut minimum_period = 0u32;
    let mut maximum_period = 0u32;
    // SAFETY: all output pointers are live u32 storage and format outlives
    // negotiation and initialization.
    unsafe {
        client.GetSharedModeEnginePeriod(
            format,
            &mut default_period,
            &mut fundamental_period,
            &mut minimum_period,
            &mut maximum_period,
        )?;
    }
    if default_period == 0
        || fundamental_period == 0
        || minimum_period == 0
        || maximum_period == 0
        || default_period < minimum_period
        || default_period > maximum_period
    {
        return Err(validation_windows_error(
            "IAudioClient3 returned an invalid shared engine period",
        ));
    }
    // SAFETY: GetSharedModeEnginePeriod accepted this exact format/period.
    unsafe { client.InitializeSharedAudioStream(0, default_period, format, None)? };
    Ok(default_period)
}

fn legacy_engine_period_frames(client: &IAudioClient) -> windows_core::Result<usize> {
    let mut default_period_100ns = 0i64;
    // SAFETY: client is initialized and the requested output pointer is live.
    unsafe { client.GetDevicePeriod(Some(&mut default_period_100ns), None)? };
    if default_period_100ns <= 0 {
        return Err(validation_windows_error(
            "legacy WASAPI returned an invalid device period",
        ));
    }
    let frames = (default_period_100ns as u128)
        .checked_mul(u128::from(PLAYBACK_TIMELINE_HZ))
        .and_then(|scaled| scaled.checked_add(u128::from(HUNDRED_NS_PER_SECOND - 1)))
        .and_then(|scaled| scaled.checked_div(u128::from(HUNDRED_NS_PER_SECOND)))
        .and_then(|frames| usize::try_from(frames).ok())
        .filter(|frames| *frames != 0 && *frames <= MAX_ENDPOINT_BUFFER_FRAMES)
        .ok_or_else(|| validation_windows_error("legacy WASAPI device period overflow"))?;
    Ok(frames)
}

fn telemetry_for_activation(
    activated: &ActivatedEndpoint,
    endpoint_epoch: u64,
    previous: Option<&WasapiRendererTelemetry>,
    recovery_count: u64,
) -> WasapiRendererTelemetry {
    WasapiRendererTelemetry {
        endpoint_id: activated.endpoint_id.clone(),
        device_format: activated.device_format.clone(),
        device_sample_rate: activated.device_sample_rate,
        device_channels: activated.device_channels,
        device_bits_per_sample: activated.device_bits_per_sample,
        device_valid_bits_per_sample: activated.device_valid_bits_per_sample,
        device_channel_mask: activated.device_channel_mask,
        device_buffer_duration_100ns: activated.device_buffer_duration_100ns,
        conversion_active: activated.conversion_active,
        initialization_path: activated.initialization_path,
        endpoint_epoch,
        buffer_frames: activated.endpoint.buffer_frames,
        engine_period_frames: activated.engine_period_frames,
        clock_frequency: activated.endpoint.clock_frequency,
        underruns: previous.map_or(0, |telemetry| telemetry.underruns),
        underrun_frames: previous.map_or(0, |telemetry| telemetry.underrun_frames),
        frames_written: previous.map_or(0, |telemetry| telemetry.frames_written),
        max_frames_written_per_call: previous
            .map_or(0, |telemetry| telemetry.max_frames_written_per_call),
        recovery_count,
    }
}

fn validation_windows_error(message: &str) -> WindowsError {
    WindowsError::new(E_FAIL, message)
}

fn allocate_endpoint_epoch() -> Result<u64, BackendError> {
    NEXT_ENDPOINT_EPOCH
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1).filter(|next| *next != 0)
        })
        .map_err(|_| fatal_error("WASAPI endpoint epoch exhausted"))
}

pub fn classify_audio_failure(code: i32) -> BackendError {
    let recoverable = is_endpoint_invalidation(code);
    BackendError {
        component: BackendComponent::AudioRenderer,
        kind: if recoverable {
            BackendErrorKind::EndpointInvalidated
        } else {
            BackendErrorKind::Unavailable
        },
        recovery: if recoverable {
            RecoveryDisposition::RecreateComponent
        } else {
            RecoveryDisposition::Fatal
        },
        native_code: Some(i64::from(code)),
        message: if recoverable {
            "WASAPI render endpoint was invalidated".to_owned()
        } else {
            "WASAPI render operation failed".to_owned()
        },
    }
}

fn is_endpoint_invalidation(code: i32) -> bool {
    code == AUDCLNT_E_DEVICE_INVALIDATED.0
        || code == AUDCLNT_E_RESOURCES_INVALIDATED.0
        || code == AUDCLNT_E_SERVICE_NOT_RUNNING.0
}

fn operation_error(operation: &str, error: &WindowsError) -> BackendError {
    let mut classified = classify_audio_failure(error.code().0);
    classified.message = format!("{operation}: {error}");
    classified
}

fn unavailable_error(operation: &str, error: &WindowsError) -> BackendError {
    BackendError {
        component: BackendComponent::AudioRenderer,
        kind: BackendErrorKind::Unavailable,
        recovery: RecoveryDisposition::Fatal,
        native_code: Some(i64::from(error.code().0)),
        message: format!("{operation}: {error}"),
    }
}

fn unavailable_message(message: &str) -> BackendError {
    BackendError {
        component: BackendComponent::AudioRenderer,
        kind: BackendErrorKind::Unavailable,
        recovery: RecoveryDisposition::Fatal,
        native_code: None,
        message: message.to_owned(),
    }
}

fn fatal_error(message: &str) -> BackendError {
    unavailable_message(message)
}

fn stale_error(message: &str) -> BackendError {
    BackendError {
        component: BackendComponent::AudioRenderer,
        kind: BackendErrorKind::StaleWork,
        recovery: RecoveryDisposition::RetryPipeline,
        native_code: None,
        message: message.to_owned(),
    }
}

fn corrupt_error(message: &str) -> BackendError {
    BackendError {
        component: BackendComponent::AudioRenderer,
        kind: BackendErrorKind::CorruptInput,
        recovery: RecoveryDisposition::RetryPipeline,
        native_code: None,
        message: message.to_owned(),
    }
}
