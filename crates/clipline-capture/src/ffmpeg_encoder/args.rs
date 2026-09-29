use crate::probe::{Codec, EncoderBackend};

/// Build the ffmpeg argument vector: NV12 rawvideo in, elementary stream out,
/// Short GOP, no B-frames, CBR for replay-buffer size predictability.
pub(crate) fn build_args(
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
pub(crate) fn rec709_limited_flags() -> Vec<String> {
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
