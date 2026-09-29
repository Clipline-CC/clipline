use clipline_mp4::HybridMp4Writer;
use crate::mock::{MockCapture, MockEncoder};
use crate::traits::{AudioPacket, EncodedPacket, Encoder};
use std::io;
use super::mux::advance_track_decode_time;
use super::recorder::Recorder;
use super::seal::sealed_video_durations;

    #[test]
    fn bounded_gop_absorbs_independent_sub_tick_timestamp_jitter() {
        let packets: Vec<_> = [0.0, 100.0, 99.4, 200.0, 199.4]
            .into_iter()
            .enumerate()
            .map(|(index, ticks)| EncodedPacket {
                data: vec![index as u8],
                pts_s: ticks / 90_000.0,
                duration_s: 1.0 / 90_000.0,
                is_keyframe: index == 0,
            })
            .collect();

        let durations = sealed_video_durations(&packets, 300.0 / 90_000.0, 90_000)
            .expect("local timestamp jitter must not terminate capture");
        assert_eq!(durations.len(), packets.len());
        assert!(durations.iter().all(|duration| duration * 90_000.0 >= 1.0));
        assert_eq!((durations.iter().sum::<f64>() * 90_000.0).round(), 300.0);
    }


    #[test]
    fn crowded_bounded_gop_extends_only_enough_for_positive_durations() {
        let packets: Vec<_> = [0.0, 0.2, 0.4]
            .into_iter()
            .enumerate()
            .map(|(index, ticks)| EncodedPacket {
                data: vec![index as u8],
                pts_s: ticks / 90_000.0,
                duration_s: 1.0 / 90_000.0,
                is_keyframe: index == 0,
            })
            .collect();

        let durations = sealed_video_durations(&packets, 2.0 / 90_000.0, 90_000)
            .expect("crowded finite timestamps must degrade without ending the session");
        assert_eq!(durations.len(), 3);
        assert!(durations
            .iter()
            .all(|duration| (duration * 90_000.0 - 1.0).abs() < 1e-9));
    }


    #[test]
    fn slightly_backward_single_packet_boundary_gets_one_tick() {
        let packets = vec![EncodedPacket {
            data: vec![0],
            pts_s: 0.0,
            duration_s: 1.0 / 90_000.0,
            is_keyframe: true,
        }];

        let durations = sealed_video_durations(&packets, -0.4 / 90_000.0, 90_000)
            .expect("a sub-tick boundary regression must not terminate capture");
        assert_eq!(durations, vec![1.0 / 90_000.0]);
    }


    #[test]
    fn failed_seal_preserves_pending_video_and_audio() {
        let mut recorder = Recorder::new(
            MockCapture::new(1, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        );
        recorder.pending = vec![EncodedPacket {
            data: vec![1, 2, 3],
            pts_s: f64::NAN,
            duration_s: 1.0 / 30.0,
            is_keyframe: true,
        }];
        recorder.pending_bytes = 3;
        recorder.pending_audio = vec![vec![AudioPacket {
            data: vec![4, 5],
            pts_s: 0.0,
            duration_s: 0.02,
        }]];
        recorder.pending_audio_bytes = 2;

        recorder
            .seal_pending(1.0)
            .expect_err("non-finite pending timestamps must still be rejected");

        assert_eq!(recorder.pending.len(), 1);
        assert_eq!(recorder.pending[0].data, vec![1, 2, 3]);
        assert_eq!(recorder.pending_bytes, 3);
        assert_eq!(recorder.pending_audio.len(), 1);
        assert_eq!(recorder.pending_audio[0][0].data, vec![4, 5]);
        assert_eq!(recorder.pending_audio_bytes, 2);
    }


    #[test]
    fn unbounded_gop_keeps_encoder_duration_for_non_finite_next_timestamp() {
        let packets = vec![
            EncodedPacket {
                data: vec![0],
                pts_s: 0.0,
                duration_s: 0.25,
                is_keyframe: true,
            },
            EncodedPacket {
                data: vec![1],
                pts_s: f64::INFINITY,
                duration_s: 0.5,
                is_keyframe: false,
            },
        ];

        assert_eq!(
            sealed_video_durations(&packets, f64::INFINITY, 90_000).unwrap(),
            vec![0.25, 0.5]
        );
    }


    #[test]
    fn zero_video_timescale_is_rejected_for_unbounded_gop() {
        let packets = vec![EncodedPacket {
            data: vec![0],
            pts_s: 0.0,
            duration_s: 0.25,
            is_keyframe: true,
        }];

        let error = sealed_video_durations(&packets, f64::INFINITY, 0)
            .expect_err("all seals require a valid MP4 video timescale");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }


    #[test]
    fn capture_timeline_never_moves_strict_writer_backward() {
        let cfg = MockEncoder::new(30, 30).track_config();
        let mut writer = HybridMp4Writer::new(std::io::Cursor::new(Vec::new()), cfg).unwrap();
        writer.set_track_decode_time(0, 100).unwrap();

        let one_tick = advance_track_decode_time(&mut writer, 0, 99).unwrap();
        assert_eq!(writer.track_decode_time(0).unwrap(), 100);
        assert_eq!(one_tick.requested_start, 99);
        assert_eq!(one_tick.write_start, 100);

        let larger_regression = advance_track_decode_time(&mut writer, 0, 98).unwrap();
        assert_eq!(writer.track_decode_time(0).unwrap(), 100);
        assert_eq!(larger_regression.requested_start, 98);
        assert_eq!(larger_regression.write_start, 100);
    }
