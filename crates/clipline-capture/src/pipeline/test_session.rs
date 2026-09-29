use super::test_support::*;
use clipline_buffer::{SampleInfo, Segment};
use clipline_test_utils::TestDir;
use crate::mock::{MockAudioSource, MockCapture, MockEncoder};
use crate::traits::Encoder;
use std::io;
use std::sync::{Arc, mpsc};
use std::sync::atomic::{AtomicUsize, Ordering};
use super::recorder::Recorder;
use super::session::{WriteSeek, try_reserve_queue_bytes, write_full_session_segment};

    #[test]
    fn full_session_sink_keeps_segments_evicted_from_replay_ring() {
        let dir = TestDir::new("clipline-pipeline", "full-session");
        let path = dir.path().join("session.mp4");
        let file = std::fs::File::create(&path).unwrap();
        let mut rec = Recorder::new(MockCapture::new(90, 30), MockEncoder::new(30, 30), 4 * 1024);

        rec.start_full_session(file).unwrap();
        rec.run_to_end().unwrap();
        let summary = rec.finish_full_session().unwrap().expect("session summary");

        assert_eq!(
            rec.ring().unwrap().len(),
            2,
            "oldest GOP evicted from replay ring"
        );
        assert!((summary.start_s - 0.0).abs() < 1e-6);
        assert!((summary.duration_s - 3.0).abs() < 1e-6);
        let data = std::fs::read(&path).unwrap();
        let duration = clipline_mp4::walker::movie_duration_s(&data).unwrap();
        assert!(
            (duration - 3.0).abs() < 1e-3,
            "full-session file keeps all GOPs, got {duration}"
        );
    }


    #[test]
    fn a_live_full_session_reports_the_start_its_summary_will_report() {
        let dir = TestDir::new("clipline-pipeline", "full-session-start");
        let path = dir.path().join("session.mp4");
        let file = std::fs::File::create(&path).unwrap();
        let mut rec = Recorder::new(MockCapture::new(90, 30), MockEncoder::new(30, 30), 4 * 1024);

        assert_eq!(rec.full_session_start_s(), None, "nothing recording yet");
        rec.start_full_session(file).unwrap();
        assert_eq!(rec.full_session_start_s(), None, "no segment has landed");

        rec.run_to_end().unwrap();
        let live_start = rec.full_session_start_s().expect("a segment has landed");
        let summary = rec.finish_full_session().unwrap().expect("session summary");

        // Callers re-base live markers on this, so it has to be the origin the
        // finished clip's sidecar re-bases on, not merely close to it.
        assert!((live_start - summary.start_s).abs() < 1e-9);
        assert_eq!(rec.full_session_start_s(), None, "session is over");
    }


    #[test]
    fn one_tick_segment_boundary_overlap_does_not_break_full_session_muxing() {
        let video_cfg = MockEncoder::new(30, 30).track_config();
        let timescale = f64::from(video_cfg.timescale);
        let segment = |start_ticks: f64, duration_ticks: f64| {
            Arc::new(Segment {
                starts_with_keyframe: true,
                pts_start_s: start_ticks / timescale,
                duration_s: duration_ticks / timescale,
                data: vec![0, 0, 0, 1],
                samples: vec![SampleInfo {
                    size: 4,
                    duration_s: duration_ticks / timescale,
                    is_sync: true,
                }],
                audio: Vec::new(),
            })
        };
        let mut target: Option<Box<dyn WriteSeek>> =
            Some(Box::new(std::io::Cursor::new(Vec::new())));
        let mut writer = None;
        let mut origin = None;

        write_full_session_segment(
            &mut target,
            &mut writer,
            &mut origin,
            video_cfg.clone(),
            Vec::new(),
            segment(0.0, 101.0),
        )
        .unwrap();
        write_full_session_segment(
            &mut target,
            &mut writer,
            &mut origin,
            video_cfg,
            Vec::new(),
            // Independent absolute rounding selects tick 100 even though the
            // preceding segment's locally quantized duration ended at 101.
            segment(100.0, 100.0),
        )
        .expect("a one-tick quantization overlap must clamp to the written frontier");
        writer.unwrap().finalize().unwrap();
    }


    #[test]
    fn repeated_segment_rounding_ties_do_not_accumulate_writer_drift() {
        let video_cfg = MockEncoder::new(30, 30).track_config();
        let timescale = f64::from(video_cfg.timescale);
        let segment = |index: u64| {
            let duration_ticks = if index == 4 { 100.0 } else { 1_000.6 };
            Arc::new(Segment {
                starts_with_keyframe: true,
                pts_start_s: index as f64 * 1_000.6 / timescale,
                duration_s: duration_ticks / timescale,
                data: vec![0, 0, 0, index as u8],
                samples: vec![SampleInfo {
                    size: 4,
                    duration_s: duration_ticks / timescale,
                    is_sync: true,
                }],
                audio: Vec::new(),
            })
        };
        let mut target: Option<Box<dyn WriteSeek>> =
            Some(Box::new(std::io::Cursor::new(Vec::new())));
        let mut writer = None;
        let mut origin = None;

        let expected_frontiers = [1_001, 2_002, 3_002, 4_003, 4_102];
        for (index, expected_frontier) in expected_frontiers.into_iter().enumerate() {
            write_full_session_segment(
                &mut target,
                &mut writer,
                &mut origin,
                video_cfg.clone(),
                Vec::new(),
                segment(index as u64),
            )
            .unwrap_or_else(|error| {
                panic!("segment {index} must absorb prior rounding drift: {error}")
            });
            assert_eq!(
                writer.as_ref().unwrap().track_decode_time(0).unwrap(),
                expected_frontier,
                "segment {index} must land on its global endpoint"
            );
        }
        writer.unwrap().finalize().unwrap();
    }


    #[test]
    fn sub_hundred_microsecond_frame_gap_does_not_break_full_session_finalization() {
        let dir = TestDir::new("clipline-pipeline", "sub-millisecond-gop-boundary");
        let path = dir.path().join("session.mp4");
        let mut recorder = Recorder::new(
            MockCapture::new(8, 30),
            PtsRemapEncoder {
                inner: MockEncoder::new(4, 30),
                // Seven ticks after the preceding frame: a valid positive
                // interval that the old 100 us floor inflated to 9 ticks.
                ticks: CLOSELY_SPACED_TICKS,
            },
            usize::MAX,
        );
        recorder
            .start_full_session(std::fs::File::create(&path).unwrap())
            .unwrap();

        recorder.run_to_end().unwrap();
        recorder
            .finish_full_session()
            .expect("a valid seven-tick frame interval must not inflate past the next GOP start")
            .expect("session summary");

        let first = recorder.ring().unwrap().segments().next().unwrap();
        assert_eq!((first.samples[1].duration_s * 90_000.0).round(), 7.0);
        assert!(clipline_mp4::walker::movie_duration_s(&std::fs::read(path).unwrap()).is_some());
    }


    #[test]
    fn repeated_sub_tick_gaps_do_not_accumulate_past_the_next_gop() {
        let dir = TestDir::new("clipline-pipeline", "repeated-sub-tick-gaps");
        let path = dir.path().join("session.mp4");
        let mut recorder = Recorder::new(
            MockCapture::new(8, 30),
            PtsRemapEncoder {
                inner: MockEncoder::new(4, 30),
                // WGC and MFT carry 100 ns timestamps. At a 90 kHz MP4
                // timescale, one 100 ns step is 0.009 tick, so two adjacent
                // steps must not become two permanent ticks of inflation.
                ticks: REPEATED_SUB_TICK_TICKS,
            },
            usize::MAX,
        );
        recorder
            .start_full_session(std::fs::File::create(&path).unwrap())
            .unwrap();

        recorder.run_to_end().unwrap();
        recorder
            .finish_full_session()
            .expect("two sub-tick gaps must not move the next GOP backward")
            .expect("session summary");

        let segments: Vec<_> = recorder.ring().unwrap().segments().collect();
        assert!((segments[0].pts_end_s() - segments[1].pts_start_s).abs() < 1e-12);
        assert!(segments[0]
            .samples
            .iter()
            .all(|sample| sample.duration_s * 90_000.0 >= 1.0));
        assert!(clipline_mp4::walker::movie_duration_s(&std::fs::read(path).unwrap()).is_some());
    }


    #[test]
    fn full_session_initializes_muxer_after_encoder_config_is_ready() {
        let dir = TestDir::new("clipline-pipeline", "full-session-lazy-config");
        let path = dir.path().join("session.mp4");
        let file = std::fs::File::create(&path).unwrap();
        let mut rec = Recorder::new(
            MockCapture::new(60, 30),
            DelayedTrackConfig {
                inner: MockEncoder::new(30, 30),
                encoded_any: false,
            },
            usize::MAX,
        );

        rec.start_full_session(file).unwrap();
        rec.run_to_end().unwrap();
        rec.finish_full_session().unwrap().expect("session summary");

        let data = std::fs::read(&path).unwrap();
        assert!(
            data.windows(DELAYED_SPS.len()).any(|w| w == DELAYED_SPS),
            "full-session moov must use the encoder config populated by the first packets"
        );
    }


    #[test]
    fn full_session_write_failure_does_not_abort_replay_capture() {
        let mut rec = Recorder::new(
            MockCapture::new(90, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        );

        rec.start_full_session(FailingWriter).unwrap();
        rec.run_to_end()
            .expect("secondary session sink must not stop capture");

        assert_eq!(rec.ring().unwrap().len(), 3);
        let err = rec.finish_full_session().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);
    }


    #[test]
    fn full_session_queue_budget_failure_does_not_abort_replay_capture() {
        let mut rec = Recorder::new(
            MockCapture::new(90, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        );

        rec.start_full_session_with_limits(std::io::Cursor::new(Vec::new()), 1, 1)
            .unwrap();
        rec.run_to_end()
            .expect("full-session backpressure must not stop replay capture");

        assert_eq!(rec.ring().unwrap().len(), 3);
        let err = rec.finish_full_session().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
        assert!(
            err.to_string().contains("queue byte budget"),
            "unexpected error: {err}"
        );
    }


    #[test]
    fn full_session_queue_byte_reservations_never_exceed_the_limit() {
        let queued = AtomicUsize::new(0);

        assert!(try_reserve_queue_bytes(&queued, 6, 10));
        assert_eq!(queued.load(Ordering::Acquire), 6);
        assert!(!try_reserve_queue_bytes(&queued, 5, 10));
        assert_eq!(queued.load(Ordering::Acquire), 6);

        queued.fetch_sub(6, Ordering::AcqRel);
        assert!(try_reserve_queue_bytes(&queued, 10, 10));
        assert_eq!(queued.load(Ordering::Acquire), 10);
    }


    #[test]
    fn stalled_full_session_writer_hits_segment_limit_without_blocking_capture() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let writer = GatedWriter {
            inner: std::io::Cursor::new(Vec::new()),
            entered: Some(entered_tx),
            release: release_rx,
        };
        let mut rec = Recorder::new(
            MockCapture::new(90, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        );
        rec.start_full_session_with_limits(writer, usize::MAX, 1)
            .unwrap();

        for _ in 0..31 {
            assert!(rec.step().unwrap());
        }
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("writer must begin the first segment");
        rec.run_to_end()
            .expect("stalled full-session output must not block capture");
        assert_eq!(rec.ring().unwrap().len(), 3);

        release_tx.send(()).unwrap();
        let err = rec.finish_full_session().unwrap_err();
        assert!(
            err.to_string().contains("segment limit"),
            "unexpected error: {err}"
        );
    }


    #[test]
    fn finish_stream_retains_audio_available_only_during_terminal_drain() {
        let mut recorder = Recorder::new(
            MockCapture::new(30, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        )
        .with_audio(Box::new(FinishOnlyAudioSource { finished: false }));

        recorder.run_to_end().unwrap();

        let segment = recorder.ring().unwrap().segments().next().unwrap();
        assert_eq!(segment.audio[0].samples.len(), 1);
        assert!((segment.audio[0].pts_start_s.unwrap() - 0.96).abs() < 1e-9);
    }


    #[test]
    fn straddling_audio_lead_in_does_not_break_full_session_finalization() {

        let dir = TestDir::new("clipline-pipeline", "straddling-audio-origin");
        let full_path = dir.path().join("full.mp4");
        let cap = OffsetCapture {
            inner: MockCapture::new(60, 30),
            // Deliberately place the first video frame inside the 500--520 ms
            // Opus packet rather than on a 20 ms packet boundary.
            offset_s: 0.51,
        };
        let mut recorder = Recorder::new(cap, MockEncoder::new(30, 30), usize::MAX)
            .with_audio(Box::new(MockAudioSource::new(48_000, 20)));
        recorder
            .start_full_session(std::fs::File::create(&full_path).unwrap())
            .unwrap();

        recorder.run_to_end().unwrap();
        let summary = recorder.finish_full_session().unwrap().unwrap();
        let first = recorder.ring().unwrap().segments().next().unwrap();
        assert!(
            first.audio[0].pts_start_s.unwrap() >= first.pts_start_s - 1e-9,
            "the first kept Opus packet must not precede the video origin"
        );
        assert!((summary.duration_s - 2.0).abs() < 1e-6);
        assert!(
            clipline_mp4::walker::movie_duration_s(&std::fs::read(full_path).unwrap()).is_some()
        );
    }


    #[test]
    fn full_session_started_mid_stream_drops_audio_before_its_new_origin() {

        let dir = TestDir::new("clipline-pipeline", "mid-stream-full-session-origin");
        let full_path = dir.path().join("full.mp4");
        let cap = OffsetCapture {
            inner: MockCapture::new(90, 30),
            offset_s: 0.51,
        };
        let mut recorder = Recorder::new(cap, MockEncoder::new(30, 30), usize::MAX)
            .with_audio(Box::new(MockAudioSource::new(48_000, 20)));

        for _ in 0..45 {
            assert!(recorder.step().unwrap());
        }
        recorder
            .start_full_session(std::fs::File::create(&full_path).unwrap())
            .unwrap();
        recorder.run_to_end().unwrap();

        let segments: Vec<_> = recorder.ring().unwrap().segments().collect();
        assert!(
            segments[1].audio[0].pts_start_s.unwrap() < segments[1].pts_start_s,
            "fixture must attach while audio straddles the next GOP origin"
        );
        let summary = recorder.finish_full_session().unwrap().unwrap();
        assert!((summary.duration_s - 2.0).abs() < 1e-6);
        assert!(
            clipline_mp4::walker::movie_duration_s(&std::fs::read(full_path).unwrap()).is_some()
        );
    }
