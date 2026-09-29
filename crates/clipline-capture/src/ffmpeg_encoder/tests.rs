use std::collections::VecDeque;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::io::{Read, Write};

use clipline_mp4::VideoCodecParams;

use super::args::{backend_rate_control, build_args, rec709_limited_flags};
use super::reader::{
    ensure_all_output_pts_consumed, finish_unit, pop_output_pts, run_reader, set_params_if_empty,
};
use super::{FfmpegVideoEncoder, Spawned, empty_params};
use crate::probe::{Codec, EncoderBackend};

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
