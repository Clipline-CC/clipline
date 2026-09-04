//! Recorder unit tests (moved verbatim from service.rs).
#[cfg(test)]
fn clips_dir_resolved_with_probe(
    media_dir: &Path,
    fallback: impl FnOnce() -> PathBuf,
    mut probe: impl FnMut(&Path) -> std::io::Result<()>,
) -> Result<(PathBuf, bool), String> {
    media_root::clips_dir_resolved_with_probe(media_dir, fallback, &mut probe)
}

    use super::{
        ffmpeg_capability_identity, ffmpeg_capability_slot, full_session_quota_check,
        quota_would_be_exceeded, storage_quota_full_event, Event, FullSessionRecording,
    };
    use clipline_capture::{Codec, EncoderApi, EncoderBackend, EncoderCapability};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn saved_media_quota_blocks_only_when_the_requested_write_would_cross_it() {
        assert!(!quota_would_be_exceeded(90, Some(100), 10));
        assert!(quota_would_be_exceeded(90, Some(100), 11));
        assert!(quota_would_be_exceeded(u64::MAX, Some(u64::MAX), 1));
        assert!(!quota_would_be_exceeded(u64::MAX, None, u64::MAX));
    }

    #[test]
    fn unreadable_quota_status_skips_the_check_instead_of_stopping_recording() {
        let dir = clipline_test_utils::TestDir::new("clipline-service", "quota-inspection-error");
        let file = dir.path().join("not-a-media-directory");
        std::fs::write(&file, b"file").unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        assert!(storage_quota_full_event(&tx, &file, Some(100), 1, false).is_none());
        assert!(storage_quota_full_event(&tx, &file, Some(100), 1, true).is_none());
    }

    #[test]
    fn auto_delete_keeps_favorites_and_drains_sessions_before_replays_before_trims() {
        let dir = clipline_test_utils::TestDir::new("clipline-service", "quota-policy-order");
        // The favorited session is the OLDEST clip, so plain oldest-first GC
        // would delete it first; kind priority must protect it and drain the
        // replay before the trim instead.
        let session = dir.path().join("session_1784525639.mp4");
        std::fs::write(&session, [0; 100]).unwrap();
        clipline_storage::ensure_clip_owned(&session).unwrap();
        crate::library::set_clip_favorite_impl(&session, true).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let replay = dir.path().join("clip_1784525638.mp4");
        std::fs::write(&replay, [0; 100]).unwrap();
        clipline_storage::ensure_clip_owned(&replay).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let trim = dir.path().join("clip_1_trim_001000_002000.mp4");
        std::fs::write(&trim, [0; 100]).unwrap();
        clipline_storage::ensure_clip_owned(&trim).unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        // A 150-byte budget requires both deletable clips to be removed, while
        // the favorited session stays.
        assert!(
            storage_quota_full_event(&tx, dir.path(), Some(150), 1, true).is_none(),
            "auto-delete should free room without touching the favorite"
        );
        assert!(session.exists(), "favorites must never be auto-deleted");
        assert!(!replay.exists(), "replays must drain before trims");
        assert!(!trim.exists());
    }

    #[test]
    fn auto_delete_makes_room_before_emitting_quota_full() {
        let dir = clipline_test_utils::TestDir::new("clipline-service", "quota-auto-delete");
        let old = dir.path().join("old.mp4");
        let keep = dir.path().join("keep.mp4");
        std::fs::write(&old, [0; 80]).unwrap();
        clipline_storage::ensure_clip_owned(&old).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&keep, [0; 10]).unwrap();
        clipline_storage::ensure_clip_owned(&keep).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        assert!(
            storage_quota_full_event(&tx, dir.path(), Some(100), 30, false).is_some(),
            "without auto-delete the requested write must still be blocked"
        );
        assert!(
            storage_quota_full_event(&tx, dir.path(), Some(100), 30, true).is_none(),
            "auto-delete should free the oldest clip and allow the write"
        );
        assert!(!old.exists());
        assert!(keep.exists());
        assert!(
            matches!(rx.try_recv(), Ok(Event::LibraryChanged)),
            "auto-delete must ask the UI to refresh after removing clips"
        );
    }

    #[test]
    fn full_session_quota_uses_cached_library_bytes_plus_active_file() {
        let dir = clipline_test_utils::TestDir::new("clipline-service", "quota-active-file");
        let temp_path = dir.path().join("session.mp4.recording");
        std::fs::write(&temp_path, [0; 15]).unwrap();
        let recording = FullSessionRecording {
            final_path: dir.path().join("session.mp4"),
            temp_path,
            wall_start_unix: 0,
        };

        let (tx, _rx) = std::sync::mpsc::channel();
        let check = full_session_quota_check(
            &tx,
            dir.path(),
            &recording,
            Some(80),
            Some(100),
            6,
            false,
        );
        assert!(matches!(
            check.event,
            Some(Event::StorageQuotaFull {
                total_bytes: 95,
                quota_bytes: 100,
                required_bytes: 6,
            })
        ));
        assert_eq!(check.new_baseline_bytes, None);
    }

    #[test]
    fn full_session_auto_delete_updates_baseline_and_refreshes_library() {
        let dir = clipline_test_utils::TestDir::new("clipline-service", "quota-full-session-gc");
        let old = dir.path().join("old.mp4");
        std::fs::write(&old, [0; 80]).unwrap();
        clipline_storage::ensure_clip_owned(&old).unwrap();
        let temp_path = dir.path().join("session.mp4.recording");
        std::fs::write(&temp_path, [0; 15]).unwrap();
        // Production reservations mark the active recording owned so inventory
        // counts it (and refuses to delete it) during auto-delete.
        clipline_storage::ensure_clip_owned(&temp_path).unwrap();
        let recording = FullSessionRecording {
            final_path: dir.path().join("session.mp4"),
            temp_path,
            wall_start_unix: 0,
        };

        let (tx, rx) = std::sync::mpsc::channel();
        // Baseline is only the saved library; active recording is added each tick.
        let check = full_session_quota_check(
            &tx,
            dir.path(),
            &recording,
            Some(80),
            Some(100),
            6,
            true,
        );
        assert!(check.event.is_none(), "cleanup should make room for the reserve");
        assert!(!old.exists());
        assert!(matches!(rx.try_recv(), Ok(Event::LibraryChanged)));
        let baseline = check.new_baseline_bytes.expect("baseline must refresh");
        assert_eq!(baseline, 0);
    }

    #[test]
    fn ffmpeg_capability_slot_reuses_same_identity() {
        let probes = AtomicUsize::new(0);
        let first = ffmpeg_capability_slot(None, "managed:x:abc", || {
            probes.fetch_add(1, Ordering::SeqCst);
            vec![EncoderCapability {
                api: EncoderApi::Ffmpeg,
                backend: EncoderBackend::SvtAv1,
                codecs: vec![Codec::Av1],
            }]
        });
        let second = ffmpeg_capability_slot(Some(&first), "managed:x:abc", || {
            probes.fetch_add(1, Ordering::SeqCst);
            panic!("should not reprobe");
        });
        assert_eq!(first.identity, second.identity);
        assert_eq!(first.caps.len(), second.caps.len());
        assert_eq!(probes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn ffmpeg_capability_slot_reprobes_when_identity_changes() {
        let probes = AtomicUsize::new(0);
        let first = ffmpeg_capability_slot(None, "missing", || {
            probes.fetch_add(1, Ordering::SeqCst);
            Vec::new()
        });
        let second = ffmpeg_capability_slot(Some(&first), "external:C:/ffmpeg.exe", || {
            probes.fetch_add(1, Ordering::SeqCst);
            vec![EncoderCapability {
                api: EncoderApi::Ffmpeg,
                backend: EncoderBackend::Nvenc,
                codecs: vec![Codec::H264],
            }]
        });
        assert_ne!(first.identity, second.identity);
        assert_eq!(second.caps.len(), 1);
        assert_eq!(probes.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn ffmpeg_capability_identity_prefers_managed_over_external() {
        let managed = crate::ffmpeg_runtime::ManagedRuntimeInfo {
            dir: std::path::PathBuf::from("C:/managed"),
            ffmpeg_exe: std::path::PathBuf::from("C:/managed/ffmpeg.exe"),
            release_tag: "tag".into(),
            archive_sha256: "aa".into(),
            manifest_sha256: "bb".into(),
        };
        assert_eq!(
            ffmpeg_capability_identity(
                Some(&managed),
                Some(std::path::Path::new("C:/ext/ffmpeg.exe"))
            ),
            "managed:C:/managed:bb"
        );
        assert_eq!(
            ffmpeg_capability_identity(None, Some(std::path::Path::new("C:/ext/ffmpeg.exe"))),
            "external:C:/ext/ffmpeg.exe"
        );
        assert_eq!(ffmpeg_capability_identity(None, None), "missing");
    }

    use super::*;
    use clipline_capture::{MockCapture, MockEncoder};
    use clipline_test_utils::TestDir;
    use std::collections::VecDeque;

    struct TimeoutSource;

    impl TimedFrameSource for TimeoutSource {
        fn next_frame_timeout(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError> {
            std::thread::sleep(timeout);
            Err(CaptureError::Timeout(timeout))
        }
    }

    struct ScriptedTimedSource {
        outcomes: VecDeque<Result<Option<Frame>, CaptureError>>,
        requested_timeouts: Vec<Duration>,
    }

    impl TimedFrameSource for ScriptedTimedSource {
        fn next_frame_timeout(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError> {
            self.requested_timeouts.push(timeout);
            let outcome = self
                .outcomes
                .pop_front()
                .expect("scripted timed source exhausted");
            if matches!(outcome, Err(CaptureError::Timeout(_))) {
                std::thread::sleep(timeout);
            }
            outcome
        }
    }

    struct DelayedFrameSource {
        frame: Option<Frame>,
        delay: Duration,
        requested_timeouts: Vec<Duration>,
    }

    impl TimedFrameSource for DelayedFrameSource {
        fn next_frame_timeout(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError> {
            self.requested_timeouts.push(timeout);
            if let Some(frame) = self.frame.take() {
                std::thread::sleep(self.delay);
                Ok(Some(frame))
            } else {
                std::thread::sleep(timeout);
                Err(CaptureError::Timeout(timeout))
            }
        }
    }

    struct BlockingTimeoutSource {
        requested_timeouts: Vec<Duration>,
    }

    impl TimedFrameSource for BlockingTimeoutSource {
        fn next_frame_timeout(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError> {
            self.requested_timeouts.push(timeout);
            std::thread::sleep(timeout);
            Err(CaptureError::Timeout(timeout))
        }
    }

    struct PrematureTimeoutSource {
        delay: Duration,
    }

    impl TimedFrameSource for PrematureTimeoutSource {
        fn next_frame_timeout(
            &mut self,
            _timeout: Duration,
        ) -> Result<Option<Frame>, CaptureError> {
            std::thread::sleep(self.delay);
            Err(CaptureError::Timeout(self.delay))
        }
    }

    #[test]
    fn video_encoder_id_matches_serde_serialization() {
        // The Settings dropdown sends EncoderOption.id; settings.rs maps it
        // back through VideoEncoder's snake_case serde. id() must stay in
        // lockstep with that derive, including the new codec variants.
        for enc in [
            VideoEncoder::Auto,
            VideoEncoder::NvencH264,
            VideoEncoder::NvencHevc,
            VideoEncoder::NvencAv1,
            VideoEncoder::AmfH264,
            VideoEncoder::AmfHevc,
            VideoEncoder::AmfAv1,
            VideoEncoder::QuickSyncH264,
            VideoEncoder::QuickSyncHevc,
            VideoEncoder::QuickSyncAv1,
            VideoEncoder::SvtAv1,
        ] {
            let serialized = serde_json::to_string(&enc).unwrap();
            assert_eq!(serialized, format!("\"{}\"", enc.id()));
        }
    }

    #[test]
    fn from_parts_round_trips_through_preference() {
        // Every explicit option maps back to the same (backend, codec).
        for (backend, codec) in [
            (EncoderBackend::Amf, Codec::Hevc),
            (EncoderBackend::Nvenc, Codec::Av1),
            (EncoderBackend::SvtAv1, Codec::Av1),
        ] {
            let enc = VideoEncoder::from_parts(backend, codec).unwrap();
            assert_eq!(
                enc.preference(),
                EncoderPreference::Explicit { backend, codec }
            );
        }
        assert!(VideoEncoder::from_parts(EncoderBackend::MfSoftware, Codec::H264).is_none());
        assert!(VideoEncoder::from_parts(EncoderBackend::SvtAv1, Codec::H264).is_none());
    }

    #[test]
    fn native_media_foundation_software_uses_synchronous_mft() {
        assert_eq!(
            mft_encoder_path(EncoderBackend::MfSoftware),
            MftEncoderPath::Software
        );
        assert_eq!(
            mft_encoder_path(EncoderBackend::Amf),
            MftEncoderPath::Hardware
        );
    }

    #[test]
    fn ffmpeg_media_foundation_software_uses_cpu_frame_conversion() {
        assert_eq!(
            ffmpeg_conversion_path(EncoderBackend::MfSoftware),
            FfmpegConversionPath::Cpu
        );
        assert_eq!(
            ffmpeg_conversion_path(EncoderBackend::Nvenc),
            FfmpegConversionPath::Gpu
        );
        assert_eq!(
            ffmpeg_conversion_path(EncoderBackend::SvtAv1),
            FfmpegConversionPath::Gpu
        );
    }

    #[test]
    fn output_dimensions_scale_down_to_selected_resolution() {
        assert_eq!(
            output_dimensions(2560, 1440, OutputResolution::Source),
            (2560, 1440)
        );
        assert_eq!(
            output_dimensions(2560, 1440, OutputResolution::P1080),
            (1920, 1080)
        );
        assert_eq!(
            output_dimensions(2560, 1440, OutputResolution::P720),
            (1280, 720)
        );
    }

    #[test]
    fn output_dimensions_preserve_aspect_and_never_upscale() {
        assert_eq!(
            output_dimensions(1600, 1000, OutputResolution::P1080),
            (1600, 1000)
        );
        assert_eq!(
            output_dimensions(5120, 1440, OutputResolution::P1080),
            (1920, 540)
        );
        assert_eq!(
            output_dimensions(5120, 1440, OutputResolution::Source),
            (2560, 720)
        );
    }

    #[test]
    fn missing_display_region_falls_back_to_full_current_display_crop() {
        let region = CaptureRegion {
            display_id: Some(r"\\.\DISPLAY-GHOST".into()),
            x: 1920,
            y: 0,
            width: 2560,
            height: 1440,
        };
        let display = clipline_capture::windows::display::DisplayInfo {
            id: r"\\.\DISPLAY1".into(),
            name: "DISPLAY1".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            is_primary: true,
        };

        let (crop, recovered) = crop_for_region_or_full_display(&region, &display, true).unwrap();

        assert!(recovered);
        assert_eq!(
            crop,
            CropRect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080
            }
        );
    }

    #[test]
    fn recovered_capture_display_builds_user_visible_warning() {
        let region = CaptureRegion {
            display_id: Some(r"\\.\DISPLAY-GHOST".into()),
            x: 1920,
            y: 0,
            width: 2560,
            height: 1440,
        };
        let display = clipline_capture::windows::display::DisplayInfo {
            id: r"\\.\DISPLAY1".into(),
            name: "DISPLAY1".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            is_primary: true,
        };

        let message = capture_display_recovery_warning(&region, &display, true, false)
            .expect("recovery warning");

        assert!(message.contains(r"\\.\DISPLAY-GHOST"), "{message}");
        assert!(message.contains("DISPLAY1"), "{message}");
        assert!(message.contains("Settings"), "{message}");
    }

    #[test]
    fn out_of_bounds_region_clamps_to_visible_display_crop() {
        let region = CaptureRegion {
            display_id: Some(r"\\.\DISPLAY1".into()),
            x: 1000,
            y: 500,
            width: 1000,
            height: 800,
        };
        let display = clipline_capture::windows::display::DisplayInfo {
            id: r"\\.\DISPLAY1".into(),
            name: "DISPLAY1".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            is_primary: true,
        };

        let (crop, recovered) = crop_for_region_or_full_display(&region, &display, false).unwrap();

        assert!(recovered);
        assert_eq!(
            crop,
            CropRect {
                x: 1000,
                y: 500,
                width: 920,
                height: 580
            }
        );
    }

    #[test]
    fn full_display_region_survives_virtual_origin_change() {
        let region = CaptureRegion {
            display_id: Some(r"\\.\DISPLAY1".into()),
            x: 1280,
            y: 0,
            width: 2560,
            height: 1440,
        };
        let display = clipline_capture::windows::display::DisplayInfo {
            id: r"\\.\DISPLAY1".into(),
            name: "DISPLAY1".into(),
            x: 0,
            y: 0,
            width: 2560,
            height: 1440,
            is_primary: true,
        };

        let (crop, recovered) = crop_for_region_or_full_display(&region, &display, false).unwrap();

        assert!(
            !recovered,
            "a full-display selection should rebase without a settings warning"
        );
        assert_eq!(
            crop,
            CropRect {
                x: 0,
                y: 0,
                width: 2560,
                height: 1440
            }
        );
    }

    #[test]
    fn marker_source_falls_back_to_league_poller_without_active_plugin() {
        let opts = ServiceOptions::default();

        assert_eq!(
            marker_source_kind(&opts),
            MarkerSourceKind::LegacyLeaguePoller
        );
    }

    #[test]
    fn marker_source_uses_active_plugin_event_source_when_available() {
        let opts = ServiceOptions {
            active_game: Some(ActiveGame {
                identity: crate::game_identity::GameIdentity::built_in_plugin(
                    crate::game_plugins::LEAGUE_OF_LEGENDS_ID,
                )
                .unwrap(),
                name: "League of Legends".into(),
                exe_path: None,
                process_id: None,
            }),
            ..ServiceOptions::default()
        };

        assert_eq!(marker_source_kind(&opts), MarkerSourceKind::Plugin);
    }

    #[test]
    fn session_game_metadata_merges_a_late_league_queue() {
        let dir = TestDir::new("clipline-service", "league-session-queue");
        let game = ActiveGame {
            identity: crate::game_identity::GameIdentity::built_in_plugin(
                crate::game_plugins::LEAGUE_OF_LEGENDS_ID,
            )
            .unwrap(),
            name: "League of Legends".into(),
            exe_path: None,
            process_id: None,
        };
        let queue = clipline_lol::LeagueQueue::from_id(420);

        write_session_game_meta(dir.path(), Some(&game), None);
        write_session_game_meta(dir.path(), Some(&game), Some(&queue));

        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join(SESSION_META_FILE)).unwrap())
                .unwrap();
        assert_eq!(value["id"], crate::game_plugins::LEAGUE_OF_LEGENDS_ID);
        assert_eq!(value["name"], "League of Legends");
        assert_eq!(value["queue"]["id"], 420);
        assert_eq!(value["queue"]["category"], "ranked-solo-duo");
        assert_eq!(value["queue"]["label"], "Ranked Solo/Duo");
    }

    #[test]
    fn recovery_sweeps_each_media_root_in_the_same_process() {
        let first = TestDir::new("clipline-service", "recovery-sweep-first");
        let second = TestDir::new("clipline-service", "recovery-sweep-second");
        let first_session = first
            .write("2026-06-12 19-15/clipline-session.json", 2)
            .parent()
            .unwrap()
            .to_path_buf();
        let crashed = first.write("2026-06-12 19-15/session_1.mp4.recording", 0);
        clipline_storage::ensure_clip_owned(&crashed).unwrap();
        let second_session = second
            .write("2026-06-12 19-16/clipline-session.json", 2)
            .parent()
            .unwrap()
            .to_path_buf();
        let (events, _rx) = std::sync::mpsc::channel();

        recover_abandoned_recordings(first.path(), &events);
        recover_abandoned_recordings(second.path(), &events);

        assert!(!first_session.exists());
        assert!(!second_session.exists());
    }

    #[test]
    fn custom_identity_cannot_enable_a_built_in_marker_source() {
        let opts = ServiceOptions {
            active_game: Some(ActiveGame {
                identity: crate::game_identity::GameIdentity::custom(
                    crate::game_plugins::LEAGUE_OF_LEGENDS_ID,
                ),
                name: "Community game".into(),
                exe_path: None,
                process_id: None,
            }),
            ..ServiceOptions::default()
        };

        assert_eq!(
            marker_source_kind(&opts),
            MarkerSourceKind::LegacyLeaguePoller
        );
    }

    #[test]
    fn split_output_candidates_exclude_clipline_process() {
        let own_pid = 42;
        let processes = vec![
            clipline_capture::windows::wasapi::AudioProcessInfo {
                pid: own_pid,
                label: "clipline-app".into(),
                process_name: Some("clipline-app".into()),
                process_path: Some(r"C:\Clipline\clipline-app.exe".into()),
            },
            clipline_capture::windows::wasapi::AudioProcessInfo {
                pid: 99,
                label: "Game".into(),
                process_name: Some("Game".into()),
                process_path: Some(r"C:\Games\Game.exe".into()),
            },
        ];

        let candidates = split_output_process_candidates(processes, own_pid);

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].label, "Game");
    }

    fn player_summary(champion_name: &str, kills: u32, deaths: u32, assists: u32) -> PlayerSummary {
        PlayerSummary {
            champion_name: champion_name.into(),
            kills,
            deaths,
            assists,
            creep_score: None,
            game_time_s: None,
            player_name: String::new(),
            team: String::new(),
            participants: Vec::new(),
            summoner_spells: Vec::new(),
            items: Vec::new(),
        }
    }

    fn review_event(
        kind: EventKind,
        actor: &str,
        victim: Option<&str>,
        offset_s: f64,
        involves_local_player: bool,
    ) -> clipline_events::GameEvent {
        clipline_events::GameEvent {
            game_id: clipline_events::GameId::LeagueOfLegends,
            kind,
            actor: actor.into(),
            victim: victim.map(String::from),
            assisters: Vec::new(),
            subtype: None,
            game_time_s: offset_s,
            recording_offset_s: Some(offset_s),
            importance: 7,
            involves_local_player,
        }
    }

    #[test]
    fn player_summary_state_stops_replay_attribution_after_match_end() {
        let mut state = PlayerSummaryState::default();
        let mid_match = player_summary("Nautilus", 3, 4, 22);
        let final_match = player_summary("Nautilus", 3, 4, 23);

        state.match_started();
        state.update(mid_match.clone());
        assert_eq!(state.active_replay_summary(), Some(&mid_match));
        assert_eq!(state.full_session_summary(), Some(&mid_match));

        state.match_ended();
        assert_eq!(state.active_replay_summary(), None);
        assert_eq!(state.full_session_summary(), Some(&mid_match));

        state.update(final_match.clone());
        assert_eq!(state.active_replay_summary(), None);
        assert_eq!(state.full_session_summary(), Some(&final_match));

        state.match_started();
        assert_eq!(state.active_replay_summary(), None);
        assert_eq!(state.full_session_summary(), None);
    }

    #[test]
    fn write_marker_sidecar_keeps_player_summary_without_markers() {
        let dir = TestDir::new("clipline-service", "sidecar-summary");
        let path = dir.path().join("clip.mp4");
        let (tx, _rx) = std::sync::mpsc::channel();
        let summary = PlayerSummary {
            champion_name: "Nautilus".into(),
            kills: 3,
            deaths: 4,
            assists: 23,
            creep_score: Some(187),
            game_time_s: Some(1800),
            player_name: String::new(),
            team: String::new(),
            participants: Vec::new(),
            summoner_spells: Vec::new(),
            items: Vec::new(),
        };

        let count = write_marker_sidecar(
            &tx,
            &MarkerLog::new(),
            &path,
            0.0,
            10.0,
            Some(&summary),
            &[],
        );

        assert_eq!(count, 0);
        let json = std::fs::read_to_string(path.with_extension("markers.json")).unwrap();
        let sidecar: clipline_events::ClipMarkers = serde_json::from_str(&json).unwrap();
        assert!(sidecar.markers.is_empty());
        assert_eq!(sidecar.player_summary, Some(summary));
    }

    #[test]
    fn write_marker_sidecar_writes_a_bookmark_only_clip() {
        let dir = TestDir::new("clipline-service", "sidecar-bookmarks");
        let path = dir.path().join("clip.mp4");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut marker_log = MarkerLog::new();
        marker_log.push_bookmark(4.0);
        marker_log.push_bookmark(9.0);
        // Outside the clip window, so it must not reach the sidecar.
        marker_log.push_bookmark(30.0);

        let count = write_marker_sidecar(&tx, &marker_log, &path, 0.0, 10.0, None, &[]);

        assert_eq!(count, 2, "bookmarks count toward the clip's markers");
        let json = std::fs::read_to_string(path.with_extension("markers.json")).unwrap();
        let sidecar: clipline_events::ClipMarkers = serde_json::from_str(&json).unwrap();
        assert!(sidecar.markers.is_empty());
        assert_eq!(
            sidecar
                .bookmarks
                .iter()
                .map(|bookmark| bookmark.t_s)
                .collect::<Vec<_>>(),
            [4.0, 9.0],
            "bookmarks survive the review-event filter"
        );
    }

    #[test]
    fn write_marker_sidecar_keeps_audio_tracks_without_markers() {
        let dir = TestDir::new("clipline-service", "sidecar-audio-tracks");
        let path = dir.path().join("clip.mp4");
        let (tx, _rx) = std::sync::mpsc::channel();
        let tracks = vec![audio_track("output", 0, "Output Audio", "output")];

        let count = write_marker_sidecar(&tx, &MarkerLog::new(), &path, 0.0, 10.0, None, &tracks);

        assert_eq!(count, 0);
        let json = std::fs::read_to_string(path.with_extension("markers.json")).unwrap();
        let sidecar: clipline_events::ClipMarkers = serde_json::from_str(&json).unwrap();
        assert!(sidecar.markers.is_empty());
        assert_eq!(sidecar.audio_tracks, tracks);
    }

    #[test]
    fn write_marker_sidecar_keeps_review_events_for_match_event_filters() {
        let dir = TestDir::new("clipline-service", "sidecar-review-events");
        let path = dir.path().join("clip.mp4");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut log = MarkerLog::new();
        log.push(review_event(
            EventKind::ChampionKill,
            "Enemy Mid",
            Some("Ally Top"),
            12.0,
            false,
        ));
        log.push(review_event(
            EventKind::ChampionAssist,
            "Dain",
            Some("Enemy Mid"),
            14.0,
            true,
        ));
        log.push(review_event(
            EventKind::HeraldKill,
            "Ally Jungle",
            None,
            16.0,
            false,
        ));
        log.push(review_event(
            EventKind::MinionsSpawning,
            "",
            None,
            18.0,
            false,
        ));

        let count = write_marker_sidecar(&tx, &log, &path, 10.0, 20.0, None, &[]);

        assert_eq!(count, 3);
        let json = std::fs::read_to_string(path.with_extension("markers.json")).unwrap();
        let sidecar: clipline_events::ClipMarkers = serde_json::from_str(&json).unwrap();
        let kinds: Vec<_> = sidecar
            .markers
            .iter()
            .map(|marker| marker.event.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![
                EventKind::ChampionKill,
                EventKind::ChampionAssist,
                EventKind::HeraldKill,
            ]
        );
        assert_eq!(sidecar.markers[0].event.actor, "Enemy Mid");
        assert!(!sidecar.markers[0].event.involves_local_player);
        assert!((sidecar.markers[0].t_s - 2.0).abs() < 1e-9);
    }

    #[test]
    fn cadenced_capture_takes_ownership_of_seed_payload_without_clone() {
        let payload = vec![7, 8, 9, 10];
        let original = payload.as_ptr();
        let seed = Frame {
            pts_s: 1.0,
            data: FrameData::Cpu(payload),
        };

        let cap = CadencedCapture::new(TimeoutSource, 60, seed);

        let Some(FrameData::Cpu(retained)) = cap.last_data.as_ref() else {
            panic!("CPU seed must remain a CPU frame");
        };
        assert_eq!(
            retained.as_ptr(),
            original,
            "constructor must move the seed allocation instead of cloning it"
        );
    }

    #[test]
    fn cadenced_capture_duplicates_seed_on_idle_timeout() {
        let seed = Frame {
            pts_s: 1.0,
            data: FrameData::Cpu(vec![7, 8, 9]),
        };
        let mut cap = CadencedCapture::new(TimeoutSource, 60, seed);

        let first = cap
            .next_frame()
            .expect("duplicate frame")
            .expect("capture still open");
        let second = cap
            .next_frame()
            .expect("duplicate frame")
            .expect("capture still open");

        assert!((first.pts_s - (1.0 + 1.0 / 60.0)).abs() < 1e-9);
        assert!((second.pts_s - (1.0 + 2.0 / 60.0)).abs() < 1e-9);
        assert!(matches!(first.data, FrameData::Cpu(ref data) if data == &[7, 8, 9]));
        assert!(matches!(second.data, FrameData::Cpu(ref data) if data == &[7, 8, 9]));
    }

    #[test]
    fn cadenced_capture_premature_timeouts_cannot_inflate_pts_past_wall_time() {
        let fps = 60;
        let frame_interval_s = 1.0 / fps as f64;
        let seed = Frame {
            pts_s: 1.0,
            data: FrameData::Cpu(vec![7, 8, 9]),
        };
        let seed_pts_s = seed.pts_s;
        let mut cap = CadencedCapture::new(
            PrematureTimeoutSource {
                delay: Duration::from_millis(1),
            },
            fps,
            seed,
        );
        let started = Instant::now();
        let mut last_pts_s = seed_pts_s;

        for _ in 0..120 {
            match cap.next_frame() {
                Ok(Some(frame)) => last_pts_s = frame.pts_s,
                Err(CaptureError::Timeout(_)) => {}
                other => panic!("unexpected capture result: {other:?}"),
            }
        }

        let wall_elapsed_s = started.elapsed().as_secs_f64();
        let pts_elapsed_s = last_pts_s - seed_pts_s;
        assert!(
            pts_elapsed_s <= wall_elapsed_s + frame_interval_s,
            "premature timeouts inflated PTS: pts={pts_elapsed_s:.6}s wall={wall_elapsed_s:.6}s"
        );
    }

    #[test]
    fn cadenced_capture_propagates_target_closure_instead_of_duplicating() {
        let seed = Frame {
            pts_s: 1.0,
            data: FrameData::Cpu(vec![7, 8, 9]),
        };
        let source = ScriptedTimedSource {
            outcomes: VecDeque::from([Ok(None)]),
            requested_timeouts: Vec::new(),
        };
        let mut capture = CadencedCapture::new(source, 60, seed);

        assert!(capture
            .next_frame()
            .expect("closed source is not an error")
            .is_none());
    }

    #[test]
    fn cadenced_capture_suppresses_stale_real_frame_after_timeout_duplicate() {
        let fps = 60;
        let interval_s = 1.0 / fps as f64;
        let seed = Frame {
            pts_s: 1.0,
            data: FrameData::Cpu(vec![1]),
        };
        let stale_pts_s = 1.0 + interval_s + 0.00005;
        let scheduled_pts_s = 1.0 + 2.0 * interval_s;
        let source = ScriptedTimedSource {
            outcomes: VecDeque::from([
                Err(CaptureError::Timeout(Duration::ZERO)),
                Ok(Some(Frame {
                    pts_s: stale_pts_s,
                    data: FrameData::Cpu(vec![2]),
                })),
                Ok(Some(Frame {
                    pts_s: scheduled_pts_s,
                    data: FrameData::Cpu(vec![3]),
                })),
            ]),
            requested_timeouts: Vec::new(),
        };
        let mut cap = CadencedCapture::new(source, fps, seed);

        let duplicate = cap.next_frame().unwrap().unwrap();
        let skipped = cap.next_frame();

        assert!((duplicate.pts_s - (1.0 + interval_s)).abs() < 1e-9);
        let skipped_for = match skipped {
            Err(CaptureError::Timeout(duration)) => duration,
            other => panic!("expected bounded stale-frame timeout, got {other:?}"),
        };
        assert_eq!(cap.inner.requested_timeouts.len(), 2);

        let next = cap.next_frame().unwrap().unwrap();

        assert!((next.pts_s - scheduled_pts_s).abs() < 1e-9);
        assert!(matches!(next.data, FrameData::Cpu(ref data) if data == &[3]));
        assert_eq!(cap.inner.requested_timeouts.len(), 3);
        assert!(cap.inner.requested_timeouts[0] <= cap.frame_interval);
        assert!(cap.inner.requested_timeouts[1] <= cap.frame_interval);
        assert!(cap.inner.requested_timeouts[2] <= skipped_for);
        let remaining_s = skipped_for.as_secs_f64();
        let pts_remaining_s = scheduled_pts_s - stale_pts_s;
        assert!(remaining_s <= pts_remaining_s + 1e-9);
        assert!(
            remaining_s >= pts_remaining_s - 0.005,
            "stale retry lost its deadline: remaining={remaining_s:.6}s expected={pts_remaining_s:.6}s"
        );
    }

    #[test]
    fn cadenced_capture_timeout_uses_latest_suppressed_frame_data() {
        let fps = 60;
        let interval_s = 1.0 / fps as f64;
        let seed = Frame {
            pts_s: 1.0,
            data: FrameData::Cpu(vec![1]),
        };
        let source = ScriptedTimedSource {
            outcomes: VecDeque::from([
                Err(CaptureError::Timeout(Duration::ZERO)),
                Ok(Some(Frame {
                    pts_s: 1.0 + interval_s + 0.00005,
                    data: FrameData::Cpu(vec![2]),
                })),
                Err(CaptureError::Timeout(Duration::ZERO)),
            ]),
            requested_timeouts: Vec::new(),
        };
        let mut cap = CadencedCapture::new(source, fps, seed);

        let first = cap.next_frame().unwrap().unwrap();
        let skipped = cap.next_frame();
        let second = cap.next_frame().unwrap().unwrap();

        assert!(matches!(first.data, FrameData::Cpu(ref data) if data == &[1]));
        assert!(matches!(skipped, Err(CaptureError::Timeout(_))));
        assert!((second.pts_s - (1.0 + 2.0 * interval_s)).abs() < 1e-9);
        assert!(matches!(second.data, FrameData::Cpu(ref data) if data == &[2]));
    }

    #[test]
    fn cadenced_capture_stale_retry_keeps_the_original_wait_deadline() {
        let fps = 60;
        let interval_s = 1.0 / fps as f64;
        let seed = Frame {
            pts_s: 1.0,
            data: FrameData::Cpu(vec![1]),
        };
        let source = DelayedFrameSource {
            frame: Some(Frame {
                pts_s: 1.0 + interval_s / 2.0,
                data: FrameData::Cpu(vec![2]),
            }),
            delay: Duration::from_millis(30),
            requested_timeouts: Vec::new(),
        };
        let mut cap = CadencedCapture::new(source, fps, seed);

        assert!(matches!(cap.next_frame(), Err(CaptureError::Timeout(_))));
        let duplicate = cap.next_frame().unwrap().unwrap();

        assert!((duplicate.pts_s - (1.0 + interval_s)).abs() < 1e-9);
        assert!(matches!(duplicate.data, FrameData::Cpu(ref data) if data == &[2]));
        assert!(
            cap.inner.requested_timeouts[1] <= Duration::from_millis(1),
            "retry restarted the cadence wait: {:?}",
            cap.inner.requested_timeouts
        );
    }

    #[test]
    fn cadenced_capture_counts_encoder_work_against_the_next_deadline() {
        let fps = 60;
        let interval_s = 1.0 / fps as f64;
        let seed = Frame {
            pts_s: 1.0,
            data: FrameData::Cpu(vec![1]),
        };
        let source = BlockingTimeoutSource {
            requested_timeouts: Vec::new(),
        };
        let mut cap = CadencedCapture::new(source, fps, seed);

        let first = cap.next_frame().unwrap().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let second = cap.next_frame().unwrap().unwrap();

        assert!(
            cap.inner.requested_timeouts[1] <= Duration::from_millis(1),
            "encoder work restarted the cadence wait: {:?}",
            cap.inner.requested_timeouts
        );
        assert!(
            second.pts_s - first.pts_s >= 3.0 * interval_s - 1e-9,
            "missed wall-clock slots were not reflected in PTS: first={}, second={}",
            first.pts_s,
            second.pts_s
        );
    }

    #[test]
    fn cadenced_capture_counts_delayed_real_frame_delivery() {
        let fps = 60;
        let interval_s = 1.0 / fps as f64;
        let seed = Frame {
            pts_s: 1.0,
            data: FrameData::Cpu(vec![1]),
        };
        let source = DelayedFrameSource {
            frame: Some(Frame {
                pts_s: 1.0 + interval_s,
                data: FrameData::Cpu(vec![2]),
            }),
            delay: Duration::from_millis(30),
            requested_timeouts: Vec::new(),
        };
        let mut cap = CadencedCapture::new(source, fps, seed);

        let real = cap.next_frame().unwrap().unwrap();
        let duplicate = cap.next_frame().unwrap().unwrap();

        assert!(
            cap.inner.requested_timeouts[1] <= Duration::from_millis(5),
            "late real-frame delivery restarted the cadence wait: {:?}",
            cap.inner.requested_timeouts
        );
        assert!((duplicate.pts_s - (real.pts_s + interval_s)).abs() < 1e-9);
    }

    #[test]
    fn manual_replay_save_does_not_shrink_after_previous_save() {
        let dir = TestDir::new("clipline-service", "manual-save-window");
        let mut rec = Recorder::new(
            MockCapture::new(120, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        );
        rec.run_to_end().unwrap();

        let first_path = dir.path().join("clip_first.mp4");
        let second_path = dir.path().join("clip_second.mp4");
        let (first_end, first_seconds) = save(&rec, &first_path, 2.0, None, None).unwrap();
        let (second_end, second_seconds) = save(&rec, &second_path, 2.0, None, None).unwrap();

        assert!((first_end - 4.0).abs() < 1e-6);
        assert!((second_end - 4.0).abs() < 1e-6);
        assert!((first_seconds - 2.0).abs() < 1e-6);
        assert!((second_seconds - 2.0).abs() < 1e-6);
        assert_eq!(
            std::fs::read(first_path.with_extension("clipline.json")).unwrap(),
            b"{}"
        );
        assert_eq!(
            std::fs::read(second_path.with_extension("clipline.json")).unwrap(),
            b"{}"
        );
    }

    #[test]
    fn failed_replay_save_reserves_attribution_and_removes_only_a_new_ownership_marker() {
        let dir = TestDir::new("clipline-service", "failed-save-marker-cleanup");
        let mut rec = Recorder::new(
            MockCapture::new(1, 30),
            MockEncoder::new(30, 30),
            usize::MAX,
        );
        rec.run_to_end().unwrap();
        let game = ActiveGame {
            identity: crate::game_identity::GameIdentity::custom("test-game"),
            name: "Test game".into(),
            exe_path: None,
            process_id: None,
        };

        let newly_marked = dir.path().join("new.mp4");
        std::fs::create_dir(&newly_marked).unwrap();
        assert!(save(&rec, &newly_marked, 1.0, Some(&game), None).is_err());
        assert!(!newly_marked.with_extension("clipline.json").exists());
        assert!(dir.path().join(SESSION_META_FILE).is_file());

        let already_marked = dir.path().join("existing.mp4");
        std::fs::create_dir(&already_marked).unwrap();
        ensure_clip_owned(&already_marked).unwrap();
        assert!(save(&rec, &already_marked, 1.0, None, None).is_err());
        assert_eq!(
            std::fs::read(already_marked.with_extension("clipline.json")).unwrap(),
            b"{}"
        );
    }

    #[test]
    fn clips_dir_uses_configured_root_when_creatable() {
        let dir = TestDir::new("clipline-service", "configured-root");
        let configured = dir.path().join("media");

        let (resolved, fell_back) =
            clips_dir_resolved(&configured, || panic!("must not fall back")).unwrap();

        assert!(!fell_back);
        assert_eq!(resolved, configured);
        assert!(configured.is_dir());
    }

    #[test]
    fn clips_dir_falls_back_when_configured_root_is_unusable() {
        let dir = TestDir::new("clipline-service", "unusable-root");
        // A directory cannot be created under a regular file, so this stands in
        // for an unreachable root (e.g. an unplugged drive).
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, b"x").unwrap();
        let unusable = blocker.join("clipline");
        let fallback = dir.path().join("fallback");

        let (resolved, fell_back) = clips_dir_resolved(&unusable, || fallback.clone()).unwrap();

        assert!(fell_back);
        assert_eq!(resolved, fallback);
        assert!(fallback.is_dir());
    }

    #[test]
    fn clips_dir_falls_back_when_existing_configured_root_is_not_writable() {
        let dir = TestDir::new("clipline-service", "unwritable-existing-root");
        let configured = dir.path().join("configured");
        let fallback = dir.path().join("fallback");
        std::fs::create_dir_all(&configured).unwrap();
        let mut probed = Vec::new();

        let (resolved, fell_back) = clips_dir_resolved_with_probe(
            &configured,
            || fallback.clone(),
            |candidate| {
                probed.push(candidate.to_path_buf());
                if candidate == configured {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "injected ACL denial",
                    ))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap();

        assert!(fell_back);
        assert_eq!(resolved, fallback);
        assert_eq!(probed, [configured, fallback]);
    }

    #[test]
    fn writable_directory_probe_leaves_no_probe_file() {
        let dir = TestDir::new("clipline-service", "writable-root-probe");
        let media = dir.path().join("media");
        std::fs::create_dir_all(&media).unwrap();
        std::fs::write(media.join("existing.txt"), b"keep").unwrap();

        probe_writable_directory(&media).unwrap();

        let mut names = std::fs::read_dir(&media)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, [std::ffi::OsString::from("existing.txt")]);
    }

    #[test]
    fn clips_dir_reports_configured_and_fallback_probe_failures() {
        let dir = TestDir::new("clipline-service", "double-probe-failure");
        let configured = dir.path().join("configured");
        let fallback = dir.path().join("fallback");

        let error = clips_dir_resolved_with_probe(
            &configured,
            || fallback.clone(),
            |_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected denial",
                ))
            },
        )
        .unwrap_err();

        assert!(error.contains(&configured.display().to_string()), "{error}");
        assert!(error.contains(&fallback.display().to_string()), "{error}");
    }

    #[test]
    fn temp_guard_flags_clips_inside_temp_root() {
        let dir = TestDir::new("clipline-service", "temp-guard");
        let temp_root = dir.path().join("temp");
        let inside = temp_root.join("Videos").join("Clipline");
        std::fs::create_dir_all(&inside).unwrap();

        assert!(is_within_temp(&inside, &temp_root));
    }

    #[test]
    fn temp_guard_allows_clips_outside_temp_root() {
        let dir = TestDir::new("clipline-service", "temp-guard-outside");
        let temp_root = dir.path().join("temp");
        let outside = dir.path().join("media").join("Clipline");
        std::fs::create_dir_all(&temp_root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        assert!(!is_within_temp(&outside, &temp_root));
    }

    #[test]
    fn full_session_temp_reservation_skips_existing_temp() {
        let dir = TestDir::new("clipline-service", "session-temp-reservation");
        let stamp = 1_725_000_000;
        let occupied_final = dir.path().join(format!("session_{stamp}.mp4"));
        let occupied_temp = occupied_final.with_extension("mp4.recording");
        let sentinel = b"active recorder bytes";
        std::fs::write(&occupied_temp, sentinel).unwrap();
        let occupied_suffix_final = dir.path().join(format!("session_{stamp}_1.mp4"));
        std::fs::write(&occupied_suffix_final, b"finished recording").unwrap();

        let (final_path, temp_path, _file) =
            reserve_full_session_path_at(dir.path(), "session", stamp).unwrap();

        assert_eq!(std::fs::read(&occupied_temp).unwrap(), sentinel);
        assert_ne!(temp_path, occupied_temp);
        assert_eq!(
            final_path,
            dir.path().join(format!("session_{stamp}_2.mp4"))
        );
        assert_eq!(
            temp_path,
            dir.path().join(format!("session_{stamp}_2.mp4.recording"))
        );
        assert_eq!(
            std::fs::read(final_path.with_extension("clipline.json")).unwrap(),
            b"{}"
        );
    }

    #[test]
    fn media_path_reservation_skips_orphaned_ownership_markers() {
        let dir = TestDir::new("clipline-service", "ownership-marker-reservation");
        let stamp = 1_725_000_002;
        let occupied = dir.path().join(format!("clip_{stamp}.mp4"));
        ensure_clip_owned(&occupied).unwrap();

        let replay = unique_media_path_at(dir.path(), "clip", stamp);
        let (session, temp, _file) =
            reserve_full_session_path_at(dir.path(), "session", stamp).unwrap();

        assert_eq!(replay, dir.path().join(format!("clip_{stamp}_1.mp4")));
        assert_eq!(session, dir.path().join(format!("session_{stamp}.mp4")));
        assert!(temp.exists());
        assert!(session.with_extension("clipline.json").is_file());
    }

    #[test]
    fn full_session_temp_reservation_retries_when_final_appears_during_reservation() {
        let dir = TestDir::new("clipline-service", "session-finalization-race");
        let stamp = 1_725_000_001;
        let raced_final = dir.path().join(format!("session_{stamp}.mp4"));
        let raced_temp = raced_final.with_extension("mp4.recording");
        let mut finalize_before_first_reservation = true;

        let (final_path, temp_path, _file) = reserve_full_session_path_at_with(
            dir.path(),
            "session",
            stamp,
            |candidate_final, candidate_temp| {
                if finalize_before_first_reservation {
                    finalize_before_first_reservation = false;
                    std::fs::write(candidate_final, b"old finalized recording").unwrap();
                }
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(candidate_temp)
            },
        )
        .unwrap();

        assert_eq!(
            std::fs::read(&raced_final).unwrap(),
            b"old finalized recording"
        );
        assert!(!raced_temp.exists());
        assert_eq!(
            final_path,
            dir.path().join(format!("session_{stamp}_1.mp4"))
        );
        assert_eq!(
            temp_path,
            dir.path().join(format!("session_{stamp}_1.mp4.recording"))
        );
    }

    #[test]
    fn finalized_session_rename_accepts_preexisting_final_file() {
        let dir = TestDir::new("clipline-service", "session-rename-recovered");
        let final_path = dir.path().join("session.mp4");
        std::fs::write(&final_path, b"mp4").unwrap();
        let recording = FullSessionRecording {
            final_path,
            temp_path: dir.path().join("session.mp4.recording"),
            wall_start_unix: 0,
        };
        let (tx, rx) = mpsc::channel();

        assert!(rename_finalized_session(&recording, &tx));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn finalized_session_rename_preserves_non_empty_temp_on_failure() {
        let dir = TestDir::new("clipline-service", "session-rename-preserve");
        let temp_path = dir.path().join("session.mp4.recording");
        std::fs::write(&temp_path, b"recoverable hybrid mp4").unwrap();
        let recording = FullSessionRecording {
            final_path: dir.path().join("missing-parent").join("session.mp4"),
            temp_path: temp_path.clone(),
            wall_start_unix: 0,
        };
        let (tx, rx) = mpsc::channel();

        assert!(!rename_finalized_session(&recording, &tx));
        assert_eq!(
            std::fs::read(&temp_path).unwrap(),
            b"recoverable hybrid mp4"
        );
        let Event::Error { message } = rx.try_recv().unwrap() else {
            panic!("expected recovery warning");
        };
        assert!(message.contains("recoverable"), "{message}");
        assert!(message.contains("session.mp4.recording"), "{message}");
    }

    #[test]
    fn failed_full_session_finish_preserves_non_empty_and_removes_empty_temp() {
        let dir = TestDir::new("clipline-service", "session-finish-preserve");
        let recoverable = dir.path().join("recoverable.mp4.recording");
        let empty = dir.path().join("empty.mp4.recording");
        std::fs::write(&recoverable, b"hybrid mp4").unwrap();
        std::fs::write(&empty, b"").unwrap();
        ensure_clip_owned(&recoverable).unwrap();
        ensure_clip_owned(&empty).unwrap();
        let (tx, rx) = mpsc::channel();

        handle_full_session_finish_error(&recoverable, &tx, "writer failed");
        handle_full_session_finish_error(&empty, &tx, "writer failed");

        assert!(recoverable.exists());
        assert!(clip_ownership_marker_path(&recoverable).unwrap().exists());
        assert!(!empty.exists());
        assert!(!clip_ownership_marker_path(&empty).unwrap().exists());
        let Event::Error { message } = rx.try_recv().unwrap() else {
            panic!("expected recovery warning");
        };
        assert!(message.contains("recoverable.mp4.recording"), "{message}");
    }

    #[test]
    fn finalized_session_rename_warns_when_temp_and_final_are_missing() {
        let dir = TestDir::new("clipline-service", "session-rename-missing");
        let recording = FullSessionRecording {
            final_path: dir.path().join("session.mp4"),
            temp_path: dir.path().join("session.mp4.recording"),
            wall_start_unix: 0,
        };
        let (tx, rx) = mpsc::channel();

        assert!(!rename_finalized_session(&recording, &tx));
        let Event::Error { message } = rx.try_recv().unwrap() else {
            panic!("expected warning");
        };
        assert!(message.contains("finalize full session"));
    }

    #[test]
    fn replay_cache_sweep_removes_stale_instance_and_preserves_live_quota() {
        let dir = TestDir::new("clipline-service", "replay-cache-sweep");
        let stale = dir.path().join("clipline-replay-cache-100-41-0");
        let live = dir.path().join("clipline-replay-cache-101-42-0");
        let unrelated = dir.path().join("somebody-elses-folder");
        for run in [&stale, &live, &unrelated] {
            std::fs::create_dir(run).unwrap();
        }
        write_replay_cache_owner(
            &stale,
            &ReplayCacheOwner {
                process_instance_id: "41:1000".into(),
                created_at_unix: 100,
            },
        )
        .unwrap();
        write_replay_cache_owner(
            &live,
            &ReplayCacheOwner {
                process_instance_id: "42:2000".into(),
                created_at_unix: 101,
            },
        )
        .unwrap();
        std::fs::write(stale.join("seg.bin"), vec![1; 17]).unwrap();
        std::fs::write(live.join("seg.bin"), vec![2; 23]).unwrap();
        std::fs::write(unrelated.join("keep.txt"), b"keep").unwrap();

        let preserved = sweep_replay_cache_runs(
            dir.path(),
            SystemTime::now() + Duration::from_secs(48 * 60 * 60),
            |pid| match pid {
                41 => Ok("41:9999".into()),
                42 => Ok("42:2000".into()),
                _ => Err("unexpected pid".into()),
            },
        )
        .unwrap();

        assert!(!stale.exists());
        assert!(live.exists());
        assert!(unrelated.exists());
        assert!(preserved >= 23);
    }

    #[test]
    fn replay_cache_sweep_preserves_ambiguous_fresh_run() {
        let dir = TestDir::new("clipline-service", "replay-cache-ambiguous");
        let run = dir.path().join("clipline-replay-cache-100-42-0");
        std::fs::create_dir(&run).unwrap();
        std::fs::write(run.join("seg.bin"), vec![3; 29]).unwrap();

        let preserved = sweep_replay_cache_runs(dir.path(), SystemTime::now(), |_| {
            Err("process cannot be queried".into())
        })
        .unwrap();

        assert!(run.exists());
        assert_eq!(preserved, 29);
    }

    #[test]
    fn replay_cache_sweep_removes_ambiguous_run_only_after_grace_period() {
        let dir = TestDir::new("clipline-service", "replay-cache-aged");
        let run = dir.path().join("clipline-replay-cache-100-42-0");
        std::fs::create_dir(&run).unwrap();
        std::fs::write(run.join("seg.bin"), vec![4; 31]).unwrap();

        let preserved = sweep_replay_cache_runs(
            dir.path(),
            SystemTime::now() + Duration::from_secs(25 * 60 * 60),
            |_| Err("process cannot be queried".into()),
        )
        .unwrap();

        assert!(!run.exists());
        assert_eq!(preserved, 0);
    }

    #[test]
    fn prepared_replay_storage_cleans_untransferred_run() {
        let dir = TestDir::new("clipline-service", "replay-cache-construction");
        let run = dir.path().join("clipline-replay-cache-100-42-0");
        std::fs::create_dir(&run).unwrap();
        std::fs::write(run.join(REPLAY_CACHE_OWNER_FILE), b"owned").unwrap();

        drop(PreparedReplayStorage::disk(run.clone(), 1024));

        assert!(!run.exists());
    }

    #[test]
    fn low_space_runtime_failure_always_finalizes_and_keeps_primary_error() {
        let finalized = std::cell::Cell::new(false);

        let message = finalize_runtime_failure("replay cache disk is low".into(), || {
            finalized.set(true);
            Some("finish: writer failed".into())
        });

        assert!(finalized.get());
        assert!(message.starts_with("replay cache disk is low"), "{message}");
        assert!(message.contains("finish: writer failed"), "{message}");
    }
