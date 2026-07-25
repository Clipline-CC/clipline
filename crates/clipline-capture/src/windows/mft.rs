//! Hardware H.264 encoder via an async Media Foundation transform
//! (handoff milestone 2). Event-driven NeedInput/HaveOutput pump wrapped
//! behind the synchronous `Encoder` pull contract; D3D-aware input (NV12
//! textures straight from the video processor); Annex B output converted
//! to AVCC for clipline-mp4.

use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize, ICodecAPI,
    IMFDXGIDeviceManager, IMFMediaEventGenerator, IMFSample, IMFTransform, MEError,
    METransformDrainComplete, METransformHaveOutput, METransformNeedInput,
    MFCreateAlignedMemoryBuffer, MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer,
    MFCreateMediaType, MFCreateSample, MFMediaType_Video, MFNominalRange_16_235,
    MFSampleExtension_CleanPoint, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, MFVideoPrimaries_BT709, MFVideoTransFunc_709,
    MFVideoTransferMatrix_BT709, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_ENUM_FLAG_SYNCMFT, MFT_INPUT_STREAM_INFO, MFT_MESSAGE_COMMAND_DRAIN,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_INFO,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MF_EVENT_FLAG_NO_WAIT, MF_E_NOTACCEPTING,
    MF_E_NO_EVENTS_AVAILABLE, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
    MF_LOW_LATENCY, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_SUBTYPE,
    MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX,
    MF_TRANSFORM_ASYNC_UNLOCK,
};

use clipline_mp4::VideoTrackConfig;

use crate::annexb::{annexb_to_avcc, extract_sps_pps};
use crate::cpu_video::{CpuCropRect, CpuVideoConverter};
use crate::probe::EncoderBackend;
use crate::traits::{EncodeError, EncodedPacket, Encoder, Frame, FrameData};
use crate::windows::mft_probe;
use crate::windows::nv12::{CropRect, VideoConverter};

/// eAVEncH264VProfile_High (codecapi.h) — windows-rs feature placement of
/// the enum varies; the wire value is stable.
const H264_PROFILE_HIGH: u32 = 100;
const MFT_EVENT_TIMEOUT: Duration = Duration::from_secs(10);
const MFT_EVENT_POLL_INTERVAL: Duration = Duration::from_millis(2);

fn take_and_clear_manually_drop_option<T>(slot: &mut ManuallyDrop<Option<T>>) -> Option<T> {
    // SAFETY: the value is immediately replaced with `None`, so the owner can
    // still drop its field without releasing the moved value a second time.
    let value = unsafe { ManuallyDrop::take(slot) };
    *slot = ManuallyDrop::new(None);
    value
}

struct OwnedMftOutputBuffer {
    raw: MFT_OUTPUT_DATA_BUFFER,
}

impl OwnedMftOutputBuffer {
    fn new(output_id: u32) -> Self {
        Self::with_sample(output_id, None)
    }

    fn with_sample(output_id: u32, sample: Option<IMFSample>) -> Self {
        Self {
            raw: MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: output_id,
                pSample: ManuallyDrop::new(sample),
                ..Default::default()
            },
        }
    }

    fn raw_mut(&mut self) -> &mut MFT_OUTPUT_DATA_BUFFER {
        &mut self.raw
    }

    fn take_sample(&mut self) -> Option<IMFSample> {
        take_and_clear_manually_drop_option(&mut self.raw.pSample)
    }
}

impl Drop for OwnedMftOutputBuffer {
    fn drop(&mut self) {
        // SAFETY: ProcessOutput transfers these fields to its caller on every
        // result. A taken sample is replaced with None before this guard drops.
        unsafe {
            ManuallyDrop::drop(&mut self.raw.pSample);
            ManuallyDrop::drop(&mut self.raw.pEvents);
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MftConfig {
    /// Encode size; must already be even (`annexb::even_dimensions`).
    pub width: u32,
    pub height: u32,
    /// Nominal fps for media types + first-frame duration fallback.
    pub fps: u32,
    pub bitrate_bps: u32,
    /// None means automatic hardware H.264 selection.
    pub encoder_backend: Option<EncoderBackend>,
}

pub struct MftH264Encoder {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    converter: VideoConverter,
    // Keeps the device manager (and through it the device binding) alive.
    _device_manager: IMFDXGIDeviceManager,
    input_id: u32,
    output_id: u32,
    need_input_credits: u32,
    sps_pps: Option<(Vec<u8>, Vec<u8>)>,
    cfg: MftConfig,
    prev_pts_s: Option<f64>,
}

/// Microsoft's inbox synchronous H.264 MFT. It deliberately uses a CPU
/// BGRA -> NV12 conversion and system-memory samples so it remains available
/// when the machine has neither a hardware encoder nor a bundled FFmpeg.
pub struct SoftwareMftH264Encoder {
    transform: IMFTransform,
    device: ID3D11Device,
    converter: CpuVideoConverter,
    crop: Option<CpuCropRect>,
    input_width: u32,
    input_height: u32,
    input_id: u32,
    output_id: u32,
    input_size: u32,
    input_alignment: u32,
    output_info: MFT_OUTPUT_STREAM_INFO,
    sps_pps: Option<(Vec<u8>, Vec<u8>)>,
    cfg: MftConfig,
    prev_pts_s: Option<f64>,
}

fn backend(e: windows::core::Error) -> EncodeError {
    EncodeError::Backend(e.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MftEventKind {
    NeedInput,
    HaveOutput,
    DrainComplete,
    Error,
    Other(u32),
}

fn classify_mft_event_type(ty: u32) -> MftEventKind {
    if ty == METransformNeedInput.0 as u32 {
        MftEventKind::NeedInput
    } else if ty == METransformHaveOutput.0 as u32 {
        MftEventKind::HaveOutput
    } else if ty == METransformDrainComplete.0 as u32 {
        MftEventKind::DrainComplete
    } else if ty == MEError.0 as u32 {
        MftEventKind::Error
    } else {
        MftEventKind::Other(ty)
    }
}

fn mft_event_error(event: &windows::Win32::Media::MediaFoundation::IMFMediaEvent) -> EncodeError {
    match unsafe { event.GetStatus() } {
        Ok(status) if status.is_err() => EncodeError::Backend(format!(
            "MFT encoder event error: {}",
            windows::core::Error::from(status)
        )),
        Ok(_) => EncodeError::Backend("MFT encoder reported MEError".into()),
        Err(e) => backend(e),
    }
}

fn mft_unexpected_event_error(ty: u32) -> EncodeError {
    EncodeError::Backend(format!("MFT encoder unexpected event type {ty}"))
}

fn mft_event_timeout_error(waiting_for: &str) -> EncodeError {
    EncodeError::Backend(format!("MFT encoder timed out waiting for {waiting_for}"))
}

fn h264_activate(
    activates: &[windows::Win32::Media::MediaFoundation::IMFActivate],
    requested: Option<EncoderBackend>,
) -> Option<&windows::Win32::Media::MediaFoundation::IMFActivate> {
    match requested {
        // Forced backend: match on vendor ID. No fallback here — the app
        // service layer decides whether to retry as Automatic.
        Some(requested) => activates
            .iter()
            .find(|activate| mft_probe::backend_of(activate) == Some(requested)),
        // Automatic: trust MFTEnumEx merit order (SORTANDFILTER). A fixed
        // vendor priority risked preferring an adapter the capture D3D device
        // can't bind, and the Automatic arm has no retry path in the service.
        None => activates.first(),
    }
}

impl MftH264Encoder {
    /// `in_w`/`in_h` = capture frame size; `cfg` = encode parameters. With
    /// `cfg.encoder_backend = None` the first enumerated hardware H.264 MFT
    /// wins (MFTEnumEx sorts by merit); a set backend selects that vendor's MFT.
    pub fn new(
        device: &ID3D11Device,
        in_w: u32,
        in_h: u32,
        cfg: MftConfig,
    ) -> Result<Self, EncodeError> {
        Self::new_with_crop(device, in_w, in_h, cfg, None)
    }

    pub fn new_with_crop(
        device: &ID3D11Device,
        in_w: u32,
        in_h: u32,
        cfg: MftConfig,
        crop: Option<CropRect>,
    ) -> Result<Self, EncodeError> {
        crate::windows::d3d11::ensure_multithread_protected(device).map_err(backend)?;
        mft_probe::ensure_mf_started().map_err(backend)?;

        let activates = mft_probe::enum_activates(
            MFVideoFormat_H264,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
        )
        .map_err(backend)?;
        let activate = h264_activate(&activates, cfg.encoder_backend).ok_or_else(|| {
            match cfg.encoder_backend {
                Some(backend) => {
                    EncodeError::Backend(format!("selected H.264 encoder unavailable: {backend:?}"))
                }
                None => EncodeError::Backend("no hardware H.264 encoder MFT".into()),
            }
        })?;
        // SAFETY: activate is a valid IMFActivate from MFTEnumEx.
        let transform: IMFTransform = unsafe { activate.ActivateObject() }.map_err(backend)?;

        // Hardware encoder MFTs are async: unlock first, everything else after.
        let attrs = unsafe { transform.GetAttributes() }.map_err(backend)?;
        unsafe { attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }.map_err(backend)?;
        let _ = unsafe { attrs.SetUINT32(&MF_LOW_LATENCY, 1) };

        // D3D-aware input: hand the shared device over via the DXGI manager.
        let mut token = 0u32;
        let mut manager: Option<IMFDXGIDeviceManager> = None;
        // SAFETY: out-params are valid; manager set on Ok.
        unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager) }.map_err(backend)?;
        let manager = manager.expect("manager out-param set on Ok");
        unsafe { manager.ResetDevice(device, token) }.map_err(backend)?;
        // SAFETY: SET_D3D_MANAGER takes the manager as the ULONG_PTR param.
        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                .map_err(backend)?;
        }

        // Stream IDs (E_NOTIMPL ⇒ fixed 0/0 per MFT docs).
        let (mut in_ids, mut out_ids) = ([0u32; 1], [0u32; 1]);
        // SAFETY: arrays sized for one stream each (encoders are 1-in/1-out).
        let _ = unsafe { transform.GetStreamIDs(&mut in_ids, &mut out_ids) };
        let (input_id, output_id) = (in_ids[0], out_ids[0]);

        // Rate control must be configured BEFORE the output type. AMD's MFT
        // otherwise treats MF_MT_AVG_BITRATE as a peak hint and the stream
        // overshoots ~2x; setting CBR + mean bitrate here pins the real target.
        // (GOP/B-frames are set after the output type, which they tolerate.)
        if let Ok(codec_api) = transform.cast::<ICodecAPI>() {
            let rc_mode = variant_u32(RATE_CONTROL_MODE_CBR);
            let mean_bitrate = variant_u32(cfg.bitrate_bps);
            // SAFETY: SetValue with VT_UI4 variants per codecapi contract.
            unsafe {
                let _ = codec_api.SetValue(&CODECAPI_AVEncCommonRateControlMode, &rc_mode);
                let _ = codec_api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &mean_bitrate);
            }
        }

        // Output type first (encoder MFTs require it before input).
        let out_ty = unsafe { MFCreateMediaType() }.map_err(backend)?;
        // SAFETY: setters on a fresh media type.
        unsafe {
            out_ty
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(backend)?;
            out_ty
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
                .map_err(backend)?;
            out_ty
                .SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate_bps)
                .map_err(backend)?;
            out_ty
                .SetUINT64(
                    &MF_MT_FRAME_SIZE,
                    ((cfg.width as u64) << 32) | cfg.height as u64,
                )
                .map_err(backend)?;
            out_ty
                .SetUINT64(&MF_MT_FRAME_RATE, ((cfg.fps as u64) << 32) | 1)
                .map_err(backend)?;
            out_ty
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(backend)?;
            out_ty
                .SetUINT32(&MF_MT_MPEG2_PROFILE, H264_PROFILE_HIGH)
                .map_err(backend)?;
            set_rec709_limited_attrs(&out_ty).map_err(backend)?;
            transform
                .SetOutputType(output_id, &out_ty, 0)
                .map_err(backend)?;
        }

        // Input type: pick the NV12 candidate the MFT offers.
        let mut set_input = false;
        for i in 0.. {
            // SAFETY: index enumeration ends with MF_E_NO_MORE_TYPES.
            let Ok(ty) = (unsafe { transform.GetInputAvailableType(input_id, i) }) else {
                break;
            };
            let subtype = unsafe { ty.GetGUID(&MF_MT_SUBTYPE) }.map_err(backend)?;
            if subtype != MFVideoFormat_NV12 {
                continue;
            }
            // SAFETY: setters on the offered type, then SetInputType.
            unsafe {
                ty.SetUINT64(
                    &MF_MT_FRAME_SIZE,
                    ((cfg.width as u64) << 32) | cfg.height as u64,
                )
                .map_err(backend)?;
                ty.SetUINT64(&MF_MT_FRAME_RATE, ((cfg.fps as u64) << 32) | 1)
                    .map_err(backend)?;
                set_rec709_limited_attrs(&ty).map_err(backend)?;
                transform.SetInputType(input_id, &ty, 0).map_err(backend)?;
            }
            set_input = true;
            break;
        }
        if !set_input {
            return Err(EncodeError::Backend("MFT offers no NV12 input type".into()));
        }

        // GOP / B-frame knobs (best-effort — vendors vary). Rate control is
        // set earlier, before the output type. These tolerate being set here.
        if let Ok(codec_api) = transform.cast::<ICodecAPI>() {
            let gop = variant_u32(crate::replay_gop_frames(cfg.fps)); // ~0.5 s keyframe interval
            let zero = variant_u32(0);
            // SAFETY: SetValue with VT_UI4 variants per codecapi contract.
            unsafe {
                let _ = codec_api.SetValue(&CODECAPI_AVEncMPVGOPSize, &gop);
                let _ = codec_api.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &zero);
            }
        }

        // SPS/PPS attempt #1: the negotiated output type's sequence header.
        let mut sps_pps = None;
        if let Ok(cur) = unsafe { transform.GetOutputCurrentType(output_id) } {
            sps_pps = sequence_header_sps_pps(&cur);
        }

        let events: IMFMediaEventGenerator = transform.cast().map_err(backend)?;
        // SAFETY: standard streaming-start message sequence.
        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(backend)?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(backend)?;
        }

        let converter =
            VideoConverter::new_with_crop(device, in_w, in_h, cfg.width, cfg.height, crop)
                .map_err(|e| EncodeError::Backend(format!("NV12 converter: {e}")))?;

        Ok(Self {
            transform,
            events,
            converter,
            _device_manager: manager,
            input_id,
            output_id,
            need_input_credits: 0,
            sps_pps,
            cfg,
            prev_pts_s: None,
        })
    }

    /// Pull one encoded sample after METransformHaveOutput.
    fn drain_one(&mut self) -> Result<EncodedPacket, EncodeError> {
        loop {
            let mut out = OwnedMftOutputBuffer::new(self.output_id);
            let mut status = 0u32;
            // SAFETY: hardware MFTs provide their own samples (pSample None
            // in); `out` releases all returned fields on every result path.
            let res = unsafe {
                self.transform
                    .ProcessOutput(0, std::slice::from_mut(out.raw_mut()), &mut status)
            };
            match res {
                Ok(()) => {
                    let sample = out
                        .take_sample()
                        .ok_or_else(|| EncodeError::Backend("no sample on Ok".into()))?;
                    return self.packet_from_sample(&sample);
                }
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // Renegotiate and retry; refresh the sequence header.
                    // SAFETY: standard stream-change handling.
                    unsafe {
                        let ty = self
                            .transform
                            .GetOutputAvailableType(self.output_id, 0)
                            .map_err(backend)?;
                        set_rec709_limited_attrs(&ty).map_err(backend)?;
                        self.transform
                            .SetOutputType(self.output_id, &ty, 0)
                            .map_err(backend)?;
                        if self.sps_pps.is_none() {
                            self.sps_pps = sequence_header_sps_pps(&ty);
                        }
                    }
                }
                Err(e) => return Err(backend(e)),
            }
        }
    }

    fn packet_from_sample(&mut self, sample: &IMFSample) -> Result<EncodedPacket, EncodeError> {
        // SAFETY: standard buffer access: contiguous buffer, lock, copy, unlock.
        let annexb = unsafe {
            let buffer = sample.ConvertToContiguousBuffer().map_err(backend)?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buffer
                .Lock(&mut ptr, None, Some(&mut len))
                .map_err(backend)?;
            let bytes = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            buffer.Unlock().map_err(backend)?;
            bytes
        };
        if self.sps_pps.is_none() {
            self.sps_pps = extract_sps_pps(&annexb);
        }
        let nominal = 1.0 / self.cfg.fps as f64;
        // SAFETY: attribute getters on a valid sample.
        let (pts_s, duration_s, clean_point) = unsafe {
            (
                sample.GetSampleTime().map_err(backend)? as f64 / 1e7,
                sample
                    .GetSampleDuration()
                    .map(|d| d as f64 / 1e7)
                    .unwrap_or(nominal),
                sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) == 1,
            )
        };
        let is_keyframe = clean_point || crate::annexb::is_keyframe(&annexb);
        Ok(EncodedPacket {
            data: annexb_to_avcc(&annexb),
            pts_s,
            duration_s,
            is_keyframe,
        })
    }

    /// Pump pending events; feed `sample` when a NeedInput credit exists.
    /// `block` waits for the first event when no credit is banked.
    fn pump(&mut self, packets: &mut Vec<EncodedPacket>, block: bool) -> Result<(), EncodeError> {
        let wait_started = Instant::now();
        loop {
            // SAFETY: GetEvent on a valid generator; NO_WAIT yields
            // MF_E_NO_EVENTS_AVAILABLE when drained.
            match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => {
                    let ty = unsafe { event.GetType() }.map_err(backend)?;
                    match classify_mft_event_type(ty) {
                        MftEventKind::NeedInput => {
                            self.need_input_credits += 1;
                            if block {
                                return Ok(());
                            }
                        }
                        MftEventKind::HaveOutput => packets.push(self.drain_one()?),
                        MftEventKind::Error => return Err(mft_event_error(&event)),
                        MftEventKind::DrainComplete => return Err(mft_unexpected_event_error(ty)),
                        MftEventKind::Other(ty) => return Err(mft_unexpected_event_error(ty)),
                    }
                }
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE && !block => return Ok(()),
                Err(e)
                    if e.code() == MF_E_NO_EVENTS_AVAILABLE
                        && wait_started.elapsed() >= MFT_EVENT_TIMEOUT =>
                {
                    return Err(mft_event_timeout_error("an encoder event"));
                }
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    std::thread::sleep(MFT_EVENT_POLL_INTERVAL);
                }
                Err(e) => return Err(backend(e)),
            }
            if block && self.need_input_credits > 0 {
                return Ok(());
            }
        }
    }
}

impl Encoder for MftH264Encoder {
    fn encode(&mut self, frame: &Frame) -> Result<Vec<EncodedPacket>, EncodeError> {
        let FrameData::Gpu(bgra) = &frame.data else {
            return Err(EncodeError::Backend("MFT encoder needs GPU frames".into()));
        };
        let nv12 = self
            .converter
            .convert(bgra)
            .map_err(|e| EncodeError::Backend(format!("NV12 convert: {e}")))?;

        // VRR-friendly duration: previous-interval delta, nominal for the
        // first frame (ddoc §6: derive PTS from stamps, not fixed cadence).
        let nominal = 1.0 / self.cfg.fps as f64;
        let duration_s = self
            .prev_pts_s
            .map(|p| (frame.pts_s - p).max(1e-4))
            .unwrap_or(nominal);
        self.prev_pts_s = Some(frame.pts_s);

        // SAFETY: sample construction from a live NV12 texture on the
        // shared device; subtype index 0.
        let sample = unsafe {
            let sample = MFCreateSample().map_err(backend)?;
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &nv12, 0, false)
                .map_err(backend)?;
            sample.AddBuffer(&buffer).map_err(backend)?;
            sample
                .SetSampleTime((frame.pts_s * 1e7).round() as i64)
                .map_err(backend)?;
            sample
                .SetSampleDuration((duration_s * 1e7).round() as i64)
                .map_err(backend)?;
            sample
        };

        let mut packets = Vec::new();
        while self.need_input_credits == 0 {
            self.pump(&mut packets, true)?;
        }
        self.need_input_credits -= 1;
        // SAFETY: ProcessInput after a NeedInput event, per async MFT contract.
        unsafe { self.transform.ProcessInput(self.input_id, &sample, 0) }.map_err(backend)?;
        // Opportunistically collect whatever is already done.
        self.pump(&mut packets, false)?;
        Ok(packets)
    }

    fn track_config(&self) -> VideoTrackConfig {
        let (sps, pps) = self.sps_pps.clone().unwrap_or_default();
        VideoTrackConfig::h264(
            self.cfg.width as u16,
            self.cfg.height as u16,
            90_000,
            sps,
            pps,
        )
    }

    fn finish(&mut self) -> Result<Vec<EncodedPacket>, EncodeError> {
        // SAFETY: end-of-stream + drain message pair, then pump until
        // METransformDrainComplete.
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, self.input_id as usize)
                .map_err(backend)?;
            self.transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, self.input_id as usize)
                .map_err(backend)?;
        }
        let mut packets = Vec::new();
        let mut wait_started = Instant::now();
        loop {
            // SAFETY: GetEvent on a valid generator; poll with a bounded wait
            // so a wedged hardware MFT can surface as an encoder error.
            match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => {
                    wait_started = Instant::now();
                    let ty = unsafe { event.GetType() }.map_err(backend)?;
                    match classify_mft_event_type(ty) {
                        MftEventKind::HaveOutput => packets.push(self.drain_one()?),
                        MftEventKind::DrainComplete => break,
                        MftEventKind::NeedInput => {}
                        MftEventKind::Error => return Err(mft_event_error(&event)),
                        MftEventKind::Other(ty) => return Err(mft_unexpected_event_error(ty)),
                    }
                }
                Err(e)
                    if e.code() == MF_E_NO_EVENTS_AVAILABLE
                        && wait_started.elapsed() >= MFT_EVENT_TIMEOUT =>
                {
                    return Err(mft_event_timeout_error("drain completion"));
                }
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    std::thread::sleep(MFT_EVENT_POLL_INTERVAL);
                }
                Err(e) => return Err(backend(e)),
            }
        }
        Ok(packets)
    }
}

impl SoftwareMftH264Encoder {
    pub fn new(
        device: &ID3D11Device,
        in_w: u32,
        in_h: u32,
        cfg: MftConfig,
    ) -> Result<Self, EncodeError> {
        Self::new_with_crop(device, in_w, in_h, cfg, None)
    }

    pub fn new_with_crop(
        device: &ID3D11Device,
        in_w: u32,
        in_h: u32,
        cfg: MftConfig,
        crop: Option<CropRect>,
    ) -> Result<Self, EncodeError> {
        if cfg
            .encoder_backend
            .is_some_and(|backend| backend != EncoderBackend::MfSoftware)
        {
            return Err(EncodeError::Backend(
                "synchronous MFT requires the MfSoftware backend".into(),
            ));
        }
        crate::windows::d3d11::ensure_multithread_protected(device).map_err(backend)?;
        mft_probe::ensure_mf_started().map_err(backend)?;

        let activates = mft_probe::enum_activates(
            MFVideoFormat_H264,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
        )
        .map_err(backend)?;
        let activate = activates
            .iter()
            .find(|activate| mft_probe::is_microsoft_software_h264(activate))
            .ok_or_else(|| EncodeError::Backend("no software H.264 encoder MFT".into()))?;
        // SAFETY: activate is a valid IMFActivate returned by MFTEnumEx.
        let transform: IMFTransform = unsafe { activate.ActivateObject() }.map_err(backend)?;
        if let Ok(attrs) = unsafe { transform.GetAttributes() } {
            let _ = unsafe { attrs.SetUINT32(&MF_LOW_LATENCY, 1) };
        }

        // Encoders are one-input/one-output. E_NOTIMPL leaves the documented
        // fixed stream IDs (zero) in place.
        let (mut in_ids, mut out_ids) = ([0u32; 1], [0u32; 1]);
        let _ = unsafe { transform.GetStreamIDs(&mut in_ids, &mut out_ids) };
        let (input_id, output_id) = (in_ids[0], out_ids[0]);

        // The inbox software encoder reads these properties when the output
        // type is committed. Its B-frame default is zero, but set it
        // explicitly because clipline-mp4 intentionally has no ctts table.
        if let Ok(codec_api) = transform.cast::<ICodecAPI>() {
            let rc_mode = variant_u32(RATE_CONTROL_MODE_CBR);
            let mean_bitrate = variant_u32(cfg.bitrate_bps);
            let gop = variant_u32(crate::replay_gop_frames(cfg.fps));
            let zero = variant_u32(0);
            // SAFETY: these codec properties take VT_UI4 values.
            unsafe {
                let _ = codec_api.SetValue(&CODECAPI_AVEncCommonRateControlMode, &rc_mode);
                let _ = codec_api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &mean_bitrate);
                let _ = codec_api.SetValue(&CODECAPI_AVEncMPVGOPSize, &gop);
                let _ = codec_api.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &zero);
            }
        }

        // Media Foundation encoder MFTs require the H.264 output type before
        // the uncompressed NV12 input type.
        let out_ty = unsafe { MFCreateMediaType() }.map_err(backend)?;
        unsafe {
            out_ty
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(backend)?;
            out_ty
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
                .map_err(backend)?;
            out_ty
                .SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate_bps)
                .map_err(backend)?;
            out_ty
                .SetUINT64(
                    &MF_MT_FRAME_SIZE,
                    ((cfg.width as u64) << 32) | cfg.height as u64,
                )
                .map_err(backend)?;
            out_ty
                .SetUINT64(&MF_MT_FRAME_RATE, ((cfg.fps as u64) << 32) | 1)
                .map_err(backend)?;
            out_ty
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(backend)?;
            out_ty
                .SetUINT32(&MF_MT_MPEG2_PROFILE, H264_PROFILE_HIGH)
                .map_err(backend)?;
            set_rec709_limited_attrs(&out_ty).map_err(backend)?;
            transform
                .SetOutputType(output_id, &out_ty, 0)
                .map_err(backend)?;
        }

        let mut set_input = false;
        for i in 0.. {
            let Ok(ty) = (unsafe { transform.GetInputAvailableType(input_id, i) }) else {
                break;
            };
            let subtype = unsafe { ty.GetGUID(&MF_MT_SUBTYPE) }.map_err(backend)?;
            if subtype != MFVideoFormat_NV12 {
                continue;
            }
            unsafe {
                ty.SetUINT64(
                    &MF_MT_FRAME_SIZE,
                    ((cfg.width as u64) << 32) | cfg.height as u64,
                )
                .map_err(backend)?;
                ty.SetUINT64(&MF_MT_FRAME_RATE, ((cfg.fps as u64) << 32) | 1)
                    .map_err(backend)?;
                ty.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                    .map_err(backend)?;
                set_rec709_limited_attrs(&ty).map_err(backend)?;
                transform.SetInputType(input_id, &ty, 0).map_err(backend)?;
            }
            set_input = true;
            break;
        }
        if !set_input {
            return Err(EncodeError::Backend(
                "software MFT offers no NV12 input type".into(),
            ));
        }

        let mut input_info = MFT_INPUT_STREAM_INFO::default();
        unsafe {
            transform
                .GetInputStreamInfo(input_id, &mut input_info)
                .map_err(backend)?;
        }
        let input_alignment = mf_alignment_mask(input_info.cbAlignment)?;
        let input_size = input_info.cbSize;
        let output_info = unsafe { transform.GetOutputStreamInfo(output_id) }.map_err(backend)?;
        let mut sps_pps = None;
        if let Ok(current) = unsafe { transform.GetOutputCurrentType(output_id) } {
            sps_pps = sequence_header_sps_pps(&current);
        }

        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(backend)?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(backend)?;
        }

        let crop = crop.map(|rect| CpuCropRect {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        });
        let converter = CpuVideoConverter::new(in_w, in_h, crop, cfg.width, cfg.height)
            .map_err(|e| EncodeError::Backend(format!("CPU nv12 converter: {e}")))?;

        Ok(Self {
            transform,
            device: device.clone(),
            converter,
            crop,
            input_width: in_w,
            input_height: in_h,
            input_id,
            output_id,
            input_size,
            input_alignment,
            output_info,
            sps_pps,
            cfg,
            prev_pts_s: None,
        })
    }

    fn convert(&mut self, texture: &ID3D11Texture2D) -> Result<Vec<u8>, EncodeError> {
        let bgra = crate::windows::nv12::read_bgra(&self.device, texture)
            .map_err(|e| EncodeError::Backend(format!("BGRA readback: {e}")))?;
        if (bgra.width, bgra.height) != (self.input_width, self.input_height) {
            self.converter = CpuVideoConverter::new(
                bgra.width,
                bgra.height,
                self.crop,
                self.cfg.width,
                self.cfg.height,
            )
            .map_err(|e| EncodeError::Backend(format!("CPU nv12 converter resize: {e}")))?;
            self.input_width = bgra.width;
            self.input_height = bgra.height;
        }
        self.converter
            .convert(&bgra.bytes, bgra.stride)
            .map_err(|e| EncodeError::Backend(format!("CPU nv12 convert: {e}")))
    }

    fn input_sample(
        &self,
        nv12: &[u8],
        pts_s: f64,
        duration_s: f64,
    ) -> Result<IMFSample, EncodeError> {
        let nv12_length = u32::try_from(nv12.len())
            .map_err(|_| EncodeError::Backend("NV12 sample is too large".into()))?;
        let sample_length = nv12_length.max(self.input_size);
        // SAFETY: the allocated IMFMediaBuffer owns `sample_length` bytes.
        // The lock is paired with Unlock on both success and bounds errors.
        unsafe {
            let buffer = MFCreateAlignedMemoryBuffer(sample_length, self.input_alignment)
                .map_err(backend)?;
            let mut ptr = std::ptr::null_mut();
            let mut capacity = 0u32;
            buffer
                .Lock(&mut ptr, Some(&mut capacity), None)
                .map_err(backend)?;
            let copy_result = if ptr.is_null() || capacity < sample_length {
                Err(EncodeError::Backend(
                    "Media Foundation input buffer is too small".into(),
                ))
            } else {
                std::ptr::write_bytes(ptr, 0, sample_length as usize);
                std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr, nv12.len());
                Ok(())
            };
            let unlock_result = buffer.Unlock().map_err(backend);
            copy_result?;
            unlock_result?;
            buffer.SetCurrentLength(sample_length).map_err(backend)?;

            let sample = MFCreateSample().map_err(backend)?;
            sample.AddBuffer(&buffer).map_err(backend)?;
            sample
                .SetSampleTime((pts_s * 1e7).round() as i64)
                .map_err(backend)?;
            sample
                .SetSampleDuration((duration_s * 1e7).round() as i64)
                .map_err(backend)?;
            Ok(sample)
        }
    }

    fn output_buffer(&mut self) -> Result<OwnedMftOutputBuffer, EncodeError> {
        // Stream-info allocation requirements are allowed to change after
        // ProcessOutput, even without a media-type change.
        self.output_info =
            unsafe { self.transform.GetOutputStreamInfo(self.output_id) }.map_err(backend)?;
        let mft_allocates = self.output_info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
            != 0;
        if mft_allocates {
            return Ok(OwnedMftOutputBuffer::new(self.output_id));
        }
        if self.output_info.cbSize == 0 {
            return Err(EncodeError::Backend(
                "software MFT requested a zero-sized output buffer".into(),
            ));
        }
        let alignment = mf_alignment_mask(self.output_info.cbAlignment)?;
        // SAFETY: the sample owns a buffer sized according to
        // GetOutputStreamInfo, as required for caller-allocated output.
        let sample = unsafe {
            let sample = MFCreateSample().map_err(backend)?;
            let buffer =
                MFCreateAlignedMemoryBuffer(self.output_info.cbSize, alignment).map_err(backend)?;
            sample.AddBuffer(&buffer).map_err(backend)?;
            sample
        };
        Ok(OwnedMftOutputBuffer::with_sample(
            self.output_id,
            Some(sample),
        ))
    }

    fn renegotiate_output(&mut self) -> Result<(), EncodeError> {
        // SAFETY: stream-change handling follows the MFT contract: select an
        // offered output type and then refresh its allocation requirements.
        unsafe {
            let ty = self
                .transform
                .GetOutputAvailableType(self.output_id, 0)
                .map_err(backend)?;
            set_rec709_limited_attrs(&ty).map_err(backend)?;
            self.transform
                .SetOutputType(self.output_id, &ty, 0)
                .map_err(backend)?;
            if let Some(header) = sequence_header_sps_pps(&ty) {
                self.sps_pps = Some(header);
            }
            self.output_info = self
                .transform
                .GetOutputStreamInfo(self.output_id)
                .map_err(backend)?;
        }
        Ok(())
    }

    fn drain_available(&mut self) -> Result<Vec<EncodedPacket>, EncodeError> {
        let mut packets = Vec::new();
        loop {
            let mut out = self.output_buffer()?;
            let mut status = 0u32;
            let result = unsafe {
                self.transform
                    .ProcessOutput(0, std::slice::from_mut(out.raw_mut()), &mut status)
            };
            match result {
                Ok(()) => {
                    let sample = out.take_sample().ok_or_else(|| {
                        EncodeError::Backend("software MFT returned no sample on Ok".into())
                    })?;
                    packets.push(self.packet_from_sample(&sample)?);
                }
                Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    self.renegotiate_output()?;
                }
                Err(error) => return Err(backend(error)),
            }
        }
        Ok(packets)
    }

    fn packet_from_sample(&mut self, sample: &IMFSample) -> Result<EncodedPacket, EncodeError> {
        // SAFETY: standard contiguous-buffer lock/copy/unlock sequence.
        let annexb = unsafe {
            let buffer = sample.ConvertToContiguousBuffer().map_err(backend)?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buffer
                .Lock(&mut ptr, None, Some(&mut len))
                .map_err(backend)?;
            let bytes = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            buffer.Unlock().map_err(backend)?;
            bytes
        };
        if self.sps_pps.is_none() {
            self.sps_pps = extract_sps_pps(&annexb);
        }
        let nominal = 1.0 / self.cfg.fps as f64;
        let (pts_s, duration_s, clean_point) = unsafe {
            (
                sample.GetSampleTime().map_err(backend)? as f64 / 1e7,
                sample
                    .GetSampleDuration()
                    .map(|duration| duration as f64 / 1e7)
                    .unwrap_or(nominal),
                sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) == 1,
            )
        };
        let is_keyframe = clean_point || crate::annexb::is_keyframe(&annexb);
        Ok(EncodedPacket {
            data: annexb_to_avcc(&annexb),
            pts_s,
            duration_s,
            is_keyframe,
        })
    }
}

impl Encoder for SoftwareMftH264Encoder {
    fn encode(&mut self, frame: &Frame) -> Result<Vec<EncodedPacket>, EncodeError> {
        let FrameData::Gpu(texture) = &frame.data else {
            return Err(EncodeError::Backend(
                "software MFT encoder needs GPU capture frames".into(),
            ));
        };
        let nv12 = self.convert(texture)?;
        let nominal = 1.0 / self.cfg.fps as f64;
        let duration_s = self
            .prev_pts_s
            .map(|previous| (frame.pts_s - previous).max(1e-4))
            .unwrap_or(nominal);
        self.prev_pts_s = Some(frame.pts_s);
        let sample = self.input_sample(&nv12, frame.pts_s, duration_s)?;

        let mut packets = Vec::new();
        let first_input = unsafe { self.transform.ProcessInput(self.input_id, &sample, 0) };
        match first_input {
            Ok(()) => {}
            Err(error) if error.code() == MF_E_NOTACCEPTING => {
                packets.extend(self.drain_available()?);
                unsafe {
                    self.transform
                        .ProcessInput(self.input_id, &sample, 0)
                        .map_err(backend)?;
                }
            }
            Err(error) => return Err(backend(error)),
        }
        packets.extend(self.drain_available()?);
        Ok(packets)
    }

    fn track_config(&self) -> VideoTrackConfig {
        let (sps, pps) = self.sps_pps.clone().unwrap_or_default();
        VideoTrackConfig::h264(
            self.cfg.width as u16,
            self.cfg.height as u16,
            90_000,
            sps,
            pps,
        )
    }

    fn finish(&mut self) -> Result<Vec<EncodedPacket>, EncodeError> {
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, self.input_id as usize)
                .map_err(backend)?;
            self.transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, self.input_id as usize)
                .map_err(backend)?;
        }
        self.drain_available()
    }
}

/// `MFCreateAlignedMemoryBuffer` takes an alignment mask (`boundary - 1`).
/// Stream-info values seen in the wild are documented masks, but accepting a
/// power-of-two boundary as well keeps third-party synchronous MFTs safe.
fn mf_alignment_mask(required: u32) -> Result<u32, EncodeError> {
    let boundary = match required.checked_add(1) {
        Some(value) if value.is_power_of_two() => value,
        _ => required.checked_next_power_of_two().ok_or_else(|| {
            EncodeError::Backend("Media Foundation buffer alignment overflow".into())
        })?,
    }
    .max(16);
    Ok(boundary - 1)
}

/// VT_UI4 VARIANT for ICodecAPI (no Drop needed for plain integers).
fn variant_u32(value: u32) -> windows::Win32::System::Variant::VARIANT {
    use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_UI4};
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_UI4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { ulVal: value },
            }),
        },
    }
}

/// eAVEncCommonRateControlMode_CBR (codecapi.h).
const RATE_CONTROL_MODE_CBR: u32 = 0;

fn set_rec709_limited_attrs(
    media_type: &windows::Win32::Media::MediaFoundation::IMFMediaType,
) -> windows::core::Result<()> {
    unsafe {
        media_type.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        media_type.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        media_type.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
        media_type.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
    }
    Ok(())
}

fn sequence_header_sps_pps(
    media_type: &windows::Win32::Media::MediaFoundation::IMFMediaType,
) -> Option<(Vec<u8>, Vec<u8>)> {
    // SAFETY: blob getters with a correctly sized out buffer.
    unsafe {
        let len = media_type.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER).ok()?;
        let mut blob = vec![0u8; len as usize];
        media_type
            .GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut blob, None)
            .ok()?;
        extract_sps_pps(&blob)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::{Encoder, Frame, FrameData};
    use std::cell::Cell;
    use std::rc::Rc;

    struct DropSpy(Rc<Cell<usize>>);

    impl Drop for DropSpy {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[test]
    fn taking_a_manually_dropped_option_clears_its_owner_slot() {
        let drops = Rc::new(Cell::new(0));
        let mut slot = ManuallyDrop::new(Some(DropSpy(drops.clone())));

        let value = take_and_clear_manually_drop_option(&mut slot).expect("owned value");
        assert!((*slot).is_none());
        assert_eq!(drops.get(), 0);

        drop(value);
        assert_eq!(drops.get(), 1);
        unsafe { ManuallyDrop::drop(&mut slot) };
        assert_eq!(
            drops.get(),
            1,
            "cleared owner must not double-drop the value"
        );

        let mut untouched = ManuallyDrop::new(Some(DropSpy(drops.clone())));
        unsafe { ManuallyDrop::drop(&mut untouched) };
        assert_eq!(drops.get(), 2, "untaken owner must release its value once");
    }

    #[test]
    fn classifies_mft_error_event_as_error() {
        assert_eq!(
            classify_mft_event_type(MEError.0 as u32),
            MftEventKind::Error
        );
        assert_eq!(
            classify_mft_event_type(METransformNeedInput.0 as u32),
            MftEventKind::NeedInput
        );
        assert_eq!(
            classify_mft_event_type(METransformHaveOutput.0 as u32),
            MftEventKind::HaveOutput
        );
        assert_eq!(
            classify_mft_event_type(METransformDrainComplete.0 as u32),
            MftEventKind::DrainComplete
        );
        assert_eq!(
            classify_mft_event_type(0xFFFF_FFFE),
            MftEventKind::Other(0xFFFF_FFFE)
        );
    }

    /// The inbox synchronous H.264 MFT must be usable whenever probing
    /// advertises it. Unlike the hardware test below, this uses WARP and must
    /// not blanket-skip on CI: it is the no-hardware/no-FFmpeg fallback.
    #[test]
    fn advertised_software_mft_encodes_warp_frames() {
        let advertised = mft_probe::enumerate()
            .map(|caps| {
                caps.iter().any(|cap| {
                    cap.api == crate::probe::EncoderApi::Mft
                        && cap.backend == EncoderBackend::MfSoftware
                        && cap.codecs.contains(&crate::probe::Codec::H264)
                })
            })
            .unwrap_or(false);
        if !advertised {
            eprintln!("SKIP: synchronous Media Foundation H.264 encoder unavailable");
            return;
        }

        let (device, _ctx) = crate::windows::d3d11::create_device_for_tests().expect("WARP device");
        let cfg = MftConfig {
            width: 640,
            height: 360,
            fps: 30,
            bitrate_bps: 2_000_000,
            encoder_backend: Some(EncoderBackend::MfSoftware),
        };
        let mut enc =
            SoftwareMftH264Encoder::new(&device, 640, 360, cfg).expect("software H.264 MFT");
        let texture =
            crate::windows::d3d11::create_bgra_texture(&device, 640, 360).expect("BGRA texture");
        let mut packets = Vec::new();
        let mut input_pts = Vec::new();
        let mut pts_s = 0.0;
        for i in 0..30 {
            input_pts.push(pts_s);
            packets.extend(
                enc.encode(&Frame {
                    pts_s,
                    data: FrameData::Gpu(texture.clone()),
                })
                .expect("encode software frame"),
            );
            pts_s += match i % 3 {
                0 => 1.0 / 60.0,
                1 => 1.0 / 30.0,
                _ => 1.0 / 24.0,
            };
        }
        packets.extend(enc.finish().expect("drain software encoder"));

        assert_eq!(packets.len(), input_pts.len(), "finish returns every frame");
        assert!(packets[0].is_keyframe, "stream starts with IDR");
        assert!(packets[0].data.len() > 4);
        assert_ne!(
            &packets[0].data[..4],
            &[0, 0, 0, 1],
            "samples are AVCC, not Annex B"
        );
        let track = enc.track_config();
        match &track.codec {
            clipline_mp4::VideoCodecParams::H264 { sps, pps } => {
                assert!(!sps.is_empty() && !pps.is_empty(), "SPS/PPS extracted");
            }
            other => panic!("software MFT must report H.264, got {other:?}"),
        }
        assert_eq!((track.width, track.height), (640, 360));
        for (index, (packet, input_pts_s)) in packets.iter().zip(input_pts.iter()).enumerate() {
            assert!(
                (packet.pts_s - input_pts_s).abs() < 1e-6,
                "packet {index} preserves its irregular input timestamp"
            );
            let expected_duration = if index == 0 {
                1.0 / 30.0
            } else {
                input_pts[index] - input_pts[index - 1]
            };
            assert!(
                (packet.duration_s - expected_duration).abs() < 1e-6,
                "packet {index} preserves its input duration"
            );
        }
    }

    /// Real hardware encode (AMF on the dev machine). CI-skipped: runners
    /// have no hardware encoder and MF behaves erratically there.
    #[test]
    fn encodes_synthetic_frames_to_keyframed_avcc() {
        if std::env::var_os("CI").is_some() {
            eprintln!("SKIP: hardware MFT test");
            return;
        }
        let (device, _ctx) = crate::windows::d3d11::create_device().expect("device");
        let cfg = MftConfig {
            width: 640,
            height: 360,
            fps: 30,
            bitrate_bps: 2_000_000,
            encoder_backend: None,
        };
        let mut enc = match MftH264Encoder::new(&device, 640, 360, cfg) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("SKIP: no hardware H.264 MFT: {e}");
                return;
            }
        };
        let mut packets = Vec::new();
        for i in 0..30 {
            let tex = crate::windows::d3d11::create_bgra_texture(&device, 640, 360).unwrap();
            let frame = Frame {
                pts_s: i as f64 / 30.0,
                data: FrameData::Gpu(tex),
            };
            packets.extend(enc.encode(&frame).unwrap());
        }
        packets.extend(enc.finish().unwrap());
        assert!(
            packets.len() >= 25,
            "most frames came back (got {})",
            packets.len()
        );
        assert!(packets[0].is_keyframe, "stream starts with IDR");
        // AVCC: first 4 bytes are a NAL length, not an Annex B start code.
        let first = &packets[0].data;
        assert!(first.len() > 4);
        assert_ne!(&first[..4], &[0, 0, 0, 1], "no Annex B start codes");
        let track = enc.track_config();
        match &track.codec {
            clipline_mp4::VideoCodecParams::H264 { sps, pps } => {
                assert!(!sps.is_empty() && !pps.is_empty(), "SPS/PPS extracted");
            }
            other => panic!("MFT encoder must report H.264, got {other:?}"),
        }
        assert_eq!((track.width, track.height), (640, 360));
        let mono = packets.windows(2).all(|w| w[1].pts_s >= w[0].pts_s);
        assert!(mono, "pts monotonic (B-frames disabled)");
    }
}
