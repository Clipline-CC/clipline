use std::collections::VecDeque;
use std::io::Read;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use clipline_mp4::VideoCodecParams;

use crate::framing::{AccessUnitFramer, AnnexBFramer, IvfFramer};
use crate::probe::Codec;
use crate::traits::EncodeError;

/// One framed access unit out of the reader thread, before pts assignment.
pub(crate) struct RawUnit {
    /// Muxer-ready sample bytes (length-prefixed NALs / stripped OBUs).
    pub(crate) data: Vec<u8>,
    pub(crate) is_keyframe: bool,
}
pub(crate) enum ReaderMsg {
    Unit(RawUnit),
    Error(String),
}
/// Reader thread: frame the elementary stream into access units, convert to
/// muxer-ready samples, classify keyframes, and lift parameter sets.
pub(crate) fn run_reader(
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
pub(crate) fn finish_unit(codec: Codec, au: &[u8]) -> Result<(Vec<u8>, bool), String> {
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
pub(crate) fn pop_output_pts(pending_pts: &mut VecDeque<f64>) -> Result<f64, EncodeError> {
    pending_pts.pop_front().ok_or_else(|| {
        EncodeError::Backend("ffmpeg emitted more pictures than input frames".into())
    })
}
pub(crate) fn ensure_all_output_pts_consumed(pending_pts: &VecDeque<f64>) -> Result<(), EncodeError> {
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
pub(crate) fn set_params_if_empty(codec: Codec, au: &[u8], params: &Arc<Mutex<Option<VideoCodecParams>>>) {
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
