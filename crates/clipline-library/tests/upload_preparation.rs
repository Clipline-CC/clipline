use std::sync::Arc;

use clipline_events::{ClipAudioTrack, ClipMarker, ClipMarkers, EventKind, GameEvent, GameId};
use clipline_library::protocol::sha256_hex;
use clipline_library::{
    client_clip_id_for_payload, local_clip_id_for_source, ActiveFileRegistry,
    CloudAccountGeneration, CloudAccountKey, DurableUploadToken, LocalLibraryRepository,
    StandardRepositoryFileSystem, StandardUploadPreparation, UploadCancellation, UploadGeneration,
    UploadIntent, UploadPreparationPort,
};
use clipline_test_utils::TestDir;

const TWO_AUDIO_FIXTURE: &[u8] =
    include_bytes!("../../../fixtures/playback/hybrid-writer-h264-two-opus-5s.mp4");

struct PreparationFixture {
    _directory: TestDir,
    source: clipline_library::UploadSourceLease,
}

impl PreparationFixture {
    fn new(name: &str, markers: Option<ClipMarkers>) -> Self {
        let directory = TestDir::new("clipline-upload-preparation", name);
        let root = directory.path().join("media").join("match-1");
        std::fs::create_dir_all(&root).unwrap();
        let clip = root.join("session_1.mp4");
        std::fs::write(&clip, TWO_AUDIO_FIXTURE).unwrap();
        if let Some(markers) = markers {
            std::fs::write(
                clip.with_extension("markers.json"),
                serde_json::to_vec(&markers).unwrap(),
            )
            .unwrap();
        }
        let registry = ActiveFileRegistry::new();
        let repository = LocalLibraryRepository::with_seams(
            directory.path().join("media"),
            Arc::new(StandardRepositoryFileSystem),
            Arc::new(registry.clone()),
        )
        .unwrap();
        let validated = repository
            .validate_clip_path(&clip.display().to_string())
            .unwrap();
        let local_clip_id = local_clip_id_for_source(validated.file_identity());
        let token = DurableUploadToken {
            account_key: CloudAccountKey::new("account-a").unwrap(),
            account_generation: CloudAccountGeneration::new(3),
            upload_generation: UploadGeneration::new(7),
            local_clip_id,
            source_path: validated.comparison_identity().clone(),
        };
        let source = registry.acquire_upload(&validated, token).unwrap();
        Self {
            _directory: directory,
            source,
        }
    }

    fn upload_temps(&self) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(self.source.canonical_path().parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.contains(".clipline-upload-") && name.ends_with(".tmp")
                    })
            })
            .collect()
    }
}

fn two_audio_markers() -> ClipMarkers {
    ClipMarkers {
        recording_start_s: 0.0,
        duration_s: 5.0,
        player_summary: None,
        audio_tracks: vec![
            ClipAudioTrack {
                id: "output".into(),
                track_index: 0,
                label: "Desktop audio".into(),
                kind: Some("output".into()),
            },
            ClipAudioTrack {
                id: "microphone".into(),
                track_index: 1,
                label: "Microphone".into(),
                kind: Some("microphone".into()),
            },
        ],
        plays: Vec::new(),
        markers: vec![ClipMarker {
            t_s: 1.0,
            event: GameEvent {
                game_id: GameId::Valorant,
                kind: EventKind::ChampionKill,
                actor: "Player".into(),
                victim: Some("Opponent".into()),
                assisters: Vec::new(),
                subtype: None,
                game_time_s: 10.0,
                recording_offset_s: Some(1.0),
                importance: 7,
                involves_local_player: true,
            },
        }],
    }
}

fn intent(audio_track_ids: Option<Vec<&str>>) -> UploadIntent {
    UploadIntent {
        title: None,
        description: None,
        visibility: "private".into(),
        audio_track_ids: audio_track_ids.map(|ids| ids.into_iter().map(str::to_string).collect()),
        delete_local_after_upload: false,
    }
}

#[tokio::test]
async fn original_payload_preserves_source_and_builds_bounded_compatibility_metadata() {
    let fixture = PreparationFixture::new("original", Some(two_audio_markers()));
    let source_path = fixture.source.canonical_path().to_path_buf();
    std::fs::write(
        source_path.with_extension("clipline.json"),
        br#"{"title":"Ranked win","kind":"session"}"#,
    )
    .unwrap();
    std::fs::write(
        source_path.parent().unwrap().join("clipline-session.json"),
        br#"{"id":"osu","name":"osu!"}"#,
    )
    .unwrap();
    let preparation = StandardUploadPreparation;
    let mut request = intent(None);
    request.description = Some("  Useful context  ".into());
    request.visibility = "unlisted".into();

    let payload = preparation
        .prepare(&fixture.source, &request, &UploadCancellation::default())
        .await
        .unwrap();

    assert_eq!(payload.path(), source_path);
    assert!(fixture.upload_temps().is_empty());
    assert_eq!(payload.request().title, "Ranked win");
    assert_eq!(payload.description(), Some("Useful context"));
    assert_eq!(payload.request().description, None);
    assert_eq!(payload.request().source_type.as_deref(), Some("session"));
    assert_eq!(payload.request().game_id.as_deref(), Some("osu"));
    assert_eq!(payload.request().game_name.as_deref(), Some("osu!"));
    assert_eq!(payload.request().visibility.as_deref(), Some("unlisted"));
    assert_eq!(
        payload.request().file_size_bytes,
        TWO_AUDIO_FIXTURE.len() as u64
    );
    assert_eq!(
        payload.request().checksum_sha256,
        sha256_hex(TWO_AUDIO_FIXTURE)
    );
    let expected_duration_ms = clipline_mp4::movie_duration_s_file(&source_path)
        .unwrap()
        .map(|seconds| (seconds * 1000.0).round() as i64);
    assert_eq!(payload.request().duration_ms, expected_duration_ms);
    assert!(payload.request().recorded_at.is_some());
    assert_eq!(
        payload.client_clip_id(),
        &client_clip_id_for_payload(
            &fixture.source.token().local_clip_id,
            &payload.request().checksum_sha256,
        )
        .unwrap()
    );
    assert_eq!(
        payload.request().client_clip_id.as_deref(),
        Some(payload.client_clip_id().as_str())
    );
}

#[tokio::test]
async fn mute_single_and_mix_plans_use_owned_identity_stable_payloads_and_cleanup_on_drop() {
    let fixture = PreparationFixture::new("audio-plans", Some(two_audio_markers()));
    let preparation = StandardUploadPreparation;

    for (selection, expected_audio) in [
        (Some(Vec::<&str>::new()), 0),
        (Some(vec!["microphone"]), 1),
        (Some(vec!["output", "microphone"]), 1),
    ] {
        let payload = preparation
            .prepare(
                &fixture.source,
                &intent(selection),
                &UploadCancellation::default(),
            )
            .await
            .unwrap();
        assert_ne!(payload.path(), fixture.source.canonical_path());
        assert_eq!(fixture.upload_temps(), vec![payload.path().to_path_buf()]);
        assert_eq!(
            clipline_mp4::media_track_counts_file(payload.path())
                .unwrap()
                .audio,
            expected_audio
        );
        assert_eq!(
            payload.request().checksum_sha256,
            sha256_hex(std::fs::read(payload.path()).unwrap())
        );
        assert!(
            std::fs::read_dir(fixture.source.canonical_path().parent().unwrap())
                .unwrap()
                .filter_map(Result::ok)
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .contains(".clipline-tmp-"))
        );
        let temp = payload.path().to_path_buf();
        drop(payload);
        assert!(!temp.exists());
        assert!(fixture.upload_temps().is_empty());
    }
}

#[tokio::test]
async fn legacy_two_audio_tracks_are_inferred_for_exact_id_selection() {
    let fixture = PreparationFixture::new("legacy-audio", None);
    let payload = StandardUploadPreparation
        .prepare(
            &fixture.source,
            &intent(Some(vec!["audio:1"])),
            &UploadCancellation::default(),
        )
        .await
        .unwrap();

    assert_eq!(
        clipline_mp4::media_track_counts_file(payload.path())
            .unwrap()
            .audio,
        1
    );
}

#[tokio::test]
async fn unknown_duplicate_and_bounded_intent_errors_create_no_temporary_payload() {
    let fixture = PreparationFixture::new("intent-errors", Some(two_audio_markers()));
    let preparation = StandardUploadPreparation;
    let cases = [
        intent(Some(vec!["missing"])),
        intent(Some(vec!["output", "output"])),
        UploadIntent {
            title: Some("x".repeat(clipline_library::MAX_UPLOAD_TITLE_UTF16 + 1)),
            ..intent(None)
        },
        UploadIntent {
            description: Some("x".repeat(clipline_library::MAX_UPLOAD_DESCRIPTION_UTF16 + 1)),
            ..intent(None)
        },
        UploadIntent {
            visibility: "friends".into(),
            ..intent(None)
        },
    ];

    for request in cases {
        assert!(preparation
            .prepare(&fixture.source, &request, &UploadCancellation::default())
            .await
            .is_err());
        assert!(fixture.upload_temps().is_empty());
    }
}

#[tokio::test]
async fn canceled_preparation_is_fail_closed_and_marker_game_is_used_without_session_metadata() {
    let fixture = PreparationFixture::new("cancel-and-game", Some(two_audio_markers()));
    let cancellation = UploadCancellation::default();
    cancellation.cancel();
    let error = StandardUploadPreparation
        .prepare(
            &fixture.source,
            &intent(Some(vec!["output", "microphone"])),
            &cancellation,
        )
        .await
        .unwrap_err();
    assert!(error.is_canceled());
    assert!(fixture.upload_temps().is_empty());

    let payload = StandardUploadPreparation
        .prepare(
            &fixture.source,
            &intent(None),
            &UploadCancellation::default(),
        )
        .await
        .unwrap();
    assert_eq!(payload.request().game_id.as_deref(), Some("valorant"));
    assert_eq!(payload.request().game_name.as_deref(), Some("Valorant"));
}
