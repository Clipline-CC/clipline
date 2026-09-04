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
use std::io::Write;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use clipline_mp4::{VideoCodecParams, VideoTrackConfig};

use crate::ffmpeg::{ChildWait, encoder_name, wait_for_child};
use crate::probe::{Codec, EncoderBackend};
use crate::traits::{EncodeError, EncodedPacket, Encoder, Frame, FrameData};

pub mod args;
pub use args::backend_rate_control;
mod reader;

#[cfg(test)]
mod tests;

use args::build_args;
use reader::{ReaderMsg, ensure_all_output_pts_consumed, pop_output_pts, run_reader};

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
/// The process-side machinery, shared by every constructor.
pub(crate) struct Spawned {
    pub(crate) child: Child,
    pub(crate) stdin: ChildStdin,
    pub(crate) rx: Receiver<ReaderMsg>,
    pub(crate) reader: JoinHandle<()>,
    pub(crate) codec_params: Arc<Mutex<Option<VideoCodecParams>>>,
}
pub struct FfmpegVideoEncoder {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<ReaderMsg>,
    reader: Option<JoinHandle<()>>,
    codec_params: Arc<Mutex<Option<VideoCodecParams>>>,
    pub(crate) pending_pts: VecDeque<f64>,
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
    pub(crate) fn assemble(spawned: Spawned, codec: Codec, width: u32, height: u32, fps: u32) -> Self {
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
    pub(crate) fn finish_with_timeout(
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
pub(crate) fn empty_params(codec: Codec) -> VideoCodecParams {
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
