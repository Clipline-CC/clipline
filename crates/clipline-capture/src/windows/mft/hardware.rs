use super::*;

pub struct MftH264Encoder {
    _activation: OwnedMftActivation,
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

/// Probe encode size. 640x360 rather than a tiny placeholder, matching the
/// FFmpeg probe and for the same reason: AMF rejects very small resolutions,
/// which would wrongly condemn a working encoder.
const HARDWARE_PROBE_WIDTH: u32 = 640;
const HARDWARE_PROBE_HEIGHT: u32 = 360;
/// Frames pushed through a probe encode.
///
/// One is not enough: an async MFT banks the first `ProcessInput` against its
/// NeedInput credit and returns `Ok` without touching the encoder, so Intel's
/// broken MFT on Alder Lake-N accepts frame 0 and only fails on frame 1. A
/// short run past that, then a drain, is what actually exercises the pump.
const HARDWARE_PROBE_FRAMES: u32 = 8;

/// Confirm a hardware H.264 MFT can actually encode on this machine.
///
/// Neither registration nor a successful open is proof. Intel's H.264 MFT on
/// Alder Lake-N enumerates, opens and accepts a frame, then fails with
/// `E_UNEXPECTED` once the pump asks it to do real work. By then the recorder
/// has committed, so the session dies mid-flight.
///
/// Success means the encoder survived [`HARDWARE_PROBE_FRAMES`] frames, drained
/// cleanly, and produced at least one packet — a silent encoder that never
/// emits anything is no more usable than one that errors.
///
/// Every failure path — no D3D device, no MFT for this backend, a failed encode
/// or drain — reports "unusable" rather than an error: probing must never fail
/// startup. All COM objects are released when this returns.
pub(crate) fn hardware_backend_can_encode(backend: EncoderBackend) -> bool {
    let Ok((device, _context)) = crate::windows::d3d11::create_device() else {
        return false;
    };
    let cfg = MftConfig {
        width: HARDWARE_PROBE_WIDTH,
        height: HARDWARE_PROBE_HEIGHT,
        fps: 30,
        bitrate_bps: 2_000_000,
        encoder_backend: Some(backend),
    };
    let Ok(mut encoder) =
        MftH264Encoder::new(&device, HARDWARE_PROBE_WIDTH, HARDWARE_PROBE_HEIGHT, cfg)
    else {
        return false;
    };
    let mut packets = 0usize;
    for frame_index in 0..HARDWARE_PROBE_FRAMES {
        let Ok(texture) = crate::windows::d3d11::create_bgra_texture(
            &device,
            HARDWARE_PROBE_WIDTH,
            HARDWARE_PROBE_HEIGHT,
        ) else {
            return false;
        };
        let frame = Frame {
            pts_s: f64::from(frame_index) / 30.0,
            data: FrameData::Gpu(texture),
        };
        match encoder.encode(&frame) {
            Ok(produced) => packets += produced.len(),
            Err(_) => return false,
        }
    }
    match encoder.finish() {
        Ok(produced) => packets + produced.len() > 0,
        Err(_) => false,
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
        let activation = OwnedMftActivation(activate.clone());

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
            _activation: activation,
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
            // Current Media Foundation documentation explicitly says ulParam
            // contains the specified input stream ID for the drain command.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::{Encoder, Frame, FrameData};

    /// Real hardware encode (AMF on the dev machine). CI-skipped: runners
    /// have no hardware encoder and MF behaves erratically there.
    ///
    /// Also skipped when probing finds no *working* hardware MFT. A machine can
    /// register one that opens and then fails its first frame (Intel Alder
    /// Lake-N); excluding those is the probe's job, and this test asserts what
    /// a functioning hardware encoder produces. It encodes with the advertised
    /// backend explicitly rather than letting `None` pick the first registered
    /// MFT, which would be the broken one on exactly those machines.
    #[test]
    fn encodes_synthetic_frames_to_keyframed_avcc() {
        if std::env::var_os("CI").is_some() {
            eprintln!("SKIP: hardware MFT test");
            return;
        }
        let advertised = mft_probe::enumerate().ok().and_then(|caps| {
            caps.into_iter()
                .find(|cap| cap.backend.is_hardware())
                .map(|cap| cap.backend)
        });
        let Some(backend) = advertised else {
            eprintln!("SKIP: no working hardware H.264 MFT on this machine");
            return;
        };
        let (device, _ctx) = crate::windows::d3d11::create_device().expect("device");
        let cfg = MftConfig {
            width: 640,
            height: 360,
            fps: 30,
            bitrate_bps: 2_000_000,
            encoder_backend: Some(backend),
        };
        let mut enc =
            MftH264Encoder::new(&device, 640, 360, cfg).expect("validated hardware MFT opens");
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
