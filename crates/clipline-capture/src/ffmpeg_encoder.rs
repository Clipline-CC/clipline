//! `Encoder` backed by an `ffmpeg.exe` child process (ddoc §4).
//!
//! One long-lived child per recording: raw NV12 frames are piped to its
//! stdin, the encoded elementary stream is read from stdout on a reader
//! thread that frames it into access units (H.264/HEVC via Annex B start
//! codes, AV1 via the IVF container) using the neutral `annexb`/`hevc`/
//! `av1` modules. B-frames are disabled, so output order equals input
//! order: per-AU `pts_s` is taken from the matching input frame in FIFO
//! order, and the pipeline re-derives durations from pts deltas at GOP
//! seal. Parameter sets (SPS/PPS, VPS/SPS/PPS, AV1 sequence header) are
//! lifted from the stream for the muxer's codec configuration box.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use clipline_mp4::{VideoCodecParams, VideoTrackConfig};

use crate::ffmpeg::{encoder_name, wait_for_child, ChildWait};
use crate::framing::{AccessUnitFramer, AnnexBFramer, IvfFramer};
use crate::probe::{Codec, EncoderBackend};
use crate::traits::{EncodeError, EncodedPacket, Encoder, Frame, FrameData};

/// B-frames are disabled and FFmpeg normally has at most a small tail to
/// flush. Thirty seconds still accommodates slow software AV1 on loaded
/// machines while placing a finite ceiling on recorder/app shutdown.
const ENCODER_FLUSH_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(windows)]
use crate::cpu_video::{CpuCropRect, CpuVideoConverter};
#[cfg(windows)]
use crate::windows::nv12::{CropRect, VideoConverter};
#[cfg(windows)]
use windows::Win32::Graphics::Direct3D11::ID3D11Device;

/// One framed access unit out of the reader thread, before pts assignment.
struct RawUnit {
    /// Muxer-ready sample bytes (length-prefixed NALs / stripped OBUs).
    data: Vec<u8>,
    is_keyframe: bool,
}

enum ReaderMsg {
    Unit(RawUnit),
    Error(String),
}

/// The process-side machinery, shared by every constructor.
struct Spawned {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<ReaderMsg>,
    reader: JoinHandle<()>,
    codec_params: Arc<Mutex<Option<VideoCodecParams>>>,
}

pub struct FfmpegVideoEncoder {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<ReaderMsg>,
    reader: Option<JoinHandle<()>>,
    codec_params: Arc<Mutex<Option<VideoCodecParams>>>,
    pending_pts: VecDeque<f64>,
    /// The codec this child was configured to produce — used for the
    /// `track_config` fallback before the reader extracts parameter sets.
    codec: Codec,
    width: u16,
    height: u16,
    fps: u32,
    /// BGRA → NV12 conversion for `FrameData::Gpu` (Windows), using either the
    /// video processor or the VM-safe CPU fallback. Pre-NV12 CPU frames leave
    /// this unset and are piped as-is.
    #[cfg(windows)]
    converter: Option<FrameConverter>,
    #[cfg(windows)]
    device: Option<ID3D11Device>,
}

#[cfg(windows)]
enum FrameConverter {
    Gpu(VideoConverter),
    Cpu(CpuFrameConverter),
}

#[cfg(windows)]
struct CpuFrameConverter {
    converter: CpuVideoConverter,
    crop: Option<CpuCropRect>,
    input_width: u32,
    input_height: u32,
    output_width: u32,
    output_height: u32,
}

#[cfg(windows)]
impl CpuFrameConverter {
    fn new(
        input_width: u32,
        input_height: u32,
        crop: Option<CropRect>,
        output_width: u32,
        output_height: u32,
    ) -> Result<Self, EncodeError> {
        let crop = crop.map(|rect| CpuCropRect {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        });
        let converter =
            CpuVideoConverter::new(input_width, input_height, crop, output_width, output_height)
                .map_err(|e| EncodeError::Backend(format!("CPU nv12 converter: {e}")))?;
        Ok(Self {
            converter,
            crop,
            input_width,
            input_height,
            output_width,
            output_height,
        })
    }

    fn convert(
        &mut self,
        device: &ID3D11Device,
        texture: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
    ) -> Result<Vec<u8>, EncodeError> {
        let bgra = crate::windows::nv12::read_bgra(device, texture)
            .map_err(|e| EncodeError::Backend(format!("BGRA readback: {e}")))?;
        if (bgra.width, bgra.height) != (self.input_width, self.input_height) {
            self.converter = CpuVideoConverter::new(
                bgra.width,
                bgra.height,
                self.crop,
                self.output_width,
                self.output_height,
            )
            .map_err(|e| EncodeError::Backend(format!("CPU nv12 converter resize: {e}")))?;
            self.input_width = bgra.width;
            self.input_height = bgra.height;
        }
        self.converter
            .convert(&bgra.bytes, bgra.stride)
            .map_err(|e| EncodeError::Backend(format!("CPU nv12 convert: {e}")))
    }
}

/// Spawn the ffmpeg child and its stdout reader thread.
fn spawn_process(
    ffmpeg: &std::path::Path,
    backend: EncoderBackend,
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u32,
) -> Result<Spawned, EncodeError> {
    let encoder = encoder_name(backend, codec).ok_or_else(|| {
        EncodeError::Backend(format!("no ffmpeg encoder for {backend:?}/{codec:?}"))
    })?;
    let args = build_args(encoder, backend, codec, width, height, fps, bitrate_bps);
    let mut command = Command::new(ffmpeg);
    command
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    crate::ffmpeg::suppress_console(&mut command);
    let mut child = command
        .spawn()
        .map_err(|e| EncodeError::Backend(format!("spawn ffmpeg: {e}")))?;

    let Some(stdin) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(EncodeError::Backend("ffmpeg stdin missing".into()));
    };
    let Some(stdout) = child.stdout.take() else {
        drop(stdin);
        let _ = child.kill();
        let _ = child.wait();
        return Err(EncodeError::Backend("ffmpeg stdout missing".into()));
    };

    let codec_params = Arc::new(Mutex::new(None));
    let (tx, rx) = std::sync::mpsc::channel();
    let reader_params = Arc::clone(&codec_params);
    let reader = match std::thread::Builder::new()
        .name("clipline-ffmpeg-reader".into())
        .spawn(move || run_reader(stdout, codec, reader_params, tx))
    {
        Ok(reader) => reader,
        Err(error) => {
            drop(stdin);
            let _ = child.kill();
            let _ = child.wait();
            return Err(EncodeError::Backend(format!("spawn reader: {error}")));
        }
    };

    Ok(Spawned {
        child,
        stdin,
        rx,
        reader,
        codec_params,
    })
}

impl FfmpegVideoEncoder {
    fn assemble(spawned: Spawned, codec: Codec, width: u32, height: u32, fps: u32) -> Self {
        Self {
            child: spawned.child,
            stdin: Some(spawned.stdin),
            rx: spawned.rx,
            reader: Some(spawned.reader),
            codec_params: spawned.codec_params,
            pending_pts: VecDeque::new(),
            codec,
            width: width as u16,
            height: height as u16,
            fps,
            #[cfg(windows)]
            converter: None,
            #[cfg(windows)]
            device: None,
        }
    }

    /// Spawn an ffmpeg child encoding pre-NV12 CPU frames → `codec` on
    /// `backend`. `width`/`height` are the encode dimensions; CPU frames must
    /// already be NV12 at that size. `ffmpeg` is the located binary.
    pub fn new(
        ffmpeg: &std::path::Path,
        backend: EncoderBackend,
        codec: Codec,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u32,
    ) -> Result<Self, EncodeError> {
        let spawned = spawn_process(ffmpeg, backend, codec, width, height, fps, bitrate_bps)?;
        Ok(Self::assemble(spawned, codec, width, height, fps))
    }

    /// Windows constructor for GPU capture: converts each BGRA `FrameData::Gpu`
    /// to NV12 at the encode size (with optional region crop) on the shared
    /// D3D11 device, reads it back, and pipes it to ffmpeg.
    #[cfg(windows)]
    #[allow(clippy::too_many_arguments)]
    pub fn new_on(
        device: &ID3D11Device,
        ffmpeg: &std::path::Path,
        backend: EncoderBackend,
        codec: Codec,
        in_w: u32,
        in_h: u32,
        crop: Option<CropRect>,
        out_w: u32,
        out_h: u32,
        fps: u32,
        bitrate_bps: u32,
    ) -> Result<Self, EncodeError> {
        crate::windows::d3d11::ensure_multithread_protected(device)
            .map_err(|e| EncodeError::Backend(format!("D3D11 multithread protection: {e}")))?;
        let converter = VideoConverter::new_with_crop(device, in_w, in_h, out_w, out_h, crop)
            .map_err(|e| EncodeError::Backend(format!("nv12 converter: {e}")))?;
        let spawned = spawn_process(ffmpeg, backend, codec, out_w, out_h, fps, bitrate_bps)?;
        let mut enc = Self::assemble(spawned, codec, out_w, out_h, fps);
        enc.converter = Some(FrameConverter::Gpu(converter));
        enc.device = Some(device.clone());
        Ok(enc)
    }

    /// Windows constructor for VMs and software-only adapters. WGC still
    /// supplies BGRA GPU textures, but no D3D11 video processor is required:
    /// frames are read back and converted to NV12 on the CPU before being
    /// piped to FFmpeg's software Media Foundation encoder.
    #[cfg(windows)]
    #[allow(clippy::too_many_arguments)]
    pub fn new_cpu_on(
        device: &ID3D11Device,
        ffmpeg: &std::path::Path,
        backend: EncoderBackend,
        codec: Codec,
        in_w: u32,
        in_h: u32,
        crop: Option<CropRect>,
        out_w: u32,
        out_h: u32,
        fps: u32,
        bitrate_bps: u32,
    ) -> Result<Self, EncodeError> {
        crate::windows::d3d11::ensure_multithread_protected(device)
            .map_err(|e| EncodeError::Backend(format!("D3D11 multithread protection: {e}")))?;
        let converter = CpuFrameConverter::new(in_w, in_h, crop, out_w, out_h)?;
        let spawned = spawn_process(ffmpeg, backend, codec, out_w, out_h, fps, bitrate_bps)?;
        let mut enc = Self::assemble(spawned, codec, out_w, out_h, fps);
        enc.converter = Some(FrameConverter::Cpu(converter));
        enc.device = Some(device.clone());
        Ok(enc)
    }

    /// Extract contiguous NV12 bytes for one frame. CPU frames are already
    /// NV12; GPU frames are converted on the GPU and read back.
    fn frame_nv12(&mut self, frame: &Frame) -> Result<Vec<u8>, EncodeError> {
        match &frame.data {
            FrameData::Cpu(bytes) => Ok(bytes.clone()),
            #[cfg(windows)]
            FrameData::Gpu(texture) => {
                let converter = self.converter.as_mut().ok_or_else(|| {
                    EncodeError::Backend("GPU frame but encoder has no converter".into())
                })?;
                let device = self.device.as_ref().expect("device set with converter");
                match converter {
                    FrameConverter::Gpu(converter) => {
                        let nv12 = converter
                            .convert(texture)
                            .map_err(|e| EncodeError::Backend(format!("nv12 convert: {e}")))?;
                        crate::windows::nv12::read_nv12(device, &nv12)
                            .map_err(|e| EncodeError::Backend(format!("nv12 readback: {e}")))
                    }
                    FrameConverter::Cpu(converter) => converter.convert(device, texture),
                }
            }
        }
    }

    fn drain_ready(&mut self) -> Result<Vec<EncodedPacket>, EncodeError> {
        let mut out = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(ReaderMsg::Unit(unit)) => {
                    let pts_s = pop_output_pts(&mut self.pending_pts)?;
                    out.push(EncodedPacket {
                        data: unit.data,
                        pts_s,
                        duration_s: 1.0 / self.fps as f64,
                        is_keyframe: unit.is_keyframe,
                    });
                }
                Ok(ReaderMsg::Error(error)) => return Err(EncodeError::Backend(error)),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        Ok(out)
    }

    fn finish_with_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Vec<EncodedPacket>, EncodeError> {
        // Closing stdin signals EOF. The existing reader keeps stdout drained
        // while FFmpeg flushes, so neither side can block on a full pipe.
        drop(self.stdin.take());
        let wait = wait_for_child(&mut self.child, timeout);
        let reader = self.reader.take().map(|reader| reader.join());

        let wait = wait
            .map_err(|error| EncodeError::Backend(format!("await ffmpeg during flush: {error}")))?;
        if matches!(wait, ChildWait::TimedOut) {
            return Err(EncodeError::Backend(format!(
                "ffmpeg did not flush within {timeout:?}; the encoded tail was discarded"
            )));
        }
        if reader.is_some_and(|result| result.is_err()) {
            return Err(EncodeError::Backend("ffmpeg reader thread panicked".into()));
        }
        let ChildWait::Exited(status) = wait else {
            unreachable!("timeout handled above")
        };
        if !status.success() {
            return Err(EncodeError::Backend(format!("ffmpeg exited with {status}")));
        }
        let packets = self.drain_ready()?;
        ensure_all_output_pts_consumed(&self.pending_pts)?;
        Ok(packets)
    }
}

impl Encoder for FfmpegVideoEncoder {
    fn encode(&mut self, frame: &Frame) -> Result<Vec<EncodedPacket>, EncodeError> {
        let nv12 = self.frame_nv12(frame)?;
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| EncodeError::Backend("ffmpeg stdin already closed".into()))?;
        stdin
            .write_all(&nv12)
            .map_err(|e| EncodeError::Backend(format!("write frame: {e}")))?;
        self.pending_pts.push_back(frame.pts_s);
        self.drain_ready()
    }

    fn track_config(&self) -> VideoTrackConfig {
        // The reader fills this from the stream's first parameter sets. If
        // it is queried before any keyframe (e.g. an empty recording), fall
        // back to the *configured* codec with empty params — never claim a
        // codec the stream isn't (which would pick the wrong sample entry).
        let codec = self
            .codec_params
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_else(|| empty_params(self.codec));
        VideoTrackConfig {
            width: self.width,
            height: self.height,
            timescale: 90_000,
            codec,
        }
    }

    fn finish(&mut self) -> Result<Vec<EncodedPacket>, EncodeError> {
        self.finish_with_timeout(ENCODER_FLUSH_TIMEOUT)
    }
}

/// Empty-parameter-set config for the configured codec — used only as the
/// pre-keyframe fallback so the muxer at least selects the right sample
/// entry box (avc1/hvc1/av01).
fn empty_params(codec: Codec) -> VideoCodecParams {
    match codec {
        Codec::H264 => VideoCodecParams::H264 {
            sps: Vec::new(),
            pps: Vec::new(),
        },
        Codec::Hevc => VideoCodecParams::Hevc {
            vps: Vec::new(),
            sps: Vec::new(),
            pps: Vec::new(),
        },
        Codec::Av1 => VideoCodecParams::Av1 {
            sequence_header_obu: Vec::new(),
        },
    }
}

impl Drop for FfmpegVideoEncoder {
    fn drop(&mut self) {
        if self.stdin.is_none() && self.reader.is_none() {
            return;
        }
        // If finish() was not called, don't leak the child or wait forever.
        drop(self.stdin.take());
        let _ = wait_for_child(&mut self.child, ENCODER_FLUSH_TIMEOUT);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// Reader thread: frame the elementary stream into access units, convert to
/// muxer-ready samples, classify keyframes, and lift parameter sets.
fn run_reader(
    mut stdout: impl Read,
    codec: Codec,
    params: Arc<Mutex<Option<VideoCodecParams>>>,
    tx: Sender<ReaderMsg>,
) {
    let mut framer: Box<dyn AccessUnitFramer> = match codec {
        Codec::H264 => Box::new(AnnexBFramer::h264()),
        Codec::Hevc => Box::new(AnnexBFramer::hevc()),
        Codec::Av1 => Box::new(IvfFramer::new()),
    };
    let mut buf = [0u8; 65536];
    let emit = |au: Vec<u8>| -> Result<bool, String> {
        let (sample, is_keyframe) = finish_unit(codec, &au)?;
        set_params_if_empty(codec, &au, &params);
        // A dropped receiver (encoder gone) just ends the thread.
        Ok(tx
            .send(ReaderMsg::Unit(RawUnit {
                data: sample,
                is_keyframe,
            }))
            .is_ok())
    };
    let mut failed = false;
    loop {
        match stdout.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                for au in framer.push(&buf[..n]) {
                    if failed {
                        continue;
                    }
                    match emit(au) {
                        Ok(true) => {}
                        Ok(false) => return,
                        Err(error) => {
                            let _ = tx.send(ReaderMsg::Error(error));
                            failed = true;
                        }
                    }
                }
            }
            Err(e) => {
                let _ = tx.send(ReaderMsg::Error(format!(
                    "ffmpeg reader stdout failed: {e}"
                )));
                break;
            }
        }
    }
    if !failed {
        if let Some(au) = framer.flush() {
            if let Err(error) = emit(au) {
                let _ = tx.send(ReaderMsg::Error(error));
            }
        }
    }
}

/// Convert one raw access unit to muxer-ready sample bytes and decide
/// whether it is a keyframe.
fn finish_unit(codec: Codec, au: &[u8]) -> Result<(Vec<u8>, bool), String> {
    match codec {
        Codec::H264 => {
            let is_key = crate::annexb::split_annexb(au)
                .iter()
                .any(|n| crate::annexb::nal_type(n) == 5);
            Ok((crate::annexb::annexb_to_avcc(au), is_key))
        }
        Codec::Hevc => Ok((
            crate::hevc::annexb_to_hvcc_samples(au),
            crate::hevc::is_keyframe(au),
        )),
        Codec::Av1 => {
            let is_keyframe = crate::av1::frame_is_keyframe(au)
                .ok_or_else(|| "AV1 temporal unit has no valid frame-type metadata".to_string())?;
            Ok((crate::av1::obus_to_av01_sample(au), is_keyframe))
        }
    }
}

fn pop_output_pts(pending_pts: &mut VecDeque<f64>) -> Result<f64, EncodeError> {
    pending_pts.pop_front().ok_or_else(|| {
        EncodeError::Backend("ffmpeg emitted more pictures than input frames".into())
    })
}

fn ensure_all_output_pts_consumed(pending_pts: &VecDeque<f64>) -> Result<(), EncodeError> {
    if pending_pts.is_empty() {
        Ok(())
    } else {
        Err(EncodeError::Backend(format!(
            "ffmpeg emitted {} fewer picture(s) than input frames",
            pending_pts.len()
        )))
    }
}

/// Cache the codec parameter sets the first time the stream carries them.
fn set_params_if_empty(codec: Codec, au: &[u8], params: &Arc<Mutex<Option<VideoCodecParams>>>) {
    let Ok(mut guard) = params.lock() else { return };
    if guard.is_some() {
        return;
    }
    *guard = match codec {
        Codec::H264 => {
            crate::annexb::extract_sps_pps(au).map(|(sps, pps)| VideoCodecParams::H264 {
                sps: vec![sps],
                pps: vec![pps],
            })
        }
        Codec::Hevc => {
            crate::hevc::extract_vps_sps_pps(au).map(|(vps, sps, pps)| VideoCodecParams::Hevc {
                vps: vec![vps],
                sps: vec![sps],
                pps: vec![pps],
            })
        }
        Codec::Av1 => crate::av1::extract_sequence_header(au).map(|sequence_header_obu| {
            VideoCodecParams::Av1 {
                sequence_header_obu,
            }
        }),
    };
}

/// Build the ffmpeg argument vector: NV12 rawvideo in, elementary stream out,
/// Short GOP, no B-frames, CBR for replay-buffer size predictability.
fn build_args(
    encoder: &str,
    backend: EncoderBackend,
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u32,
) -> Vec<String> {
    let gop = crate::replay_gop_frames(fps);
    let bufsize = bitrate_bps as u64 * 2;
    let out_format = match codec {
        Codec::H264 => "h264",
        Codec::Hevc => "hevc",
        Codec::Av1 => "ivf",
    };
    let mut a: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-f".into(),
        "rawvideo".into(),
        "-pix_fmt".into(),
        "nv12".into(),
    ];
    a.extend(rec709_limited_flags());
    a.extend([
        "-s".into(),
        format!("{width}x{height}"),
        "-r".into(),
        fps.to_string(),
        "-i".into(),
        "pipe:0".into(),
        "-an".into(),
        "-c:v".into(),
        encoder.into(),
        "-g".into(),
        gop.to_string(),
        "-bf".into(),
        "0".into(),
    ]);
    a.extend(backend_rate_control(backend, bitrate_bps, bufsize));
    a.extend(rec709_limited_flags());
    a.extend(["-f".into(), out_format.into(), "pipe:1".into()]);
    a
}

fn rec709_limited_flags() -> Vec<String> {
    [
        "-color_range",
        "tv",
        "-colorspace",
        "bt709",
        "-color_primaries",
        "bt709",
        "-color_trc",
        "bt709",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

/// Per-backend rate control. Hardware encoders use low-latency CBR (capped
/// rate + bufsize) for replay-buffer size predictability. SVT-AV1 takes only
/// a target bitrate and a realtime preset — it rejects `-maxrate/-bufsize`
/// (verified live: `Init failed`/exit -22), so those stay hardware-only.
/// Unknown flags would make ffmpeg fail to open the encoder, so each family
/// sticks to widely-supported options. Derived-media exports reuse this proven
/// argument set when they need the same compatibility.
pub fn backend_rate_control(
    backend: EncoderBackend,
    bitrate_bps: u32,
    bufsize: u64,
) -> Vec<String> {
    let s = |v: &str| v.to_string();
    let b = bitrate_bps.to_string();
    let cbr_capped = || {
        vec![
            s("-b:v"),
            b.clone(),
            s("-maxrate"),
            b.clone(),
            s("-bufsize"),
            bufsize.to_string(),
        ]
    };
    match backend {
        EncoderBackend::Nvenc => {
            let mut v = vec![s("-rc"), s("cbr")];
            v.extend(cbr_capped());
            v.extend([s("-preset"), s("p4"), s("-tune"), s("ll")]);
            v
        }
        EncoderBackend::Amf => {
            let mut v = vec![s("-rc"), s("cbr")];
            v.extend(cbr_capped());
            v.extend([s("-usage"), s("lowlatency")]);
            v
        }
        EncoderBackend::QuickSync => {
            let mut v = cbr_capped();
            v.extend([s("-low_power"), s("0")]);
            v
        }
        EncoderBackend::SvtAv1 => vec![s("-b:v"), b, s("-preset"), s("8")],
        EncoderBackend::MfSoftware => vec![s("-hw_encoding"), s("0"), s("-b:v"), b],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENCODER_CHILD_MODE: &str = "CLIPLINE_FFMPEG_ENCODER_CHILD_MODE";

    #[test]
    fn encoder_subprocess_helper() {
        match std::env::var(ENCODER_CHILD_MODE).as_deref() {
            Ok("hang") => std::thread::sleep(Duration::from_secs(60)),
            Ok("h264_tail") => {
                let mut input = Vec::new();
                std::io::stdin()
                    .read_to_end(&mut input)
                    .expect("read encoder stdin");
                std::io::stdout()
                    .write_all(&[0, 0, 0, 1, 0x65, 0x80, 1, 0, 0, 0, 1, 0x41, 0x80, 2])
                    .expect("write encoded tail");
                std::io::stdout().flush().expect("flush encoded tail");
                std::process::exit(0);
            }
            _ => {}
        }
    }

    fn helper_encoder_for_test(mode: &str) -> FfmpegVideoEncoder {
        let mut command = Command::new(std::env::current_exe().expect("current test executable"));
        command
            .args([
                "--exact",
                "ffmpeg_encoder::tests::encoder_subprocess_helper",
                "--nocapture",
            ])
            .env(ENCODER_CHILD_MODE, mode)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        crate::ffmpeg::suppress_console(&mut command);
        let mut child = command.spawn().expect("spawn stalled helper");
        let stdin = child.stdin.take().expect("helper stdin");
        let stdout = child.stdout.take().expect("helper stdout");
        let codec_params = Arc::new(Mutex::new(None));
        let reader_params = Arc::clone(&codec_params);
        let (tx, rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            run_reader(stdout, Codec::H264, reader_params, tx);
        });

        FfmpegVideoEncoder::assemble(
            Spawned {
                child,
                stdin,
                rx,
                reader,
                codec_params,
            },
            Codec::H264,
            16,
            16,
            30,
        )
    }

    #[test]
    fn encoder_flush_timeout_kills_before_joining_stdout_reader() {
        let mut encoder = helper_encoder_for_test("hang");
        let started = std::time::Instant::now();

        let error = encoder
            .finish_with_timeout(Duration::from_millis(100))
            .expect_err("stalled encoder must time out");

        assert!(error.to_string().contains("encoded tail was discarded"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn normal_flush_preserves_tail_packets_before_joining_reader() {
        let mut encoder = helper_encoder_for_test("h264_tail");
        encoder.pending_pts.extend([0.0, 1.0 / 30.0]);

        let packets = encoder
            .finish_with_timeout(Duration::from_secs(2))
            .expect("normal helper flush");

        assert_eq!(packets.len(), 2);
        assert!(packets[0].is_keyframe);
        assert!(!packets[1].is_keyframe);
    }

    #[test]
    fn args_set_nv12_input_gop_and_output_format() {
        let args = build_args(
            "libsvtav1",
            EncoderBackend::SvtAv1,
            Codec::Av1,
            1920,
            1080,
            60,
            8_000_000,
        );
        let joined = args.join(" ");
        assert!(joined.contains("rawvideo"));
        assert!(joined.contains("nv12"));
        assert!(joined.contains("-color_range tv"));
        assert!(joined.contains("-colorspace bt709"));
        assert!(joined.contains("-color_primaries bt709"));
        assert!(joined.contains("-color_trc bt709"));
        assert!(joined.contains("-s 1920x1080"));
        assert!(joined.contains("-r 60"));
        assert!(joined.contains("-c:v libsvtav1"));
        assert!(joined.contains("-g 30"), "half-second GOP at 60 fps");
        assert!(joined.contains("-bf 0"), "no B-frames");
        assert!(joined.ends_with("-f ivf pipe:1"), "AV1 → IVF: {joined}");
    }

    #[test]
    fn h264_and_hevc_select_their_elementary_stream_muxers() {
        let h264 = build_args(
            "h264_amf",
            EncoderBackend::Amf,
            Codec::H264,
            640,
            360,
            30,
            4_000_000,
        );
        assert!(h264.join(" ").ends_with("-f h264 pipe:1"));
        let hevc = build_args(
            "hevc_amf",
            EncoderBackend::Amf,
            Codec::Hevc,
            640,
            360,
            30,
            4_000_000,
        );
        assert!(hevc.join(" ").ends_with("-f hevc pipe:1"));
    }

    #[test]
    fn finish_unit_classifies_h264_idr_as_keyframe() {
        // Annex B: SPS, PPS, IDR → keyframe; a lone non-IDR slice → not.
        let key = [
            &[0, 0, 0, 1, 0x67, 0x42][..],
            &[0, 0, 1, 0x68, 0xEE][..],
            &[0, 0, 1, 0x65, 0x88][..],
        ]
        .concat();
        let (_sample, is_key) = finish_unit(Codec::H264, &key).unwrap();
        assert!(is_key);
        let inter = [0, 0, 0, 1, 0x41, 0x9A];
        let (_s, is_key) = finish_unit(Codec::H264, &inter).unwrap();
        assert!(!is_key);
    }

    #[test]
    fn finish_unit_uses_av1_frame_header_not_position() {
        let key = [0x32, 0x01, 0x00];
        let inter = [0x32, 0x01, 0x20];
        assert!(finish_unit(Codec::Av1, &key).unwrap().1);
        assert!(!finish_unit(Codec::Av1, &inter).unwrap().1);
        assert!(finish_unit(Codec::Av1, &[0x80]).is_err());
    }

    #[test]
    fn output_pts_requires_one_queued_input_timestamp() {
        let mut pending = VecDeque::from([1.25]);
        assert_eq!(pop_output_pts(&mut pending).unwrap(), 1.25);
        assert!(pop_output_pts(&mut pending).is_err());
    }

    #[test]
    fn finish_rejects_unmatched_input_timestamps() {
        assert!(ensure_all_output_pts_consumed(&VecDeque::new()).is_ok());
        let error = ensure_all_output_pts_consumed(&VecDeque::from([1.0, 2.0])).unwrap_err();
        assert!(error.to_string().contains("2 fewer picture"));
    }

    #[test]
    fn finish_unit_classifies_hevc_irap_as_keyframe() {
        // Annex B HEVC: BLA_W_LP (NAL type 16) → keyframe
        let irap = [0x00, 0x00, 0x00, 0x01, 0x20, 0x01]; // NAL type = (0x20 >> 1) & 0x3F = 16
        let (_sample, is_key) = finish_unit(Codec::Hevc, &irap).unwrap();
        assert!(is_key, "HEVC IRAP should be keyframe");
        // Non-IRAP: TRAIL_R (NAL type 1)
        let inter = [0x00, 0x00, 0x00, 0x01, 0x02, 0x01]; // NAL type = (0x02 >> 1) & 0x3F = 1
        let (_s, is_key) = finish_unit(Codec::Hevc, &inter).unwrap();
        assert!(!is_key, "HEVC TRAIL_R should not be keyframe");
    }

    #[test]
    fn empty_params_produces_correct_codec_variant() {
        match empty_params(Codec::H264) {
            VideoCodecParams::H264 { sps, pps } => {
                assert!(sps.is_empty());
                assert!(pps.is_empty());
            }
            _ => panic!("expected H264"),
        }
        match empty_params(Codec::Hevc) {
            VideoCodecParams::Hevc { vps, sps, pps } => {
                assert!(vps.is_empty());
                assert!(sps.is_empty());
                assert!(pps.is_empty());
            }
            _ => panic!("expected Hevc"),
        }
        match empty_params(Codec::Av1) {
            VideoCodecParams::Av1 {
                sequence_header_obu,
            } => {
                assert!(sequence_header_obu.is_empty());
            }
            _ => panic!("expected Av1"),
        }
    }

    #[test]
    fn rec709_limited_flags_include_all_four_bt709_params() {
        let flags = rec709_limited_flags();
        let joined = flags.join(" ");
        assert!(joined.contains("-color_range tv"));
        assert!(joined.contains("-colorspace bt709"));
        assert!(joined.contains("-color_primaries bt709"));
        assert!(joined.contains("-color_trc bt709"));
    }

    #[test]
    fn backend_rate_control_nvenc_uses_cbr_with_preset() {
        let rc = backend_rate_control(EncoderBackend::Nvenc, 8_000_000, 16_000_000);
        let joined = rc.join(" ");
        assert!(joined.contains("-rc cbr"));
        assert!(joined.contains("-b:v 8000000"));
        assert!(joined.contains("-maxrate 8000000"));
        assert!(joined.contains("-bufsize 16000000"));
        assert!(joined.contains("-preset p4"));
        assert!(joined.contains("-tune ll"));
    }

    #[test]
    fn backend_rate_control_amf_uses_cbr_with_lowlatency() {
        let rc = backend_rate_control(EncoderBackend::Amf, 4_000_000, 8_000_000);
        let joined = rc.join(" ");
        assert!(joined.contains("-rc cbr"));
        assert!(joined.contains("-usage lowlatency"));
    }

    #[test]
    fn backend_rate_control_quicksync_has_cbr_and_low_power() {
        let rc = backend_rate_control(EncoderBackend::QuickSync, 4_000_000, 8_000_000);
        let joined = rc.join(" ");
        assert!(joined.contains("-b:v 4000000"));
        assert!(joined.contains("-low_power 0"));
    }

    #[test]
    fn backend_rate_control_svtav1_has_no_maxrate() {
        let rc = backend_rate_control(EncoderBackend::SvtAv1, 6_000_000, 12_000_000);
        let joined = rc.join(" ");
        assert!(joined.contains("-b:v 6000000"));
        assert!(joined.contains("-preset 8"));
        assert!(!joined.contains("-maxrate"), "SVT-AV1 rejects -maxrate");
        assert!(!joined.contains("-bufsize"), "SVT-AV1 rejects -bufsize");
    }

    #[test]
    fn backend_rate_control_mf_software_forces_cpu_encoding() {
        let rc = backend_rate_control(EncoderBackend::MfSoftware, 4_000_000, 8_000_000);
        let joined = rc.join(" ");
        assert!(joined.contains("-hw_encoding 0"));
        assert!(joined.contains("-b:v 4000000"));
    }

    #[test]
    fn media_foundation_software_args_emit_h264_elementary_stream() {
        let args = build_args(
            "h264_mf",
            EncoderBackend::MfSoftware,
            Codec::H264,
            1280,
            720,
            30,
            6_000_000,
        );
        let joined = args.join(" ");
        assert!(joined.contains("-c:v h264_mf"));
        assert!(joined.contains("-hw_encoding 0"));
        assert!(joined.ends_with("-f h264 pipe:1"));
    }

    #[test]
    fn set_params_if_empty_caches_on_first_call_only() {
        use std::sync::{Arc, Mutex};
        let params = Arc::new(Mutex::new(None));
        // H.264 Annex B with SPS + PPS
        let au = [
            0x00, 0x00, 0x00, 0x01, 0x67, 0x64, 0x00, 0x0A, 0xAC, // SPS (nal_type 7)
            0x00, 0x00, 0x00, 0x01, 0x68, 0xEE, 0x38, 0x80, // PPS (nal_type 8)
        ];
        set_params_if_empty(Codec::H264, &au, &params);
        assert!(params.lock().unwrap().is_some());
        // A second call with different data should not overwrite
        let au2 = [
            0x00, 0x00, 0x00, 0x01, 0x67, 0xFF, 0xFF, // different SPS
            0x00, 0x00, 0x00, 0x01, 0x68, 0xFF, 0xFF, // different PPS
        ];
        set_params_if_empty(Codec::H264, &au2, &params);
        {
            let guard = params.lock().unwrap();
            match guard.as_ref().unwrap() {
                VideoCodecParams::H264 { sps, .. } => {
                    assert_eq!(
                        sps,
                        &[vec![0x67, 0x64, 0x00, 0x0A, 0xAC]],
                        "first params cached"
                    );
                }
                _ => panic!("expected H264"),
            }
        }
    }
}
