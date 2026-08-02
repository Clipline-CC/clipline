use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG};
use windows::Win32::Graphics::Direct3D11::{ID3D11Texture2D, D3D11_TEXTURE2D_DESC};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_DEVICE_HUNG, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
    DXGI_ERROR_DRIVER_INTERNAL_ERROR,
};
use windows::Win32::Media::MediaFoundation::{
    CMSH264DecoderMFT, IMF2DBuffer, IMFActivate, IMFDXGIBuffer, IMFMediaEventGenerator, IMFSample,
    IMFTransform, MEError, METransformDrainComplete, METransformHaveOutput, METransformNeedInput,
    MFCreateAlignedMemoryBuffer, MFCreateDXGISurfaceBuffer, MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFSampleExtension_CleanPoint,
    MFTEnumEx, MFT_TRANSFORM_CLSID_Attribute, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG_ASYNCMFT,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT,
    MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_INFO,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MF_EVENT_FLAG_NO_WAIT,
    MF_E_HW_MFT_FAILED_START_STREAMING, MF_E_NOTACCEPTING, MF_E_NO_EVENTS_AVAILABLE,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_LOW_LATENCY, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_SUBTYPE,
    MF_SA_D3D11_AWARE, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows_core::{Error as WinError, Interface};

use crate::{
    BackendComponent, BackendError, BackendErrorKind, DecodedVideoFrame, EncodedVideoPacket,
    H264DecoderConfig, PipelineToken, RecoveryDisposition, SubmitStatus, TimelineDuration,
    TimelinePosition, VideoAcceleration, VideoDecoder, VideoDecoderInfo, VideoPixelFormat,
    PLAYBACK_TIMELINE_HZ,
};

use super::com::{ComApartment, MediaFoundationRuntime};
use super::d3d11::PlaybackD3D11Device;

const MAX_PENDING_ACCESS_UNITS: usize = 32;
const MAX_PENDING_ENCODED_BYTES: usize = crate::MAX_ANNEX_B_ACCESS_UNIT_BYTES;
pub const MAX_PLAYBACK_SURFACES: usize = 2;
const MAX_STREAM_CHANGE_RETRIES: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecoderPreference {
    PreferHardware,
    HardwareOnly,
    SoftwareOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecoderCapabilities {
    hardware_decoders: usize,
    software_available: bool,
}

impl DecoderCapabilities {
    pub const fn hardware_available(self) -> bool {
        self.hardware_decoders != 0
    }

    pub const fn hardware_decoder_count(self) -> usize {
        self.hardware_decoders
    }

    pub const fn software_available(self) -> bool {
        self.software_available
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DecoderOwnershipTelemetry {
    pub mft_samples_received: u64,
    pub mft_samples_released: u64,
    pub output_copies: u64,
    pub presentable_frames: u64,
}

struct TexturePoolState {
    free: Vec<ID3D11Texture2D>,
}

struct TexturePool {
    state: Mutex<TexturePoolState>,
}

impl TexturePool {
    fn new(textures: Vec<ID3D11Texture2D>) -> Self {
        Self {
            state: Mutex::new(TexturePoolState { free: textures }),
        }
    }

    fn checkout(self: &Arc<Self>) -> Result<D3D11VideoSurface, BackendError> {
        let mut state = self.state.lock().map_err(|_| BackendError {
            component: BackendComponent::VideoDecoder,
            kind: BackendErrorKind::DecoderFailure,
            recovery: RecoveryDisposition::RecreateComponent,
            native_code: None,
            message: "playback texture pool lock poisoned".into(),
        })?;
        let texture = state.free.pop().ok_or_else(|| BackendError {
            component: BackendComponent::VideoDecoder,
            kind: BackendErrorKind::DecoderFailure,
            recovery: RecoveryDisposition::RetryPipeline,
            native_code: None,
            message: format!("all {MAX_PLAYBACK_SURFACES} bounded playback surfaces are retained"),
        })?;
        drop(state);
        Ok(D3D11VideoSurface {
            texture: Some(texture),
            pool: Arc::clone(self),
        })
    }

    fn give_back(&self, texture: ID3D11Texture2D) {
        if let Ok(mut state) = self.state.lock() {
            state.free.push(texture);
        }
    }
}

/// Move-only presentation surface. Dropping it returns the texture to the
/// bounded playback pool; it never retains an IMF sample or decoder-pool slice.
pub struct D3D11VideoSurface {
    texture: Option<ID3D11Texture2D>,
    pool: Arc<TexturePool>,
}

impl D3D11VideoSurface {
    pub fn texture(&self) -> &ID3D11Texture2D {
        self.texture
            .as_ref()
            .expect("live playback surface owns a texture")
    }
}

impl std::fmt::Debug for D3D11VideoSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("D3D11VideoSurface").finish_non_exhaustive()
    }
}

impl Drop for D3D11VideoSurface {
    fn drop(&mut self) {
        if let Some(texture) = self.texture.take() {
            self.pool.give_back(texture);
        }
    }
}

pub fn probe_h264_decoders() -> Result<DecoderCapabilities, BackendError> {
    let _apartment = ComApartment::multithreaded()
        .map_err(|error| windows_backend(error, "initialize playback COM apartment"))?;
    let _media_foundation = MediaFoundationRuntime::acquire()
        .map_err(|error| windows_backend(error, "start Media Foundation"))?;
    let hardware = enumerate_h264_decoders(true)?;
    let software = enumerate_h264_decoders(false)?;
    Ok(DecoderCapabilities {
        hardware_decoders: hardware.len(),
        software_available: software.iter().any(is_inbox_software_decoder),
    })
}

pub struct WindowsH264Decoder {
    // Drop the transform/session before the process/thread initialization
    // guards. Field order is intentional.
    session: Option<DecoderSession>,
    device: Rc<PlaybackD3D11Device>,
    preference: DecoderPreference,
    last_telemetry: DecoderOwnershipTelemetry,
    _media_foundation: MediaFoundationRuntime,
    _apartment: ComApartment,
}

impl WindowsH264Decoder {
    pub fn new(preference: DecoderPreference) -> Result<Self, BackendError> {
        let apartment = ComApartment::multithreaded()
            .map_err(|error| windows_backend(error, "initialize playback COM apartment"))?;
        let media_foundation = MediaFoundationRuntime::acquire()
            .map_err(|error| windows_backend(error, "start Media Foundation"))?;
        let device = Rc::new(
            PlaybackD3D11Device::hardware()
                .map_err(|error| windows_backend(error, "create playback D3D11 device"))?,
        );
        Ok(Self {
            session: None,
            device,
            preference,
            last_telemetry: DecoderOwnershipTelemetry::default(),
            _media_foundation: media_foundation,
            _apartment: apartment,
        })
    }

    pub const fn preference(&self) -> DecoderPreference {
        self.preference
    }

    pub fn ownership_telemetry(&self) -> DecoderOwnershipTelemetry {
        self.session
            .as_ref()
            .map_or(self.last_telemetry, |session| session.telemetry)
    }

    fn candidates(&self) -> Result<Vec<DecoderCandidate>, BackendError> {
        let mut candidates = Vec::new();
        if self.preference != DecoderPreference::SoftwareOnly {
            candidates.extend(
                enumerate_h264_decoders(true)?
                    .into_iter()
                    .map(|activation| DecoderCandidate {
                        activation,
                        acceleration: VideoAcceleration::Hardware,
                    }),
            );
        }
        if self.preference != DecoderPreference::HardwareOnly {
            candidates.extend(
                enumerate_h264_decoders(false)?
                    .into_iter()
                    .filter(is_inbox_software_decoder)
                    .map(|activation| DecoderCandidate {
                        activation,
                        acceleration: VideoAcceleration::Software,
                    }),
            );
        }
        Ok(candidates)
    }
}

impl VideoDecoder for WindowsH264Decoder {
    type Surface = D3D11VideoSurface;

    fn configure(
        &mut self,
        config: &H264DecoderConfig,
        token: PipelineToken,
    ) -> Result<(), BackendError> {
        self.close();
        self.device
            .reset_manager()
            .map_err(|error| windows_backend(error, "reset playback DXGI device manager"))?;
        let candidates = self.candidates()?;
        if candidates.is_empty() {
            return Err(unavailable_error(match self.preference {
                DecoderPreference::PreferHardware => "no H.264 decoder MFT is available",
                DecoderPreference::HardwareOnly => "no hardware H.264 decoder MFT is available",
                DecoderPreference::SoftwareOnly => {
                    "the inbox software H.264 decoder MFT is unavailable"
                }
            }));
        }

        let mut failures = Vec::new();
        for candidate in candidates {
            match DecoderSession::new(Rc::clone(&self.device), candidate, config, token) {
                Ok(session) => {
                    self.last_telemetry = DecoderOwnershipTelemetry::default();
                    self.session = Some(session);
                    return Ok(());
                }
                Err(error) => failures.push(error.to_string()),
            }
        }
        Err(BackendError {
            component: BackendComponent::VideoDecoder,
            kind: BackendErrorKind::Unavailable,
            recovery: RecoveryDisposition::Fatal,
            native_code: None,
            message: format!(
                "every eligible H.264 decoder failed configuration: {}",
                failures.join("; ")
            ),
        })
    }

    fn info(&self) -> Option<VideoDecoderInfo> {
        self.session.as_ref().map(|session| session.info)
    }

    fn submit(
        &mut self,
        packet: EncodedVideoPacket<'_>,
        token: PipelineToken,
    ) -> Result<SubmitStatus, BackendError> {
        let session = self.session.as_mut().ok_or_else(|| {
            unavailable_error("H.264 decoder must be configured before submitting input")
        })?;
        session.submit(packet, token)
    }

    fn receive(&mut self) -> Result<Option<DecodedVideoFrame<Self::Surface>>, BackendError> {
        let session = self.session.as_mut().ok_or_else(|| {
            unavailable_error("H.264 decoder must be configured before receiving output")
        })?;
        session.receive()
    }

    fn flush(&mut self, token: PipelineToken) -> Result<(), BackendError> {
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| unavailable_error("H.264 decoder must be configured before flushing"))?;
        session.flush(token)
    }

    fn drain(&mut self, token: PipelineToken) -> Result<(), BackendError> {
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| unavailable_error("H.264 decoder must be configured before draining"))?;
        session.drain(token)
    }

    fn close(&mut self) {
        if let Some(session) = self.session.take() {
            self.last_telemetry = session.telemetry;
            drop(session);
        }
    }
}

impl Drop for WindowsH264Decoder {
    fn drop(&mut self) {
        self.close();
    }
}

struct DecoderCandidate {
    activation: IMFActivate,
    acceleration: VideoAcceleration,
}

struct OwnedActivation(IMFActivate);

impl Drop for OwnedActivation {
    fn drop(&mut self) {
        // SAFETY: this wrapper is created only after ActivateObject succeeds
        // and owns the matching activation shutdown responsibility.
        let _ = unsafe { self.0.ShutdownObject() };
    }
}

#[derive(Clone, Copy)]
struct SubmittedFrame {
    sample_index: usize,
    pts: TimelinePosition,
    duration: TimelineDuration,
    token: PipelineToken,
    sample_time_100ns: i64,
    encoded_bytes: usize,
}

struct CallerOutputSample {
    sample: IMFSample,
    _storage: CallerOutputStorage,
}

enum CallerOutputStorage {
    Dxgi { _texture: ID3D11Texture2D },
    Memory,
}

struct DecoderSession {
    _activation: OwnedActivation,
    transform: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    asynchronous: bool,
    uses_dxgi_output: bool,
    device: Rc<PlaybackD3D11Device>,
    surfaces: Arc<TexturePool>,
    input_id: u32,
    output_id: u32,
    output_info: MFT_OUTPUT_STREAM_INFO,
    caller_output: Option<CallerOutputSample>,
    submitted: VecDeque<SubmittedFrame>,
    pending_encoded_bytes: usize,
    active_token: PipelineToken,
    need_input_credits: u32,
    have_output_events: u32,
    drain_complete: bool,
    output_time_offset_100ns: Option<i64>,
    info: VideoDecoderInfo,
    telemetry: DecoderOwnershipTelemetry,
    coded_width: u32,
    coded_height: u32,
    cpu_nv12: Vec<u8>,
    visible_nv12: Vec<u8>,
}

impl DecoderSession {
    fn new(
        device: Rc<PlaybackD3D11Device>,
        candidate: DecoderCandidate,
        config: &H264DecoderConfig,
        token: PipelineToken,
    ) -> Result<Self, BackendError> {
        // SAFETY: candidate came from MFTEnumEx and remains alive through the
        // activation call.
        let transform: IMFTransform = unsafe { candidate.activation.ActivateObject() }
            .map_err(|error| windows_backend(error, "activate H.264 decoder MFT"))?;
        let activation = OwnedActivation(candidate.activation);
        // SAFETY: transform attributes are queried and updated before stream
        // negotiation begins.
        let attributes = unsafe { transform.GetAttributes() }
            .map_err(|error| windows_backend(error, "query H.264 decoder attributes"))?;
        let asynchronous = unsafe { attributes.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) != 0;
        if asynchronous {
            // SAFETY: asynchronous MFTs must be explicitly unlocked before use.
            unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }
                .map_err(|error| windows_backend(error, "unlock asynchronous H.264 decoder"))?;
        }
        let _ = unsafe { attributes.SetUINT32(&MF_LOW_LATENCY, 1) };

        let d3d_aware = unsafe { attributes.GetUINT32(&MF_SA_D3D11_AWARE) }.unwrap_or(0) != 0;
        if !d3d_aware && candidate.acceleration == VideoAcceleration::Hardware {
            return Err(unavailable_error(
                "hardware H.264 decoder does not support D3D11 output surfaces",
            ));
        }
        let (mut input_ids, mut output_ids) = ([0_u32; 1], [0_u32; 1]);
        // SAFETY: decoders are one-input/one-output. E_NOTIMPL leaves the
        // documented fixed IDs at zero.
        let _ = unsafe { transform.GetStreamIDs(&mut input_ids, &mut output_ids) };
        let (input_id, output_id) = (input_ids[0], output_ids[0]);

        // SAFETY: SET_D3D_MANAGER takes the live manager pointer as ULONG_PTR.
        // Some inbox software-decoder builds advertise D3D11 awareness but
        // reject a DXGI manager with E_NOINTERFACE. Hardware candidates must
        // remain zero-copy; the explicit software tier may use bounded memory
        // output followed by a copy into the playback-owned NV12 texture.
        let manager_result = if d3d_aware {
            unsafe {
                transform.ProcessMessage(
                    MFT_MESSAGE_SET_D3D_MANAGER,
                    device.manager.as_raw() as usize,
                )
            }
        } else {
            Err(WinError::new(
                E_FAIL,
                "software H.264 decoder is not D3D11 aware",
            ))
        };
        let uses_dxgi_output = match manager_result {
            Ok(()) => true,
            Err(_) if candidate.acceleration == VideoAcceleration::Software => false,
            Err(error) => return Err(windows_backend(error, "bind decoder DXGI device manager")),
        };
        // Decoder MFTs choose their DXVA or system-memory pipeline while
        // negotiating types, so manager binding must happen first.
        configure_input_type(&transform, input_id, config)?;
        configure_nv12_output(&transform, output_id, config)?;
        let output_info = unsafe { transform.GetOutputStreamInfo(output_id) }
            .map_err(|error| windows_backend(error, "query H.264 decoder output stream"))?;
        let events = if asynchronous {
            Some(
                transform
                    .cast::<IMFMediaEventGenerator>()
                    .map_err(|error| windows_backend(error, "query asynchronous MFT events"))?,
            )
        } else {
            None
        };

        // SAFETY: standard streaming-start message order after both types and
        // the optional D3D manager are set.
        unsafe { transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0) }
            .map_err(|error| windows_backend(error, "begin H.264 decoder streaming"))?;
        unsafe { transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0) }
            .map_err(|error| windows_backend(error, "start H.264 decoder streaming"))?;

        let width = config.width();
        let height = config.height();
        let max_coded_width = align_up_16(u32::from(width))?;
        let max_coded_height = align_up_16(u32::from(height))?;
        let packed_nv12_size = usize::try_from(max_coded_width)
            .ok()
            .and_then(|width| {
                usize::try_from(max_coded_height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|luma| luma.checked_add(luma / 2))
            .ok_or_else(|| corrupt_error("NV12 decoder output size overflow"))?;
        let mut cpu_nv12 = Vec::new();
        cpu_nv12
            .try_reserve_exact(packed_nv12_size)
            .map_err(|_| corrupt_error("NV12 decoder output allocation failed"))?;
        cpu_nv12.resize(packed_nv12_size, 0);
        let visible_size = usize::from(width)
            .checked_mul(usize::from(height))
            .and_then(|luma| luma.checked_add(luma / 2))
            .ok_or_else(|| corrupt_error("visible NV12 output size overflow"))?;
        let mut visible_nv12 = Vec::new();
        visible_nv12
            .try_reserve_exact(visible_size)
            .map_err(|_| corrupt_error("visible NV12 allocation failed"))?;
        visible_nv12.resize(visible_size, 0);
        let mut playback_textures = Vec::with_capacity(MAX_PLAYBACK_SURFACES);
        for _ in 0..MAX_PLAYBACK_SURFACES {
            playback_textures.push(
                device
                    .create_nv12_texture(u32::from(width), u32::from(height))
                    .map_err(|error| windows_backend(error, "create playback NV12 texture"))?,
            );
        }
        Ok(Self {
            _activation: activation,
            transform,
            events,
            asynchronous,
            uses_dxgi_output,
            device: Rc::clone(&device),
            surfaces: Arc::new(TexturePool::new(playback_textures)),
            input_id,
            output_id,
            output_info,
            caller_output: None,
            submitted: VecDeque::with_capacity(MAX_PENDING_ACCESS_UNITS),
            pending_encoded_bytes: 0,
            active_token: token,
            need_input_credits: 0,
            have_output_events: 0,
            drain_complete: false,
            output_time_offset_100ns: None,
            info: VideoDecoderInfo {
                acceleration: candidate.acceleration,
                pixel_format: VideoPixelFormat::Nv12,
                width,
                height,
                adapter_luid: device_adapter_luid(&device),
            },
            telemetry: DecoderOwnershipTelemetry::default(),
            coded_width: u32::from(width),
            coded_height: u32::from(height),
            cpu_nv12,
            visible_nv12,
        })
    }

    fn submit(
        &mut self,
        packet: EncodedVideoPacket<'_>,
        token: PipelineToken,
    ) -> Result<SubmitStatus, BackendError> {
        if token != self.active_token {
            return Err(stale_token_error(self.active_token, token));
        }
        validate_annex_b(packet.bytes)?;
        if self.submitted.len() >= MAX_PENDING_ACCESS_UNITS {
            return Ok(SubmitStatus::Backpressured);
        }
        let pending_encoded_bytes = self
            .pending_encoded_bytes
            .checked_add(packet.bytes.len())
            .ok_or_else(|| corrupt_error("pending H.264 byte count overflow"))?;
        if pending_encoded_bytes > MAX_PENDING_ENCODED_BYTES {
            return Ok(SubmitStatus::Backpressured);
        }
        self.poll_events()?;
        if self.asynchronous && self.need_input_credits == 0 {
            return Ok(SubmitStatus::Backpressured);
        }

        let sample_time_100ns = timeline_position_to_100ns(packet.pts)?;
        let duration_100ns = timeline_duration_to_100ns(packet.duration)?;
        let sample = make_input_sample(
            packet.bytes,
            sample_time_100ns,
            duration_100ns,
            packet.is_sync,
        )?;
        // SAFETY: sample owns its bounded input buffer and stream ID is the
        // negotiated decoder input.
        let result = unsafe { self.transform.ProcessInput(self.input_id, &sample, 0) };
        match result {
            Ok(()) => {
                if self.asynchronous {
                    self.need_input_credits -= 1;
                }
                self.submitted.push_back(SubmittedFrame {
                    sample_index: packet.sample_index,
                    pts: packet.pts,
                    duration: packet.duration,
                    token,
                    sample_time_100ns,
                    encoded_bytes: packet.bytes.len(),
                });
                self.pending_encoded_bytes = pending_encoded_bytes;
                Ok(SubmitStatus::Accepted)
            }
            Err(error) if error.code() == MF_E_NOTACCEPTING => Ok(SubmitStatus::Backpressured),
            Err(error) => Err(windows_backend(error, "submit H.264 access unit")),
        }
    }

    fn receive(&mut self) -> Result<Option<DecodedVideoFrame<D3D11VideoSurface>>, BackendError> {
        self.poll_events()?;
        if self.asynchronous && self.have_output_events == 0 {
            return Ok(None);
        }

        for _ in 0..MAX_STREAM_CHANGE_RETRIES {
            let mut output = self.output_buffer()?;
            let mut status = 0_u32;
            // SAFETY: output guard owns both ManuallyDrop fields on every
            // result path and stream ID matches negotiation.
            let result = unsafe {
                self.transform
                    .ProcessOutput(0, std::slice::from_mut(output.raw_mut()), &mut status)
            };
            match result {
                Ok(()) => {
                    if self.asynchronous {
                        self.have_output_events = self.have_output_events.saturating_sub(1);
                    }
                    let sample = output.take_sample().ok_or_else(|| BackendError {
                        component: BackendComponent::VideoDecoder,
                        kind: BackendErrorKind::DecoderFailure,
                        recovery: RecoveryDisposition::RetryPipeline,
                        native_code: None,
                        message: "H.264 decoder returned no sample on successful output".into(),
                    })?;
                    drop(output);
                    return self.frame_from_sample(sample).map(Some);
                }
                Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                    if self.asynchronous {
                        self.have_output_events = self.have_output_events.saturating_sub(1);
                    }
                    return Ok(None);
                }
                Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    self.renegotiate_output()?;
                }
                Err(error) => return Err(windows_backend(error, "receive H.264 decoder output")),
            }
        }
        Err(BackendError {
            component: BackendComponent::VideoDecoder,
            kind: BackendErrorKind::DecoderFailure,
            recovery: RecoveryDisposition::RetryPipeline,
            native_code: None,
            message: "H.264 decoder exceeded bounded stream-change retries".into(),
        })
    }

    fn frame_from_sample(
        &mut self,
        sample: IMFSample,
    ) -> Result<DecodedVideoFrame<D3D11VideoSurface>, BackendError> {
        self.telemetry.mft_samples_received = self.telemetry.mft_samples_received.saturating_add(1);
        let metadata = self.submitted.pop_front().ok_or_else(|| BackendError {
            component: BackendComponent::VideoDecoder,
            kind: BackendErrorKind::DecoderFailure,
            recovery: RecoveryDisposition::RetryPipeline,
            native_code: None,
            message: "H.264 decoder emitted output without submitted metadata".into(),
        })?;
        self.pending_encoded_bytes = self
            .pending_encoded_bytes
            .checked_sub(metadata.encoded_bytes)
            .ok_or_else(|| BackendError {
                component: BackendComponent::VideoDecoder,
                kind: BackendErrorKind::DecoderFailure,
                recovery: RecoveryDisposition::RecreateComponent,
                native_code: None,
                message: "pending H.264 byte accounting underflow".into(),
            })?;
        if let Ok(output_time) = unsafe { sample.GetSampleTime() } {
            let offset = output_time
                .checked_sub(metadata.sample_time_100ns)
                .ok_or_else(|| corrupt_error("decoder output timestamp offset overflow"))?;
            if let Some(expected_offset) = self.output_time_offset_100ns {
                if offset != expected_offset {
                    // The inbox decoder may stamp only the first post-flush
                    // keyframe at zero, then resume the submitted timeline on
                    // the next output. Accept exactly that one-way transition;
                    // every later frame must keep the zero offset. Clipline's
                    // no-B-frame contract still makes FIFO metadata canonical.
                    if expected_offset < 0 && offset == 0 {
                        self.output_time_offset_100ns = Some(0);
                    } else {
                        return Err(BackendError {
                            component: BackendComponent::VideoDecoder,
                            kind: BackendErrorKind::DecoderFailure,
                            recovery: RecoveryDisposition::RetryPipeline,
                            native_code: None,
                            message: format!(
                                "H.264 decoder timestamp offset changed from {expected_offset} to {offset}"
                            ),
                        });
                    }
                }
            } else {
                // The inbox software decoder may rebase the first post-flush
                // keyframe to zero. Preserve indexed PTS while requiring the
                // same offset for every later FIFO output in the generation.
                self.output_time_offset_100ns = Some(offset);
            }
        }

        // The returned sample may refer to a decoder-owned texture array.
        // Extract the exact slice and copy it into a separate playback texture.
        let media_buffer = unsafe { sample.GetBufferByIndex(0) }
            .map_err(|error| windows_backend(error, "get decoder DXGI buffer"))?;
        let surface = self.surfaces.checkout()?;
        if let Ok(dxgi_buffer) = media_buffer.cast::<IMFDXGIBuffer>() {
            let mut raw_resource = std::ptr::null_mut();
            // SAFETY: GetResource writes one AddRef'd ID3D11Texture2D pointer.
            unsafe { dxgi_buffer.GetResource(&ID3D11Texture2D::IID, &mut raw_resource) }
                .map_err(|error| windows_backend(error, "get decoder output texture"))?;
            if raw_resource.is_null() {
                return Err(unavailable_error("decoder returned a null D3D11 texture"));
            }
            // SAFETY: GetResource returned an owned COM reference for this IID.
            let source_texture = unsafe { ID3D11Texture2D::from_raw(raw_resource) };
            let source_subresource = unsafe { dxgi_buffer.GetSubresourceIndex() }
                .map_err(|error| windows_backend(error, "get decoder texture subresource"))?;
            validate_output_texture(
                &source_texture,
                self.coded_width,
                self.coded_height,
                source_subresource,
            )?;
            self.device
                .copy_texture(
                    surface.texture(),
                    &source_texture,
                    source_subresource,
                    u32::from(self.info.width),
                    u32::from(self.info.height),
                )
                .map_err(|error| windows_backend(error, "copy decoder output texture"))?;
            drop(source_texture);
            drop(dxgi_buffer);
        } else {
            let coded_size = nv12_size(self.coded_width, self.coded_height)?;
            copy_system_nv12(&media_buffer, &mut self.cpu_nv12[..coded_size])?;
            let upload = if self.coded_width == u32::from(self.info.width)
                && self.coded_height == u32::from(self.info.height)
            {
                &self.cpu_nv12[..coded_size]
            } else {
                crop_nv12(
                    &self.cpu_nv12[..coded_size],
                    self.coded_width,
                    self.coded_height,
                    &mut self.visible_nv12,
                    u32::from(self.info.width),
                    u32::from(self.info.height),
                )?;
                &self.visible_nv12
            };
            self.device
                .upload_nv12(
                    surface.texture(),
                    upload,
                    u32::from(self.info.width),
                    u32::from(self.info.height),
                )
                .map_err(|error| windows_backend(error, "upload software decoder output"))?;
        }
        self.telemetry.output_copies = self.telemetry.output_copies.saturating_add(1);

        // Explicitly release every MFT/sample/source reference before the
        // surface crosses the presentable boundary. Only the playback-owned
        // destination texture survives in `surface`.
        drop(media_buffer);
        drop(sample);
        self.telemetry.mft_samples_released = self.telemetry.mft_samples_released.saturating_add(1);

        let frame = DecodedVideoFrame::new(
            surface,
            metadata.sample_index,
            metadata.pts,
            metadata.duration,
            metadata.token,
        );
        self.telemetry.presentable_frames = self.telemetry.presentable_frames.saturating_add(1);
        Ok(frame)
    }

    fn flush(&mut self, token: PipelineToken) -> Result<(), BackendError> {
        // SAFETY: FLUSH synchronously discards transform-owned queued data.
        unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0) }
            .map_err(|error| windows_backend(error, "flush H.264 decoder"))?;
        self.submitted.clear();
        self.pending_encoded_bytes = 0;
        self.caller_output = None;
        self.need_input_credits = 0;
        self.have_output_events = 0;
        self.drain_complete = false;
        self.output_time_offset_100ns = None;
        self.discard_pending_events()?;
        self.active_token = token;
        // SAFETY: restarts the already-negotiated stream after FLUSH.
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
        }
        .map_err(|error| windows_backend(error, "restart H.264 decoder after flush"))
    }

    fn drain(&mut self, token: PipelineToken) -> Result<(), BackendError> {
        if token != self.active_token {
            return Err(stale_token_error(self.active_token, token));
        }
        // SAFETY: standard end-of-input then drain message sequence.
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, self.input_id as usize)
        }
        .map_err(|error| windows_backend(error, "end H.264 decoder input"))?;
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, self.input_id as usize)
        }
        .map_err(|error| windows_backend(error, "drain H.264 decoder"))?;
        Ok(())
    }

    fn output_buffer(&mut self) -> Result<OwnedOutputBuffer, BackendError> {
        let flags = self.output_info.dwFlags;
        if flags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0
            || flags & MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32 != 0
        {
            return Ok(OwnedOutputBuffer::new(self.output_id));
        }
        if self.caller_output.is_none() {
            let sample = unsafe { MFCreateSample() }
                .map_err(|error| windows_backend(error, "create caller decoder output sample"))?;
            let (buffer, storage) = if self.uses_dxgi_output {
                let texture = self
                    .device
                    .create_decoder_output_nv12_texture(self.coded_width, self.coded_height)
                    .map_err(|error| {
                        windows_backend(error, "create caller decoder output texture")
                    })?;
                // SAFETY: wraps the live texture's subresource zero in an MF buffer.
                let buffer =
                    unsafe { MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &texture, 0, false) }
                        .map_err(|error| {
                            windows_backend(error, "wrap caller decoder output texture")
                        })?;
                (buffer, CallerOutputStorage::Dxgi { _texture: texture })
            } else {
                if self.output_info.cbSize == 0 {
                    return Err(unavailable_error(
                        "software decoder requested a zero-sized output buffer",
                    ));
                }
                let alignment = mf_alignment_mask(self.output_info.cbAlignment)?;
                let buffer =
                    unsafe { MFCreateAlignedMemoryBuffer(self.output_info.cbSize, alignment) }
                        .map_err(|error| {
                            windows_backend(error, "allocate software decoder output buffer")
                        })?;
                (buffer, CallerOutputStorage::Memory)
            };
            unsafe { sample.AddBuffer(&buffer) }
                .map_err(|error| windows_backend(error, "attach caller decoder output buffer"))?;
            self.caller_output = Some(CallerOutputSample {
                sample,
                _storage: storage,
            });
        }
        let sample = self
            .caller_output
            .as_ref()
            .expect("caller output initialized")
            .sample
            .clone();
        // SAFETY: clear MFT-written attributes before reusing caller storage.
        unsafe { sample.DeleteAllItems() }
            .map_err(|error| windows_backend(error, "reset caller decoder output sample"))?;
        if !self.uses_dxgi_output {
            let buffer = unsafe { sample.GetBufferByIndex(0) }
                .map_err(|error| windows_backend(error, "get reusable output buffer"))?;
            unsafe { buffer.SetCurrentLength(0) }
                .map_err(|error| windows_backend(error, "reset reusable output length"))?;
        }
        Ok(OwnedOutputBuffer::with_sample(self.output_id, sample))
    }

    fn renegotiate_output(&mut self) -> Result<(), BackendError> {
        let (coded_width, coded_height) = accept_stream_change_nv12_output(
            &self.transform,
            self.output_id,
            self.info.width,
            self.info.height,
        )?;
        self.coded_width = coded_width;
        self.coded_height = coded_height;
        self.output_info = unsafe { self.transform.GetOutputStreamInfo(self.output_id) }
            .map_err(|error| windows_backend(error, "refresh decoder output stream"))?;
        self.caller_output = None;
        Ok(())
    }

    fn poll_events(&mut self) -> Result<(), BackendError> {
        let Some(events) = self.events.as_ref() else {
            return Ok(());
        };
        loop {
            // SAFETY: nonblocking event retrieval on a live event generator.
            let event = match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => event,
                Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => break,
                Err(error) => return Err(windows_backend(error, "poll H.264 decoder events")),
            };
            let event_type = unsafe { event.GetType() }
                .map_err(|error| windows_backend(error, "read H.264 decoder event type"))?;
            if event_type == METransformNeedInput.0 as u32 {
                self.need_input_credits = self.need_input_credits.saturating_add(1);
            } else if event_type == METransformHaveOutput.0 as u32 {
                self.have_output_events = self.have_output_events.saturating_add(1);
            } else if event_type == METransformDrainComplete.0 as u32 {
                self.drain_complete = true;
            } else if event_type == MEError.0 as u32 {
                let status = unsafe { event.GetStatus() }
                    .map_err(|error| windows_backend(error, "read H.264 decoder error event"))?;
                let error = if status.is_err() {
                    WinError::from(status)
                } else {
                    WinError::new(E_FAIL, "H.264 decoder reported MEError")
                };
                return Err(windows_backend(error, "H.264 decoder event"));
            }
        }
        Ok(())
    }

    fn discard_pending_events(&mut self) -> Result<(), BackendError> {
        let Some(events) = self.events.as_ref() else {
            return Ok(());
        };
        loop {
            // SAFETY: discard stale nonblocking events after a synchronous
            // transform flush; no returned event crosses the generation fence.
            match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(_) => {}
                Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(error) => {
                    return Err(windows_backend(
                        error,
                        "discard flushed H.264 decoder events",
                    ))
                }
            }
        }
    }
}

impl Drop for DecoderSession {
    fn drop(&mut self) {
        self.submitted.clear();
        self.caller_output = None;
        // SAFETY: best-effort shutdown for a live transform. The activation
        // guard then calls ShutdownObject before all COM wrappers are released.
        let _ = unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0)
        };
    }
}

struct OwnedOutputBuffer {
    raw: MFT_OUTPUT_DATA_BUFFER,
}

impl OwnedOutputBuffer {
    fn new(output_id: u32) -> Self {
        Self {
            raw: MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: output_id,
                pSample: ManuallyDrop::new(None),
                ..Default::default()
            },
        }
    }

    fn with_sample(output_id: u32, sample: IMFSample) -> Self {
        Self {
            raw: MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: output_id,
                pSample: ManuallyDrop::new(Some(sample)),
                ..Default::default()
            },
        }
    }

    fn raw_mut(&mut self) -> &mut MFT_OUTPUT_DATA_BUFFER {
        &mut self.raw
    }

    fn take_sample(&mut self) -> Option<IMFSample> {
        // SAFETY: immediately replace the moved ManuallyDrop value with None
        // so this guard still owns a valid field for Drop.
        let sample = unsafe { ManuallyDrop::take(&mut self.raw.pSample) };
        self.raw.pSample = ManuallyDrop::new(None);
        sample
    }
}

impl Drop for OwnedOutputBuffer {
    fn drop(&mut self) {
        // SAFETY: ProcessOutput transfers both fields to its caller on every
        // return path; untaken values remain owned by this guard.
        unsafe {
            ManuallyDrop::drop(&mut self.raw.pSample);
            ManuallyDrop::drop(&mut self.raw.pEvents);
        }
    }
}

fn enumerate_h264_decoders(hardware: bool) -> Result<Vec<IMFActivate>, BackendError> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let flags = if hardware {
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER
    } else {
        MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER
    };
    let mut raw_activations: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0_u32;
    // SAFETY: out-parameters receive a CoTaskMem array. Each interface is
    // moved into Rust ownership before the array allocation is freed.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            flags,
            Some(&input),
            Some(&output),
            &mut raw_activations,
            &mut count,
        )
    }
    .map_err(|error| windows_backend(error, "enumerate H.264 decoder MFTs"))?;
    if count == 0 || raw_activations.is_null() {
        if !raw_activations.is_null() {
            // SAFETY: MFTEnumEx owns this zero-length CoTaskMem allocation.
            unsafe { CoTaskMemFree(Some(raw_activations.cast())) };
        }
        return Ok(Vec::new());
    }
    // SAFETY: MFTEnumEx returned `count` initialized optional interfaces.
    let activations = unsafe {
        let slice = std::slice::from_raw_parts_mut(raw_activations, count as usize);
        let owned = slice.iter_mut().filter_map(Option::take).collect();
        CoTaskMemFree(Some(raw_activations.cast()));
        owned
    };
    Ok(activations)
}

fn is_inbox_software_decoder(activation: &IMFActivate) -> bool {
    // SAFETY: reads a GUID attribute from a live activation object.
    unsafe { activation.GetGUID(&MFT_TRANSFORM_CLSID_Attribute) }
        .is_ok_and(|class_id| class_id == CMSH264DecoderMFT)
}

fn configure_input_type(
    transform: &IMFTransform,
    input_id: u32,
    config: &H264DecoderConfig,
) -> Result<(), BackendError> {
    let media_type = unsafe { MFCreateMediaType() }
        .map_err(|error| windows_backend(error, "create H.264 decoder input type"))?;
    let sequence_header = annex_b_sequence_header(config)?;
    // SAFETY: setters target a fresh media type; input is set before output as
    // required by decoder MFTs.
    unsafe { media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video) }
        .map_err(|error| windows_backend(error, "set decoder input major type"))?;
    unsafe { media_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264) }
        .map_err(|error| windows_backend(error, "set decoder input subtype"))?;
    unsafe {
        media_type.SetUINT64(
            &MF_MT_FRAME_SIZE,
            pack_pair(u32::from(config.width()), u32::from(config.height())),
        )
    }
    .map_err(|error| windows_backend(error, "set decoder input dimensions"))?;
    unsafe { media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32) }
        .map_err(|error| windows_backend(error, "set decoder interlace mode"))?;
    unsafe { media_type.SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &sequence_header) }
        .map_err(|error| windows_backend(error, "set decoder sequence header"))?;
    unsafe { transform.SetInputType(input_id, &media_type, 0) }
        .map_err(|error| windows_backend(error, "configure H.264 decoder input type"))
}

fn configure_nv12_output(
    transform: &IMFTransform,
    output_id: u32,
    config: &H264DecoderConfig,
) -> Result<(), BackendError> {
    for type_index in 0.. {
        // SAFETY: enumeration ends with MF_E_NO_MORE_TYPES.
        let Ok(media_type) = (unsafe { transform.GetOutputAvailableType(output_id, type_index) })
        else {
            break;
        };
        let subtype = unsafe { media_type.GetGUID(&MF_MT_SUBTYPE) }
            .map_err(|error| windows_backend(error, "query decoder output subtype"))?;
        if subtype != MFVideoFormat_NV12 {
            continue;
        }
        // SAFETY: constrain the selected offered NV12 type to indexed media
        // dimensions, then commit it as the current output type.
        unsafe {
            media_type.SetUINT64(
                &MF_MT_FRAME_SIZE,
                pack_pair(u32::from(config.width()), u32::from(config.height())),
            )
        }
        .map_err(|error| windows_backend(error, "set NV12 decoder dimensions"))?;
        unsafe { transform.SetOutputType(output_id, &media_type, 0) }
            .map_err(|error| windows_backend(error, "configure NV12 decoder output type"))?;
        return Ok(());
    }
    Err(unavailable_error(
        "H.264 decoder offers no compatible NV12 output type",
    ))
}

fn accept_stream_change_nv12_output(
    transform: &IMFTransform,
    output_id: u32,
    expected_width: u16,
    expected_height: u16,
) -> Result<(u32, u32), BackendError> {
    let mut offered = Vec::new();
    let maximum_width = align_up_16(u32::from(expected_width))?;
    let maximum_height = align_up_16(u32::from(expected_height))?;
    for type_index in 0.. {
        // SAFETY: enumeration ends with MF_E_NO_MORE_TYPES.
        let Ok(media_type) = (unsafe { transform.GetOutputAvailableType(output_id, type_index) })
        else {
            break;
        };
        let subtype = unsafe { media_type.GetGUID(&MF_MT_SUBTYPE) }
            .map_err(|error| windows_backend(error, "query changed decoder output subtype"))?;
        if subtype != MFVideoFormat_NV12 {
            continue;
        }
        let dimensions = unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE) }
            .map_err(|error| windows_backend(error, "query changed decoder dimensions"))?;
        let width = (dimensions >> 32) as u32;
        let height = dimensions as u32;
        offered.push((width, height));
        if width < u32::from(expected_width)
            || height < u32::from(expected_height)
            || width > maximum_width
            || height > maximum_height
        {
            continue;
        }
        // SAFETY: accept the MFT-authored stream-change type without mutating
        // its dependent stride/aperture attributes.
        unsafe { transform.SetOutputType(output_id, &media_type, 0) }
            .map_err(|error| windows_backend(error, "accept changed NV12 decoder output"))?;
        return Ok((width, height));
    }
    Err(unavailable_error(format!(
        "H.264 stream change offers no {expected_width}x{expected_height} NV12 output; offered {offered:?}"
    )))
}

fn annex_b_sequence_header(config: &H264DecoderConfig) -> Result<Vec<u8>, BackendError> {
    let nal_count = config
        .sequence_parameter_sets()
        .len()
        .checked_add(config.picture_parameter_sets().len())
        .ok_or_else(|| corrupt_error("H.264 parameter-set count overflow"))?;
    let payload_bytes = config
        .sequence_parameter_sets()
        .iter()
        .chain(config.picture_parameter_sets())
        .try_fold(0_usize, |total, nal| total.checked_add(nal.len()))
        .ok_or_else(|| corrupt_error("H.264 parameter-set size overflow"))?;
    let capacity = payload_bytes
        .checked_add(
            nal_count
                .checked_mul(4)
                .ok_or_else(|| corrupt_error("H.264 sequence-header size overflow"))?,
        )
        .ok_or_else(|| corrupt_error("H.264 sequence-header size overflow"))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| corrupt_error("H.264 sequence-header allocation failed"))?;
    for nal in config
        .sequence_parameter_sets()
        .iter()
        .chain(config.picture_parameter_sets())
    {
        bytes.extend_from_slice(&[0, 0, 0, 1]);
        bytes.extend_from_slice(nal);
    }
    Ok(bytes)
}

fn make_input_sample(
    bytes: &[u8],
    time_100ns: i64,
    duration_100ns: i64,
    is_sync: bool,
) -> Result<IMFSample, BackendError> {
    let length = u32::try_from(bytes.len())
        .map_err(|_| corrupt_error("H.264 access unit exceeds Media Foundation limits"))?;
    let buffer = unsafe { MFCreateMemoryBuffer(length) }
        .map_err(|error| windows_backend(error, "allocate H.264 input buffer"))?;
    // SAFETY: the locked buffer has at least `length` bytes. It is always
    // unlocked before this function returns.
    let mut destination = std::ptr::null_mut();
    let mut capacity = 0_u32;
    unsafe { buffer.Lock(&mut destination, Some(&mut capacity), None) }
        .map_err(|error| windows_backend(error, "lock H.264 input buffer"))?;
    if capacity < length || destination.is_null() {
        let _ = unsafe { buffer.Unlock() };
        return Err(windows_backend(
            WinError::new(E_FAIL, "Media Foundation input buffer too small"),
            "populate H.264 input buffer",
        ));
    }
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), destination, bytes.len()) };
    unsafe { buffer.Unlock() }
        .map_err(|error| windows_backend(error, "unlock H.264 input buffer"))?;
    unsafe { buffer.SetCurrentLength(length) }
        .map_err(|error| windows_backend(error, "commit H.264 input buffer length"))?;
    let sample = unsafe { MFCreateSample() }
        .map_err(|error| windows_backend(error, "create H.264 input sample"))?;
    // SAFETY: attaches the owned buffer and sets bounded timestamp metadata.
    unsafe { sample.AddBuffer(&buffer) }
        .map_err(|error| windows_backend(error, "attach H.264 input buffer"))?;
    unsafe { sample.SetSampleTime(time_100ns) }
        .map_err(|error| windows_backend(error, "set H.264 input timestamp"))?;
    unsafe { sample.SetSampleDuration(duration_100ns) }
        .map_err(|error| windows_backend(error, "set H.264 input duration"))?;
    unsafe { sample.SetUINT32(&MFSampleExtension_CleanPoint, u32::from(is_sync)) }
        .map_err(|error| windows_backend(error, "set H.264 input sync flag"))?;
    Ok(sample)
}

fn validate_annex_b(bytes: &[u8]) -> Result<(), BackendError> {
    if bytes.len() > crate::MAX_ANNEX_B_ACCESS_UNIT_BYTES {
        return Err(corrupt_error(format!(
            "H.264 access unit has {} bytes, limit is {}",
            bytes.len(),
            crate::MAX_ANNEX_B_ACCESS_UNIT_BYTES
        )));
    }
    let start_code_len = if bytes.starts_with(&[0, 0, 0, 1]) {
        4
    } else if bytes.starts_with(&[0, 0, 1]) {
        3
    } else {
        return Err(corrupt_error(
            "H.264 access unit does not begin with an Annex-B start code",
        ));
    };
    if bytes.len() <= start_code_len || bytes[start_code_len] & 0x1f == 0 {
        return Err(corrupt_error("H.264 access unit has an invalid first NAL"));
    }
    Ok(())
}

fn validate_output_texture(
    texture: &ID3D11Texture2D,
    width: u32,
    height: u32,
    subresource: u32,
) -> Result<(), BackendError> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: GetDesc fills the initialized descriptor.
    unsafe { texture.GetDesc(&mut desc) };
    if desc.Format != DXGI_FORMAT_NV12
        || desc.Width != width
        || desc.Height != height
        || subresource >= desc.ArraySize
    {
        return Err(BackendError {
            component: BackendComponent::VideoDecoder,
            kind: BackendErrorKind::DecoderFailure,
            recovery: RecoveryDisposition::RecreateComponent,
            native_code: None,
            message: format!(
                "invalid decoder texture: {}x{} {:?} array {}, subresource {subresource}",
                desc.Width, desc.Height, desc.Format, desc.ArraySize
            ),
        });
    }
    Ok(())
}

fn copy_system_nv12(
    media_buffer: &windows::Win32::Media::MediaFoundation::IMFMediaBuffer,
    destination: &mut [u8],
) -> Result<(), BackendError> {
    if let Ok(buffer_2d) = media_buffer.cast::<IMF2DBuffer>() {
        let contiguous_length = unsafe { buffer_2d.GetContiguousLength() }
            .map_err(|error| windows_backend(error, "query software decoder output length"))?;
        if usize::try_from(contiguous_length).ok() != Some(destination.len()) {
            return Err(BackendError {
                component: BackendComponent::VideoDecoder,
                kind: BackendErrorKind::DecoderFailure,
                recovery: RecoveryDisposition::RetryPipeline,
                native_code: None,
                message: format!(
                    "software decoder output has {contiguous_length} bytes, expected {}",
                    destination.len()
                ),
            });
        }
        unsafe { buffer_2d.ContiguousCopyTo(destination) }
            .map_err(|error| windows_backend(error, "copy software decoder NV12 output"))?;
        return Ok(());
    }

    let contiguous = unsafe { media_buffer.GetCurrentLength() }
        .map_err(|error| windows_backend(error, "query software decoder buffer length"))?;
    if usize::try_from(contiguous).ok() != Some(destination.len()) {
        return Err(BackendError {
            component: BackendComponent::VideoDecoder,
            kind: BackendErrorKind::DecoderFailure,
            recovery: RecoveryDisposition::RetryPipeline,
            native_code: None,
            message: format!(
                "software decoder buffer has {contiguous} bytes, expected {}",
                destination.len()
            ),
        });
    }
    let mut source = std::ptr::null_mut();
    let mut current = 0_u32;
    // SAFETY: lock returns a readable region whose current length was checked.
    unsafe { media_buffer.Lock(&mut source, None, Some(&mut current)) }
        .map_err(|error| windows_backend(error, "lock software decoder output"))?;
    if source.is_null() || usize::try_from(current).ok() != Some(destination.len()) {
        let _ = unsafe { media_buffer.Unlock() };
        return Err(BackendError {
            component: BackendComponent::VideoDecoder,
            kind: BackendErrorKind::DecoderFailure,
            recovery: RecoveryDisposition::RetryPipeline,
            native_code: None,
            message: "software decoder returned an invalid locked buffer".into(),
        });
    }
    unsafe { std::ptr::copy_nonoverlapping(source, destination.as_mut_ptr(), destination.len()) };
    unsafe { media_buffer.Unlock() }
        .map_err(|error| windows_backend(error, "unlock software decoder output"))?;
    Ok(())
}

fn crop_nv12(
    source: &[u8],
    source_width: u32,
    source_height: u32,
    destination: &mut [u8],
    visible_width: u32,
    visible_height: u32,
) -> Result<(), BackendError> {
    let source_width = usize::try_from(source_width)
        .map_err(|_| corrupt_error("coded NV12 width exceeds address space"))?;
    let source_height = usize::try_from(source_height)
        .map_err(|_| corrupt_error("coded NV12 height exceeds address space"))?;
    let visible_width = usize::try_from(visible_width)
        .map_err(|_| corrupt_error("visible NV12 width exceeds address space"))?;
    let visible_height = usize::try_from(visible_height)
        .map_err(|_| corrupt_error("visible NV12 height exceeds address space"))?;
    if visible_width > source_width
        || visible_height > source_height
        || visible_width % 2 != 0
        || visible_height % 2 != 0
    {
        return Err(corrupt_error("invalid NV12 visible crop"));
    }
    let expected_source = source_width
        .checked_mul(source_height)
        .and_then(|luma| luma.checked_add(luma / 2))
        .ok_or_else(|| corrupt_error("coded NV12 crop size overflow"))?;
    let expected_destination = visible_width
        .checked_mul(visible_height)
        .and_then(|luma| luma.checked_add(luma / 2))
        .ok_or_else(|| corrupt_error("visible NV12 crop size overflow"))?;
    if source.len() != expected_source || destination.len() != expected_destination {
        return Err(corrupt_error(
            "NV12 crop buffers do not match their dimensions",
        ));
    }

    for row in 0..visible_height {
        let source_start = row * source_width;
        let destination_start = row * visible_width;
        destination[destination_start..destination_start + visible_width]
            .copy_from_slice(&source[source_start..source_start + visible_width]);
    }
    let source_chroma = source_width * source_height;
    let destination_chroma = visible_width * visible_height;
    for row in 0..visible_height / 2 {
        let source_start = source_chroma + row * source_width;
        let destination_start = destination_chroma + row * visible_width;
        destination[destination_start..destination_start + visible_width]
            .copy_from_slice(&source[source_start..source_start + visible_width]);
    }
    Ok(())
}

fn nv12_size(width: u32, height: u32) -> Result<usize, BackendError> {
    usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|luma| luma.checked_add(luma / 2))
        .ok_or_else(|| corrupt_error("NV12 size overflow"))
}

fn align_up_16(value: u32) -> Result<u32, BackendError> {
    value
        .checked_add(15)
        .map(|value| value & !15)
        .ok_or_else(|| corrupt_error("H.264 coded dimension overflow"))
}

fn mf_alignment_mask(required: u32) -> Result<u32, BackendError> {
    let boundary = match required.checked_add(1) {
        Some(value) if value.is_power_of_two() => value,
        _ => required
            .checked_next_power_of_two()
            .ok_or_else(|| unavailable_error("Media Foundation output alignment overflow"))?,
    }
    .max(16);
    Ok(boundary - 1)
}

fn timeline_position_to_100ns(position: TimelinePosition) -> Result<i64, BackendError> {
    timeline_ticks_to_100ns(position.ticks())
}

fn timeline_duration_to_100ns(duration: TimelineDuration) -> Result<i64, BackendError> {
    timeline_ticks_to_100ns(duration.ticks()).map(|ticks| ticks.max(1))
}

fn timeline_ticks_to_100ns(ticks: u64) -> Result<i64, BackendError> {
    let value = u128::from(ticks)
        .checked_mul(10_000_000)
        .and_then(|scaled| scaled.checked_div(u128::from(PLAYBACK_TIMELINE_HZ)))
        .and_then(|scaled| i64::try_from(scaled).ok())
        .ok_or_else(|| corrupt_error("H.264 timestamp exceeds Media Foundation range"))?;
    Ok(value)
}

fn pack_pair(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}

fn device_adapter_luid(device: &PlaybackD3D11Device) -> Option<u64> {
    device.adapter_luid()
}

fn unavailable_error(message: impl Into<String>) -> BackendError {
    BackendError {
        component: BackendComponent::VideoDecoder,
        kind: BackendErrorKind::Unavailable,
        recovery: RecoveryDisposition::Fatal,
        native_code: None,
        message: message.into(),
    }
}

fn corrupt_error(message: impl Into<String>) -> BackendError {
    BackendError {
        component: BackendComponent::VideoDecoder,
        kind: BackendErrorKind::CorruptInput,
        recovery: RecoveryDisposition::RetryPipeline,
        native_code: Some(i64::from(E_INVALIDARG.0)),
        message: message.into(),
    }
}

fn stale_token_error(expected: PipelineToken, actual: PipelineToken) -> BackendError {
    BackendError {
        component: BackendComponent::VideoDecoder,
        kind: BackendErrorKind::DecoderFailure,
        recovery: RecoveryDisposition::RetryPipeline,
        native_code: None,
        message: format!("stale decoder token: expected {expected:?}, got {actual:?}"),
    }
}

pub fn classify_device_failure(native_code: i32) -> BackendError {
    let device_lost = [
        DXGI_ERROR_DEVICE_REMOVED.0,
        DXGI_ERROR_DEVICE_RESET.0,
        DXGI_ERROR_DEVICE_HUNG.0,
        DXGI_ERROR_DRIVER_INTERNAL_ERROR.0,
        MF_E_HW_MFT_FAILED_START_STREAMING.0,
    ]
    .contains(&native_code);
    BackendError {
        component: BackendComponent::VideoDecoder,
        kind: if device_lost {
            BackendErrorKind::DeviceLost
        } else {
            BackendErrorKind::DecoderFailure
        },
        recovery: if device_lost {
            RecoveryDisposition::RecreateComponent
        } else {
            RecoveryDisposition::RetryPipeline
        },
        native_code: Some(i64::from(native_code)),
        message: if device_lost {
            "Windows video decode device was lost".into()
        } else {
            "Windows video decoder failed".into()
        },
    }
}

fn windows_backend(error: WinError, context: &str) -> BackendError {
    let mut backend = classify_device_failure(error.code().0);
    backend.message = format!("{context}: {error}");
    backend
}
