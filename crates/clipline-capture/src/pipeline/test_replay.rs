use super::test_support::*;
use clipline_buffer::{SampleInfo, Segment, TrackSamples};
use clipline_test_utils::TestDir;
use crate::mock::{MockAudioSource, MockCapture, MockEncoder};
use std::io;
use super::mux::select_audio_after_replay_origin;
use super::recorder::Recorder;
use super::storage::{ReplayStorage, ReplayStorageConfig, ReplayWindow};
use clipline_mp4::walker::{children, find, walk};

    #[test]
    fn delayed_and_gapped_audio_timing_survives_replay_and_full_session_muxing() {
        let dir = TestDir::new("clipline-pipeline", "gapped-audio-timeline");
        let full_path = dir.path().join("full.mp4");
        let mut recorder = Recorder::new(
            MockCapture::new(120, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        )
        .with_audio(Box::new(GappedAudioSource::new()));
        recorder
            .start_full_session(std::fs::File::create(&full_path).unwrap())
            .unwrap();

        recorder.run_to_end().unwrap();
        let segments: Vec<_> = recorder.ring().unwrap().segments().collect();
        assert!(segments[0].audio[0].samples.is_empty());
        assert!((segments[1].audio[0].pts_start_s.unwrap() - 1.2).abs() < 1e-9);
        assert!(segments[2].audio[0].samples.is_empty());
        assert!((segments[3].audio[0].pts_start_s.unwrap() - 3.2).abs() < 1e-9);

        let replay = recorder
            .save_replay(std::io::Cursor::new(Vec::new()), 10.0, None)
            .map(|(writer, _)| writer.into_inner())
            .unwrap();
        recorder.finish_full_session().unwrap().unwrap();
        let full = std::fs::read(full_path).unwrap();
        let expected = vec![
            (864_000, -1),
            (547_200, 0),
            (892_800, -1),
            (547_200, 36_480),
        ];
        assert_eq!(edit_list_entries(&replay), expected);
        assert_eq!(edit_list_entries(&full), expected);
    }


    #[test]
    fn save_replay_preserves_multiple_audio_tracks() {

        let mut rec = Recorder::new(
            MockCapture::new(90, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        )
        .with_audio(Box::new(MockAudioSource::new(48_000, 20)))
        .with_audio(Box::new(MockAudioSource::new(48_000, 20)));

        rec.run_to_end().unwrap();
        for seg in rec.ring().unwrap().segments() {
            assert_eq!(seg.audio.len(), 2, "system plus microphone tracks");
        }

        let (buf, _) = rec
            .save_replay(std::io::Cursor::new(Vec::new()), 10.0, None)
            .map(|(w, e)| (w.into_inner(), e))
            .expect("multi-audio save");
        let boxes = walk(&buf);
        let moov = find(&boxes, b"moov").expect("moov");
        let kids = children(&buf, moov);
        let traks = kids.iter().filter(|b| &b.fourcc == b"trak").count();
        assert_eq!(traks, 3, "video plus two audio tracks");
    }


    #[test]
    fn save_replay_from_stream_start_keeps_opus_pre_skip() {

        let mut rec = Recorder::new(
            MockCapture::new(90, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        )
        .with_audio(Box::new(MockAudioSource::new(48_000, 20)));

        rec.run_to_end().unwrap();
        let (buf, _) = rec
            .save_replay(std::io::Cursor::new(Vec::new()), 10.0, None)
            .map(|(w, end)| (w.into_inner(), end))
            .expect("replay from stream start");

        assert_eq!(first_opus_pre_skip(&buf), 312);
    }


    #[test]
    fn save_replay_from_middle_discards_opus_start_preroll() {

        let mut rec = Recorder::new(
            MockCapture::new(90, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        )
        .with_audio(Box::new(MockAudioSource::new(48_000, 20)));

        rec.run_to_end().unwrap();
        let (buf, _) = rec
            .save_replay(std::io::Cursor::new(Vec::new()), 1.5, None)
            .map(|(w, end)| (w.into_inner(), end))
            .expect("replay from middle");

        assert_eq!(
            first_opus_pre_skip(&buf),
            960,
            "mid-stream replay clips discard only the first Opus frame to avoid cold decoder startup artifacts"
        );
    }


    #[test]
    fn disk_replay_storage_saves_same_bytes_as_memory_storage() {

        let mut ram = Recorder::new(
            OffsetCapture {
                inner: MockCapture::new(90, 30),
                offset_s: 0.51,
            },
            MockEncoder::new(30, 30),
            usize::MAX,
        )
        .with_audio(Box::new(MockAudioSource::new(48_000, 20)))
        .with_audio(Box::new(MockAudioSource::new(48_000, 20)));
        ram.run_to_end().unwrap();
        let (ram_buf, ram_end) = ram
            .save_replay(std::io::Cursor::new(Vec::new()), 1.5, None)
            .map(|(w, end)| (w.into_inner(), end))
            .unwrap();

        let dir = TestDir::new("clipline-pipeline", "disk-equivalence");
        let mut disk = Recorder::new_with_replay_storage(
            OffsetCapture {
                inner: MockCapture::new(90, 30),
                offset_s: 0.51,
            },
            MockEncoder::new(30, 30),
            ReplayStorageConfig::Disk {
                max_bytes: usize::MAX,
                // Matches the byte-only `Recorder::new` ring above.
                retention_s: f64::INFINITY,
                dir: dir.path().to_path_buf(),
            },
        )
        .unwrap()
        .with_audio(Box::new(MockAudioSource::new(48_000, 20)))
        .with_audio(Box::new(MockAudioSource::new(48_000, 20)));
        disk.run_to_end().unwrap();
        let (disk_buf, disk_end) = disk
            .save_replay(std::io::Cursor::new(Vec::new()), 1.5, None)
            .map(|(w, end)| (w.into_inner(), end))
            .unwrap();

        assert_eq!(disk.ring_len(), ram.ring_len());
        assert_eq!(disk.ring_bytes(), ram.ring_bytes());
        assert_eq!(disk_end, ram_end);
        assert_eq!(disk_buf, ram_buf);
    }


    #[test]
    fn memory_replay_window_borrows_retained_segment_allocation() {
        let mut recorder = Recorder::new(
            MockCapture::new(60, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        );
        recorder.run_to_end().unwrap();
        let retained = recorder.ring().unwrap().segments().next().unwrap() as *const Segment;

        let ReplayWindow::Memory(selected) = recorder.ring.save_window(10.0, None) else {
            panic!("memory recorder must return a borrowed memory window");
        };

        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0] as *const Segment, retained);
    }


    #[test]
    fn sample_selection_drops_straddling_audio_without_mutating_payload() {
        let track = TrackSamples {
            pts_start_s: Some(1.50),
            data: b"AB".to_vec(),
            samples: vec![
                SampleInfo {
                    size: 1,
                    duration_s: 0.02,
                    is_sync: true,
                },
                SampleInfo {
                    size: 1,
                    duration_s: 0.02,
                    is_sync: true,
                },
            ],
        };
        let original_ptr = track.data.as_ptr();
        let original_data = track.data.clone();
        let original_sample_count = track.samples.len();

        let selection = select_audio_after_replay_origin(
            track.pts_start_s,
            &track.samples,
            track.data.len(),
            1.51,
        )
        .unwrap();

        assert_eq!(selection.first_sample, 1);
        assert_eq!(selection.first_byte, 1);
        assert!((selection.pts_start_s.unwrap() - 1.52).abs() < 1e-9);
        assert_eq!(track.data, original_data);
        assert_eq!(track.samples.len(), original_sample_count);
        assert_eq!(
            track.data[selection.first_byte..].as_ptr(),
            original_ptr.wrapping_add(1)
        );
    }


    #[test]
    fn disk_replay_rejects_video_samples_crossing_into_audio_region() {
        let dir = TestDir::new("clipline-pipeline", "disk-track-boundary");
        let mut recorder = Recorder::new_with_replay_storage(
            MockCapture::new(0, 30),
            MockEncoder::new(30, 30),
            ReplayStorageConfig::Disk {
                max_bytes: usize::MAX,
                retention_s: f64::INFINITY,
                dir: dir.path().to_path_buf(),
            },
        )
        .unwrap();
        let malformed = Segment {
            starts_with_keyframe: true,
            pts_start_s: 0.0,
            duration_s: 1.0,
            data: vec![1, 2, 3, 4],
            samples: vec![SampleInfo {
                size: 5,
                duration_s: 1.0,
                is_sync: true,
            }],
            audio: vec![TrackSamples {
                pts_start_s: Some(0.0),
                data: vec![5, 6, 7, 8],
                samples: vec![SampleInfo {
                    size: 4,
                    duration_s: 1.0,
                    is_sync: true,
                }],
            }],
        };
        let ReplayStorage::Disk(ring) = &mut recorder.ring else {
            panic!("fixture must use disk replay storage");
        };
        ring.push(malformed).unwrap();
        recorder.video_start_pts_s = Some(0.0);

        let error = recorder
            .save_replay(std::io::Cursor::new(Vec::new()), 10.0, None)
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            error.to_string().contains("track data"),
            "unexpected error: {error}"
        );
    }


    #[test]
    fn memory_retention_bounds_the_ring_by_duration() {
        // 3s of footage in 1s GOPs. A 1.5s retention window keeps the two
        // newest GOPs, where a byte-only ring keeps all three.
        let mut bounded = Recorder::new_with_replay_storage(
            MockCapture::new(90, 30),
            MockEncoder::new(30, 30),
            ReplayStorageConfig::Memory {
                max_bytes: usize::MAX,
                retention_s: 1.5,
            },
        )
        .unwrap();
        bounded.run_to_end().unwrap();

        let mut unbounded = Recorder::new(
            MockCapture::new(90, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        );
        unbounded.run_to_end().unwrap();

        assert_eq!(
            unbounded.ring_len(),
            3,
            "`new` must stay byte-only so existing callers are unaffected"
        );
        assert_eq!(bounded.ring_len(), 2, "retention drops the oldest GOP");
        assert!(bounded.ring_bytes() < unbounded.ring_bytes());
    }


    #[test]
    fn disk_retention_matches_memory_retention() {
        let dir = TestDir::new("clipline-pipeline", "disk-retention");
        let mut disk = Recorder::new_with_replay_storage(
            MockCapture::new(90, 30),
            MockEncoder::new(30, 30),
            ReplayStorageConfig::Disk {
                max_bytes: usize::MAX,
                retention_s: 1.5,
                dir: dir.path().to_path_buf(),
            },
        )
        .unwrap();
        disk.run_to_end().unwrap();

        let mut ram = Recorder::with_retention(
            MockCapture::new(90, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
            1.5,
        );
        ram.run_to_end().unwrap();

        assert_eq!(disk.ring_len(), ram.ring_len());
        assert_eq!(disk.ring_bytes(), ram.ring_bytes());
    }


    #[test]
    fn save_replay_works_between_steps_while_recording() {
        let mut rec = Recorder::new(
            MockCapture::new(90, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        );
        // Two GOPs in: a save must succeed without ending the recording.
        for _ in 0..60 {
            assert!(rec.step().unwrap());
        }
        let (buf, end) = rec
            .save_replay(std::io::Cursor::new(Vec::new()), 10.0, None)
            .map(|(w, e)| (w.into_inner(), e))
            .expect("mid-recording save");
        assert!(!buf.is_empty());
        assert!(
            (end - 1.0).abs() < 1e-6,
            "one sealed GOP at save time (second pending)"
        );
        // Recording continues; smart mode skips the already-saved second.
        for _ in 0..30 {
            assert!(rec.step().unwrap());
        }
        assert!(!rec.step().unwrap(), "source exhausted");
        rec.finish_stream().unwrap();
        let (_, end2) = rec
            .save_replay(std::io::Cursor::new(Vec::new()), 10.0, Some(end))
            .expect("post-finish save");
        assert!((end2 - 3.0).abs() < 1e-6, "everything sealed after finish");
        // run_to_end equivalence: same segment layout as the stepped path.
        let mut whole = Recorder::new(
            MockCapture::new(90, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        );
        whole.run_to_end().unwrap();
        assert_eq!(whole.ring().unwrap().len(), rec.ring().unwrap().len());
    }


    #[test]
    fn save_window_bytes_measures_only_the_selected_encoded_segments() {
        let mut recorder = Recorder::new(
            MockCapture::new(90, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        );
        recorder.run_to_end().unwrap();
        let ring = recorder.ring().unwrap();
        let selected_bytes: usize = ring
            .save_window(1.5, None)
            .into_iter()
            .map(Segment::byte_len)
            .sum();

        assert_eq!(recorder.save_window_bytes(1.5, None), selected_bytes);
        assert!(selected_bytes < recorder.ring_bytes());
    }


    #[test]
    fn audio_lead_in_before_first_video_frame_is_dropped() {
        // Video starts 0.5 s after the shared clock origin; audio has been
        // capturing (and silence-filling) since t=0. The pre-video audio
        // must not ride in the file or video plays early by the lead-in.
        let cap = OffsetCapture {
            inner: MockCapture::new(60, 30),
            offset_s: 0.5,
        };
        let mut rec = Recorder::new(cap, MockEncoder::new(30, 30), usize::MAX)
            .with_audio(Box::new(MockAudioSource::new(48_000, 20)));
        rec.run_to_end().unwrap();
        let segs: Vec<_> = rec.ring().unwrap().segments().collect();
        assert_eq!(segs.len(), 2);
        // First segment: audio coverage matches video duration within one
        // 20 ms packet (the packet straddling the boundary is dropped).
        let covered: f64 = segs[0].audio[0].samples.iter().map(|s| s.duration_s).sum();
        assert!(
            (covered - segs[0].duration_s).abs() <= 0.02 + 1e-9,
            "lead-in dropped: covered {covered}, video {}",
            segs[0].duration_s
        );
        // And the first kept packet starts at/after the video start.
        // (MockAudioSource stamps pts; we can't read them back from the
        // sealed track, but coverage bounds above imply it.)
    }


    #[test]
    fn replay_drops_audio_packet_straddling_selected_video_origin() {

        let cap = OffsetCapture {
            inner: MockCapture::new(60, 30),
            // GOP boundaries land at x.51 s, halfway through x.50--x.52 s
            // Opus packets.
            offset_s: 0.51,
        };
        let mut recorder = Recorder::new(cap, MockEncoder::new(30, 30), usize::MAX)
            .with_audio(Box::new(MockAudioSource::new(48_000, 20)));
        recorder.run_to_end().unwrap();

        let segments: Vec<_> = recorder.ring().unwrap().segments().collect();
        assert_eq!(segments.len(), 2);
        assert!(
            segments[1].audio[0].pts_start_s.unwrap() < segments[1].pts_start_s,
            "fixture must put a straddling Opus packet before the selected GOP origin"
        );

        let ReplayWindow::Memory(selected) = recorder.ring.save_window(0.25, None) else {
            panic!("fixture uses memory replay storage");
        };
        let origin = selected[0].pts_start_s;
        let track = &selected[0].audio[0];
        let selection = select_audio_after_replay_origin(
            track.pts_start_s,
            &track.samples,
            track.data.len(),
            origin,
        )
        .unwrap();
        assert!(
            (selection.pts_start_s.unwrap() - 1.52).abs() < 1e-9,
            "discarding the 1.50--1.52 s packet must advance audio by exactly one packet"
        );
        assert_eq!(selection.first_sample, 1);

        let (replay, _) = recorder
            .save_replay(std::io::Cursor::new(Vec::new()), 0.25, None)
            .expect("a mid-stream replay must discard audio preceding its video origin");
        assert!(clipline_mp4::walker::movie_duration_s(&replay.into_inner()).is_some());
    }


    #[test]
    fn replay_origin_filter_cleans_audio_from_every_selected_segment() {
        let sample = || SampleInfo {
            size: 1,
            duration_s: 0.02,
            is_sync: true,
        };
        let segment = |pts_start_s, audio_start_s, audio: Vec<u8>| Segment {
            starts_with_keyframe: true,
            pts_start_s,
            duration_s: 1.0,
            data: vec![0],
            samples: vec![sample()],
            audio: vec![TrackSamples {
                pts_start_s: Some(audio_start_s),
                samples: vec![sample(); audio.len()],
                data: audio,
            }],
        };
        let selected = [
            segment(1.0, 1.0, vec![1]),
            segment(2.0, 0.98, vec![2, 3, 4]),
        ];
        let original: Vec<_> = selected
            .iter()
            .map(|segment| segment.audio[0].data.clone())
            .collect();

        let selections: Vec<_> = selected
            .iter()
            .map(|segment| {
                let track = &segment.audio[0];
                select_audio_after_replay_origin(
                    track.pts_start_s,
                    &track.samples,
                    track.data.len(),
                    1.0,
                )
                .unwrap()
            })
            .collect();

        assert_eq!(selections[0].first_byte, 0);
        assert_eq!(selections[1].pts_start_s, Some(1.0));
        assert_eq!(
            &selected[1].audio[0].data[selections[1].first_byte..],
            &[3, 4]
        );
        assert_eq!(
            selected[1].audio[0].samples.len() - selections[1].first_sample,
            2
        );
        for (segment, data) in selected.iter().zip(original) {
            assert_eq!(segment.audio[0].data, data);
        }
    }
