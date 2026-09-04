use super::test_support::*;
use crate::mock::{MockAudioSource, MockCapture, MockEncoder};
use crate::traits::{AudioPacket, EncodedPacket};
use super::recorder::Recorder;

    #[test]
    fn groups_packets_into_gop_aligned_segments() {
        // 90 frames at 30 fps, GOP 30 → exactly 3 keyframe-led segments.
        let mut rec = Recorder::new(
            MockCapture::new(90, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        );
        rec.run_to_end().unwrap();
        let ring = rec.ring().unwrap();
        assert_eq!(ring.len(), 3);
        for seg in ring.segments() {
            assert!(seg.starts_with_keyframe);
            assert_eq!(seg.samples.len(), 30);
            assert!((seg.duration_s - 1.0).abs() < 1e-6);
        }
    }


    #[test]
    fn byte_budget_evicts_oldest_gop() {
        // Each MockEncoder sample is 64–70 bytes → a GOP of 30 ≈ ~2 KB.
        // Budget for ~2 GOPs: the first of three must be evicted.
        let mut rec = Recorder::new(MockCapture::new(90, 30), MockEncoder::new(30, 30), 4 * 1024);
        rec.run_to_end().unwrap();
        let ring = rec.ring().unwrap();
        assert_eq!(ring.len(), 2);
        let first = ring.segments().next().unwrap();
        assert!((first.pts_start_s - 1.0).abs() < 1e-6, "GOP at t=0 evicted");
    }


    #[test]
    fn pending_packets_are_byte_budgeted_when_keyframes_never_arrive() {
        let mut rec = Recorder::new(MockCapture::new(20, 30), NeverKeyframeEncoder::new(30), 512);

        let err = rec
            .run_to_end()
            .expect_err("unkeyframed stream should fail");

        assert!(
            err.to_string().contains("keyframe") && err.to_string().contains("budget"),
            "error should explain the keyframe/budget guard, got {err}"
        );
    }


    #[test]
    fn pending_gop_budget_counts_audio_payloads() {

        let mut video_only =
            Recorder::new(MockCapture::new(10, 30), MockEncoder::new(30, 30), 1024);
        video_only
            .run_to_end()
            .expect("video payload alone fits the pending budget");

        let mut with_audio =
            Recorder::new(MockCapture::new(10, 30), MockEncoder::new(30, 30), 1024)
                .with_audio(Box::new(MockAudioSource::new(48_000, 20)));
        let error = with_audio
            .run_to_end()
            .expect_err("audio must consume the same pending GOP budget");

        assert!(
            error.to_string().contains("video/audio GOP budget"),
            "unexpected error: {error}"
        );
    }


    #[test]
    fn pending_gop_duration_is_bounded_when_keyframes_stop() {
        let mut recorder = Recorder::new(
            MockCapture::new(360, 30),
            MockEncoder::new(1000, 30),
            usize::MAX,
        );

        let error = recorder
            .run_to_end()
            .expect_err("an encoder must not retain an arbitrarily long GOP");

        assert!(
            error.to_string().contains("GOP duration") && error.to_string().contains("keyframe"),
            "unexpected error: {error}"
        );
    }


    #[test]
    fn sparse_capture_timestamps_do_not_look_like_missing_keyframes() {
        let capture = TimestampCapture::new([0.0, 10.5, 10.5 + 1.0 / 60.0]);
        let encoder = VariableDurationEncoder::new(30, 60);
        let mut recorder = Recorder::new(capture, encoder, usize::MAX);

        recorder
            .run_to_end()
            .expect("a static-screen gap is not encoder keyframe failure");

        assert_eq!(recorder.ring_len(), 1);
    }


    #[test]
    fn short_stream_without_initial_keyframe_is_reported() {
        let mut rec = Recorder::new(
            MockCapture::new(1, 30),
            NeverKeyframeEncoder::new(30),
            usize::MAX,
        );

        let err = rec
            .run_to_end()
            .expect_err("short unkeyframed stream should fail");

        assert!(
            err.to_string().contains("keyframe") && err.to_string().contains("ended"),
            "error should explain that the stream ended before an initial keyframe, got {err}"
        );
        assert_eq!(rec.ring().unwrap().len(), 0);
    }


    #[test]
    fn audio_packets_land_in_their_gop_segments() {
        let mut rec = Recorder::new(
            MockCapture::new(90, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        )
        .with_audio(Box::new(MockAudioSource::new(48_000, 20)));
        rec.run_to_end().unwrap();
        let ring = rec.ring().unwrap();
        assert_eq!(ring.len(), 3);
        for (i, seg) in ring.segments().enumerate() {
            assert_eq!(seg.audio.len(), 1, "one audio track");
            // 1 s GOP at 20 ms packets = 50 packets per segment.
            assert_eq!(seg.audio[0].samples.len(), 50, "segment {i}");
        }
        // First packet of the second segment starts at its GOP boundary.
        let seg2 = ring.segments().nth(1).unwrap();
        assert_eq!(&seg2.audio[0].data[..6], b"P00050");
        assert!((seg2.audio[0].pts_start_s.unwrap() - 1.0).abs() < 1e-9);
    }


    #[test]
    fn pending_audio_reservation_is_released_when_each_gop_seals() {

        let mut recorder =
            Recorder::new(MockCapture::new(90, 30), MockEncoder::new(30, 30), 8 * 1024)
                .with_audio(Box::new(MockAudioSource::new(48_000, 20)));

        recorder
            .run_to_end()
            .expect("each individual GOP fits even though all three do not");
        assert_eq!(
            recorder.ring().unwrap().len(),
            2,
            "ring still enforces its budget"
        );
    }


    #[test]
    fn sealed_durations_come_from_pts_deltas_not_encoder_claims() {
        // GOP of 4 over 8 frames → two segments, boundary at frame 4.
        let enc = JitteryEncoder {
            inner: MockEncoder::new(4, 30),
        };
        let mut rec = Recorder::new(MockCapture::new(8, 30), enc, usize::MAX);
        rec.run_to_end().unwrap();
        let segs: Vec<_> = rec.ring().unwrap().segments().collect();
        assert_eq!(segs.len(), 2);
        // Within a GOP: 10/30/10 ms gaps, NOT the encoder's flat 33.3 ms.
        let d: Vec<f64> = segs[0].samples.iter().map(|s| s.duration_s).collect();
        assert!((d[0] - 0.01).abs() < 1e-9, "got {d:?}");
        assert!((d[1] - 0.03).abs() < 1e-9, "got {d:?}");
        assert!((d[2] - 0.01).abs() < 1e-9, "got {d:?}");
        // Boundary: last sample of GOP 1 closes exactly at GOP 2's keyframe.
        let gop2_start = segs[1].pts_start_s;
        assert!(
            (segs[0].pts_end_s() - gop2_start).abs() < 1e-9,
            "no gap, no overlap"
        );
        // Final seal falls back to the encoder duration for the last sample.
        let last = segs[1].samples.last().unwrap();
        assert!((last.duration_s - 1.0 / 30.0).abs() < 1e-9);
    }


    #[test]
    fn sealed_gop_vectors_have_no_geometric_growth_slack() {
        let mut recorder = Recorder::new(
            MockCapture::new(0, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        );
        recorder.pending = vec![
            EncodedPacket {
                data: vec![1; 3],
                pts_s: 0.0,
                duration_s: 0.1,
                is_keyframe: true,
            },
            EncodedPacket {
                data: vec![2; 5],
                pts_s: 0.1,
                duration_s: 0.1,
                is_keyframe: false,
            },
            EncodedPacket {
                data: vec![3; 7],
                pts_s: 0.2,
                duration_s: 0.1,
                is_keyframe: false,
            },
        ];
        recorder.pending_bytes = 15;
        recorder.video_start_pts_s = Some(0.0);
        recorder.pending_audio = vec![vec![
            AudioPacket {
                data: vec![4; 3],
                pts_s: 0.0,
                duration_s: 0.1,
            },
            AudioPacket {
                data: vec![5; 5],
                pts_s: 0.1,
                duration_s: 0.1,
            },
            AudioPacket {
                data: vec![6; 7],
                pts_s: 0.2,
                duration_s: 0.1,
            },
        ]];
        recorder.pending_audio_bytes = 15;

        recorder.seal_pending(0.3).unwrap();

        let segment = recorder
            .ring()
            .unwrap()
            .segments()
            .next()
            .expect("sealed segment");
        assert_eq!(segment.data.capacity(), segment.data.len());
        assert_eq!(segment.samples.capacity(), segment.samples.len());
        assert_eq!(
            segment.audio[0].data.capacity(),
            segment.audio[0].data.len()
        );
        assert_eq!(
            segment.audio[0].samples.capacity(),
            segment.audio[0].samples.len()
        );
    }


    #[test]
    fn run_to_end_drains_encoder_via_finish() {
        let enc = OneFrameLatency {
            inner: MockEncoder::new(30, 30),
            held: None,
        };
        let mut rec = Recorder::new(MockCapture::new(30, 30), enc, usize::MAX);
        rec.run_to_end().unwrap();
        // All 30 frames present despite the encoder's one-frame latency.
        let total: usize = rec
            .ring()
            .unwrap()
            .segments()
            .map(|s| s.samples.len())
            .sum();
        assert_eq!(total, 30);
    }


    #[test]
    fn trailing_partial_gop_is_sealed_at_end() {
        // 45 frames, GOP 30 → one full GOP + one 15-frame partial.
        let mut rec = Recorder::new(
            MockCapture::new(45, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        );
        rec.run_to_end().unwrap();
        let counts: Vec<usize> = rec
            .ring()
            .unwrap()
            .segments()
            .map(|s| s.samples.len())
            .collect();
        assert_eq!(counts, vec![30, 15]);
    }
