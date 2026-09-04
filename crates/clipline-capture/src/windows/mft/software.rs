use super::*;

/// Microsoft's inbox synchronous H.264 MFT. It deliberately uses a CPU
/// BGRA -> NV12 conversion and system-memory samples so it remains available
/// when the machine has neither a hardware encoder nor a bundled FFmpeg.
pub struct SoftwareMftH264Encoder {
    _activation: OwnedMftActivation,
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
    output_sample: Option<ReusableMftOutputSample>,
    sps_pps: Option<(Vec<u8>, Vec<u8>)>,
    cfg: MftConfig,
    prev_pts_s: Option<f64>,
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
        let activation = OwnedMftActivation(activate.clone());
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
            _activation: activation,
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
            output_sample: None,
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
            self.output_sample = None;
            return Ok(OwnedMftOutputBuffer::new(self.output_id));
        }
        if self.output_info.cbSize == 0 {
            return Err(EncodeError::Backend(
                "software MFT requested a zero-sized output buffer".into(),
            ));
        }
        let alignment = mf_alignment_mask(self.output_info.cbAlignment)?;
        let needs_sample = self.output_sample.as_ref().is_none_or(|sample| {
            sample.capacity < self.output_info.cbSize || sample.alignment != alignment
        });
        if needs_sample {
            // SAFETY: the sample owns a buffer sized according to
            // GetOutputStreamInfo, as required for caller-allocated output.
            let sample = unsafe {
                let sample = MFCreateSample().map_err(backend)?;
                let buffer = MFCreateAlignedMemoryBuffer(self.output_info.cbSize, alignment)
                    .map_err(backend)?;
                sample.AddBuffer(&buffer).map_err(backend)?;
                sample
            };
            self.output_sample = Some(ReusableMftOutputSample {
                sample,
                capacity: self.output_info.cbSize,
                alignment,
            });
        }
        let sample = self
            .output_sample
            .as_ref()
            .expect("caller-allocated output sample initialized");
        // Reuse is safe only after the previous packet has been copied out.
        // Clear sample attributes (notably CleanPoint) and reset the buffer's
        // logical length before handing the retained allocation back to the MFT.
        unsafe {
            sample.sample.DeleteAllItems().map_err(backend)?;
            sample
                .sample
                .GetBufferByIndex(0)
                .map_err(backend)?
                .SetCurrentLength(0)
                .map_err(backend)?;
        }
        Ok(OwnedMftOutputBuffer::with_sample(
            self.output_id,
            Some(sample.sample.clone()),
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
        self.output_sample = None;
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
        // Match the hardware MFT path's VRR convention: each frame carries
        // the interval preceding it, with nominal duration on the first frame.
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
            // Current Media Foundation documentation explicitly says ulParam
            // contains the specified input stream ID for the drain command.
            self.transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, self.input_id as usize)
                .map_err(backend)?;
        }
        self.drain_available()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::{Encoder, Frame, FrameData};

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
        let caller_supplies_output = enc.output_info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
            == 0;
        if caller_supplies_output {
            let mut first = enc.output_buffer().expect("first output buffer");
            let first_sample = first.take_sample().expect("first caller sample");
            let first_identity = first_sample.as_raw();
            drop(first_sample);

            let mut second = enc.output_buffer().expect("reused output buffer");
            let second_sample = second.take_sample().expect("second caller sample");
            assert_eq!(
                second_sample.as_raw(),
                first_identity,
                "caller-owned output allocation is reused"
            );
        }
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
}
