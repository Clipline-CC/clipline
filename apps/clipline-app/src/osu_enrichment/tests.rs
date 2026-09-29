use super::*;
use clipline_test_utils::TestDir;

fn write_session_game(dir: &std::path::Path, id: &str, name: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("clipline-session.json"),
        format!(r#"{{"id":"{id}","name":"{name}"}}"#),
    )
    .unwrap();
}

#[test]
fn writes_pending_record_for_osu_full_session() {
    let dir = TestDir::new("clipline-osu", "pending-write");
    let session = dir.path().join("2026-06-30");
    write_session_game(&session, crate::game_plugins::OSU_ID, "osu!");
    let clip = session.join("session_123.mp4");
    std::fs::write(&clip, b"mp4").unwrap();

    let written = write_pending_for_saved_clip(&OsuSavedClip {
        path: clip.clone(),
        seconds: 120.0,
        full_session: true,
        recording_start_unix: Some(1_820_000_000),
        recording_end_unix: Some(1_820_000_120),
        title_events: vec![OsuTitleEvent {
            unix_s: 1_820_000_030,
            title: "osu! - xi - Blue Zenith [FOUR DIMENSIONS]".into(),
        }],
    })
    .unwrap()
    .expect("pending file");

    assert_eq!(written, pending_path(&clip));
    let pending: OsuPendingEnrichment =
        serde_json::from_str(&std::fs::read_to_string(written).unwrap()).unwrap();
    assert_eq!(pending.schema_version, 1);
    assert_eq!(pending.clip_path, clip.display().to_string());
    assert_eq!(pending.recording_start_unix, 1_820_000_000);
    assert_eq!(pending.recording_end_unix, 1_820_000_120);
    assert_eq!(pending.clip_duration_s, 120.0);
    assert_eq!(pending.status, OsuEnrichmentStatus::Pending);
    assert!(!pending.pagination_ceiling_reached);
    assert_eq!(
        pending.title_events,
        vec![OsuTitleEvent {
            unix_s: 1_820_000_030,
            title: "osu! - xi - Blue Zenith [FOUR DIMENSIONS]".into(),
        }]
    );
}

#[test]
fn skips_non_osu_or_non_full_session_saves() {
    let dir = TestDir::new("clipline-osu", "pending-skip");
    let league = dir.path().join("league");
    write_session_game(&league, crate::game_plugins::LEAGUE_OF_LEGENDS_ID, "League");
    let league_clip = league.join("session.mp4");
    std::fs::write(&league_clip, b"mp4").unwrap();

    assert!(write_pending_for_saved_clip(&OsuSavedClip {
        path: league_clip.clone(),
        seconds: 60.0,
        full_session: true,
        recording_start_unix: Some(10),
        recording_end_unix: Some(70),
        title_events: Vec::new(),
    })
    .unwrap()
    .is_none());

    let osu = dir.path().join("osu");
    write_session_game(&osu, crate::game_plugins::OSU_ID, "osu!");
    let replay_clip = osu.join("clip.mp4");
    std::fs::write(&replay_clip, b"mp4").unwrap();
    assert!(write_pending_for_saved_clip(&OsuSavedClip {
        path: replay_clip.clone(),
        seconds: 15.0,
        full_session: false,
        recording_start_unix: Some(20),
        recording_end_unix: Some(35),
        title_events: Vec::new(),
    })
    .unwrap()
    .is_none());
    assert!(!pending_path(&replay_clip).exists());
}

#[test]
fn discovers_pending_records_under_media_root_for_retry() {
    let dir = TestDir::new("clipline-osu", "pending-discover");
    let session = dir.path().join("session");
    write_session_game(&session, crate::game_plugins::OSU_ID, "osu!");
    let clip = session.join("session.mp4");
    std::fs::write(&clip, b"mp4").unwrap();
    write_pending_for_saved_clip(&OsuSavedClip {
        path: clip.clone(),
        seconds: 30.0,
        full_session: true,
        recording_start_unix: Some(100),
        recording_end_unix: Some(130),
        title_events: Vec::new(),
    })
    .unwrap();

    let pending = discover_pending(dir.path()).unwrap();

    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].clip_path(), clip.canonicalize().unwrap());
    assert_eq!(
        pending[0].sidecar_path(),
        pending_path(&clip).canonicalize().unwrap()
    );
    assert_eq!(pending[0].record().clip_path, clip.display().to_string());
}

#[test]
fn discovery_rejects_a_serialized_clip_path_outside_the_media_root() {
    let dir = TestDir::new("clipline-osu", "pending-path-escape");
    let media_root = dir.path().join("media");
    let session = media_root.join("session");
    write_session_game(&session, crate::game_plugins::OSU_ID, "osu!");
    let expected_clip = session.join("session.mp4");
    std::fs::write(&expected_clip, b"mp4").unwrap();

    let outside_clip = dir.path().join("outside.mp4");
    std::fs::write(&outside_clip, b"outside").unwrap();
    let record = OsuPendingEnrichment {
        schema_version: 1,
        clip_path: outside_clip.display().to_string(),
        recording_start_unix: 100,
        recording_end_unix: 130,
        clip_duration_s: 30.0,
        status: OsuEnrichmentStatus::Pending,
        attempts: 0,
        pagination_ceiling_reached: false,
        title_events: Vec::new(),
        message: None,
    };
    std::fs::write(
        pending_path(&expected_clip),
        serde_json::to_vec_pretty(&record).unwrap(),
    )
    .unwrap();

    let pending = discover_pending(&media_root).unwrap();

    assert!(pending.is_empty());
    assert!(!pending_path(&expected_clip).exists());
    assert!(!outside_clip.with_extension("markers.json").exists());
    assert!(!pending_path(&outside_clip).exists());
}

#[test]
fn discovery_requires_the_mp4_named_by_the_sidecar() {
    let dir = TestDir::new("clipline-osu", "pending-missing-clip");
    std::fs::create_dir_all(dir.path()).unwrap();
    let missing_clip = dir.path().join("missing.mp4");
    let record = OsuPendingEnrichment {
        schema_version: 1,
        clip_path: missing_clip.display().to_string(),
        recording_start_unix: 100,
        recording_end_unix: 130,
        clip_duration_s: 30.0,
        status: OsuEnrichmentStatus::Pending,
        attempts: 0,
        pagination_ceiling_reached: false,
        title_events: Vec::new(),
        message: None,
    };
    std::fs::write(
        pending_path(&missing_clip),
        serde_json::to_vec_pretty(&record).unwrap(),
    )
    .unwrap();

    let pending = discover_pending(dir.path()).unwrap();

    assert!(pending.is_empty());
    assert!(!pending_path(&missing_clip).exists());
}

#[test]
fn discovery_does_not_follow_a_linked_session_directory() {
    let dir = TestDir::new("clipline-osu", "pending-linked-session");
    let media_root = dir.path().join("media");
    let outside = dir.path().join("outside-session");
    std::fs::create_dir_all(&media_root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let clip = outside.join("session.mp4");
    std::fs::write(&clip, b"mp4").unwrap();
    let record = OsuPendingEnrichment {
        schema_version: 1,
        clip_path: clip.display().to_string(),
        recording_start_unix: 100,
        recording_end_unix: 130,
        clip_duration_s: 30.0,
        status: OsuEnrichmentStatus::Pending,
        attempts: 0,
        pagination_ceiling_reached: false,
        title_events: Vec::new(),
        message: None,
    };
    std::fs::write(
        pending_path(&clip),
        serde_json::to_vec_pretty(&record).unwrap(),
    )
    .unwrap();
    let linked = media_root.join("linked-session");
    #[cfg(windows)]
    if std::os::windows::fs::symlink_dir(&outside, &linked).is_err() {
        return;
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, &linked).unwrap();

    let pending = discover_pending(&media_root).unwrap();

    assert!(pending.is_empty());
}

#[test]
fn retry_writes_only_the_path_bound_to_the_discovered_job() {
    let dir = TestDir::new("clipline-osu", "pending-bound-retry");
    let safe_clip = dir.path().join("safe.mp4");
    std::fs::write(&safe_clip, b"mp4").unwrap();
    let safe_sidecar = pending_path(&safe_clip);
    let outside_clip = dir.path().join("outside").join("victim.mp4");
    std::fs::create_dir_all(outside_clip.parent().unwrap()).unwrap();
    std::fs::write(&outside_clip, b"victim").unwrap();
    let record = OsuPendingEnrichment {
        schema_version: 1,
        clip_path: outside_clip.display().to_string(),
        recording_start_unix: 100,
        recording_end_unix: 130,
        clip_duration_s: 30.0,
        status: OsuEnrichmentStatus::Pending,
        attempts: 0,
        pagination_ceiling_reached: false,
        title_events: Vec::new(),
        message: None,
    };
    std::fs::write(&safe_sidecar, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
    let job = DiscoveredPendingEnrichment {
        record,
        clip_path: safe_clip.clone(),
        sidecar_path: safe_sidecar.clone(),
    };

    mark_pending_retry(&job, "retry safely").unwrap();

    let safe: OsuPendingEnrichment =
        serde_json::from_slice(&std::fs::read(&safe_sidecar).unwrap()).unwrap();
    assert_eq!(safe.attempts, 1);
    assert!(!pending_path(&outside_clip).exists());
}

#[test]
fn mixed_malformed_and_valid_records_quarantine_only_the_bad_job() {
    let dir = TestDir::new("clipline-osu", "pending-quarantine");
    let valid_clip = dir.path().join("valid.mp4");
    std::fs::write(&valid_clip, b"mp4").unwrap();
    let valid = OsuPendingEnrichment {
        schema_version: 1,
        clip_path: valid_clip.display().to_string(),
        recording_start_unix: 100,
        recording_end_unix: 130,
        clip_duration_s: 30.0,
        status: OsuEnrichmentStatus::Pending,
        attempts: 0,
        pagination_ceiling_reached: false,
        title_events: Vec::new(),
        message: None,
    };
    std::fs::write(
        pending_path(&valid_clip),
        serde_json::to_vec_pretty(&valid).unwrap(),
    )
    .unwrap();
    let bad = dir.path().join("bad.osu-enrichment.json");
    std::fs::write(&bad, b"{ truncated").unwrap();

    let pending = discover_pending(dir.path()).unwrap();

    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].clip_path(), valid_clip.canonicalize().unwrap());
    assert!(!bad.exists());
    assert!(std::fs::read_dir(dir.path()).unwrap().any(|entry| {
        entry
            .ok()
            .and_then(|entry| entry.file_name().to_str().map(str::to_string))
            .is_some_and(|name| name.starts_with("bad.osu-enrichment.json.invalid."))
    }));
    assert_eq!(discover_pending(dir.path()).unwrap().len(), 1);
}

#[test]
fn retry_schedule_honors_status_attempts_and_caps() {
    assert!(retry_is_due(OsuEnrichmentStatus::Pending, 0, 1_000, 1_000));
    assert!(!retry_is_due(OsuEnrichmentStatus::Pending, 1, 1_000, 1_059));
    assert!(retry_is_due(OsuEnrichmentStatus::Pending, 1, 1_000, 1_060));
    assert_eq!(
        retry_delay(OsuEnrichmentStatus::Pending, u32::MAX),
        Duration::from_secs(6 * 60 * 60)
    );
    assert!(!retry_is_due(
        OsuEnrichmentStatus::Failed,
        1,
        1_000,
        1_000 + 6 * 60 * 60 - 1
    ));
    assert!(retry_is_due(
        OsuEnrichmentStatus::Failed,
        1,
        1_000,
        1_000 + 6 * 60 * 60
    ));
    assert_eq!(
        retry_delay(OsuEnrichmentStatus::Failed, u32::MAX),
        Duration::from_secs(24 * 60 * 60)
    );
    assert_eq!(retry_delay(OsuEnrichmentStatus::Complete, 1), Duration::MAX);
}

#[test]
fn atomic_json_replacement_leaves_one_complete_file_and_no_owned_temp() {
    let dir = TestDir::new("clipline-osu", "pending-atomic-write");
    let path = dir.path().join("clip.osu-enrichment.json");
    std::fs::write(&path, b"old").unwrap();
    let record = OsuPendingEnrichment {
        schema_version: 1,
        clip_path: dir.path().join("clip.mp4").display().to_string(),
        recording_start_unix: 100,
        recording_end_unix: 130,
        clip_duration_s: 30.0,
        status: OsuEnrichmentStatus::Pending,
        attempts: 2,
        pagination_ceiling_reached: false,
        title_events: Vec::new(),
        message: Some("complete replacement".into()),
    };

    write_json_atomically(&path, &record, "test pending JSON").unwrap();

    let stored: OsuPendingEnrichment =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(stored, record);
    assert!(!std::fs::read_dir(dir.path()).unwrap().any(|entry| {
        entry
            .ok()
            .and_then(|entry| entry.file_name().to_str().map(str::to_string))
            .is_some_and(|name| name.contains(".clipline-osu-tmp."))
    }));
}

#[test]
fn write_pending_creates_title_only_play_sidecar_before_api_enrichment() {
    let dir = TestDir::new("clipline-osu", "title-only-sidecar");
    let session = dir.path().join("2026-07-01");
    write_session_game(&session, crate::game_plugins::OSU_ID, "osu!");
    let clip = session.join("session_123.mp4");
    std::fs::write(&clip, b"mp4").unwrap();

    write_pending_for_saved_clip(&OsuSavedClip {
        path: clip.clone(),
        seconds: 120.0,
        full_session: true,
        recording_start_unix: Some(1_820_000_000),
        recording_end_unix: Some(1_820_000_120),
        title_events: vec![
            OsuTitleEvent {
                unix_s: 1_820_000_004,
                title: "osu!".into(),
            },
            OsuTitleEvent {
                unix_s: 1_820_000_010,
                title: "osu! - xi - Blue Zenith [FOUR DIMENSIONS]".into(),
            },
            OsuTitleEvent {
                unix_s: 1_820_000_042,
                title: "osu!".into(),
            },
            OsuTitleEvent {
                unix_s: 1_820_000_050,
                title: "osu! - Camellia - Exit This Earth's Atomosphere [Extra]".into(),
            },
            OsuTitleEvent {
                unix_s: 1_820_000_090,
                title: "osu!".into(),
            },
        ],
    })
    .unwrap();

    let markers: ClipMarkers = serde_json::from_str(
        &std::fs::read_to_string(clip.with_extension("markers.json")).unwrap(),
    )
    .unwrap();

    assert_eq!(markers.plays.len(), 2);
    assert_eq!(markers.plays[0].source, "osu_title");
    assert_eq!(markers.plays[0].external_id, "osu-title:1820000010");
    assert_eq!(markers.plays[0].artist, "xi");
    assert_eq!(markers.plays[0].title, "Blue Zenith");
    assert_eq!(markers.plays[0].difficulty, "FOUR DIMENSIONS");
    assert_eq!(markers.plays[0].rank, None);
    assert_eq!(markers.plays[0].pp, None);
    assert_eq!(markers.plays[0].t_start_s, 10.0);
    assert_eq!(markers.plays[0].t_end_s, Some(42.0));
    assert_eq!(markers.plays[1].artist, "Camellia");
    assert_eq!(markers.plays[1].title, "Exit This Earth's Atomosphere");
    assert_eq!(markers.plays[1].difficulty, "Extra");
    assert_eq!(markers.plays[1].t_start_s, 50.0);
    assert_eq!(markers.plays[1].t_end_s, Some(90.0));
    assert!(pending_path(&clip).exists());
}

#[test]
fn empty_api_enrichment_preserves_title_fallback_and_pending_retry() {
    let dir = TestDir::new("clipline-osu", "empty-api-keeps-fallback");
    let session = dir.path().join("2026-07-01");
    write_session_game(&session, crate::game_plugins::OSU_ID, "osu!");
    let clip = session.join("session_123.mp4");
    std::fs::write(&clip, b"mp4").unwrap();

    let pending_path = write_pending_for_saved_clip(&OsuSavedClip {
        path: clip.clone(),
        seconds: 60.0,
        full_session: true,
        recording_start_unix: Some(1_820_000_000),
        recording_end_unix: Some(1_820_000_060),
        title_events: vec![
            OsuTitleEvent {
                unix_s: 1_820_000_005,
                title: "osu! - xi - Blue Zenith [FOUR DIMENSIONS]".into(),
            },
            OsuTitleEvent {
                unix_s: 1_820_000_045,
                title: "osu!".into(),
            },
        ],
    })
    .unwrap()
    .expect("pending file");
    let pending = discover_pending(dir.path()).unwrap();
    let pending = pending.first().expect("discovered pending job");

    let mapped = apply_scores_to_pending(pending, &[], false).unwrap();

    assert!(mapped.plays.is_empty());
    let markers: ClipMarkers = serde_json::from_str(
        &std::fs::read_to_string(clip.with_extension("markers.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(markers.plays.len(), 1);
    assert_eq!(markers.plays[0].source, "osu_title");
    assert_eq!(markers.plays[0].title, "Blue Zenith");
    assert!(pending_path.exists());
    let retried: OsuPendingEnrichment =
        serde_json::from_str(&std::fs::read_to_string(&pending_path).unwrap()).unwrap();
    assert_eq!(retried.status, OsuEnrichmentStatus::Pending);
    assert_eq!(retried.attempts, 1);
    assert!(retried
        .message
        .as_deref()
        .unwrap_or_default()
        .contains("No osu! API plays matched"));
}

#[test]
fn maps_proxy_scores_to_clip_plays_with_derived_start_clamp() {
    let pending = OsuPendingEnrichment {
        schema_version: 1,
        clip_path: "session.mp4".into(),
        recording_start_unix: 1_000,
        recording_end_unix: 1_300,
        clip_duration_s: 300.0,
        status: OsuEnrichmentStatus::Pending,
        attempts: 0,
        pagination_ceiling_reached: false,
        title_events: Vec::new(),
        message: None,
    };
    let scores = vec![
        proxy_score("known", Some(1_010), 1_070, Some(100.0), true, &[]),
        proxy_score("failed-derived", None, 1_080, Some(120.0), false, &[]),
        proxy_score("dt-derived", None, 1_160, Some(90.0), true, &["DT"]),
        proxy_score("known", Some(1_200), 1_240, Some(40.0), true, &[]),
    ];

    let mapped = map_proxy_scores_to_clip_plays(&pending, &scores, false);

    assert!(!mapped.pagination_ceiling_reached);
    assert_eq!(mapped.plays.len(), 3);
    assert_eq!(mapped.plays[0].external_id, "known");
    assert_eq!(mapped.plays[0].t_start_s, 10.0);
    assert_eq!(mapped.plays[0].t_end_s, Some(71.0));
    assert!(!mapped.plays[0].derived_start);
    assert_eq!(mapped.plays[1].external_id, "failed-derived");
    assert_eq!(mapped.plays[1].t_start_s, 80.0);
    assert_eq!(mapped.plays[1].t_end_s, None);
    assert!(mapped.plays[1].derived_start);
    assert_eq!(mapped.plays[2].external_id, "dt-derived");
    assert!((mapped.plays[2].t_start_s - 100.0).abs() < 1e-6);
    assert_eq!(mapped.plays[2].t_end_s, Some(161.0));
    assert!(mapped.plays[2].derived_start);
}

#[test]
fn maps_proxy_scores_with_tolerance_and_point_fallback() {
    let pending = OsuPendingEnrichment {
        schema_version: 1,
        clip_path: "session.mp4".into(),
        recording_start_unix: 1_000,
        recording_end_unix: 1_100,
        clip_duration_s: 100.0,
        status: OsuEnrichmentStatus::Pending,
        attempts: 0,
        pagination_ceiling_reached: false,
        title_events: Vec::new(),
        message: None,
    };
    let scores = vec![
        proxy_score("near-end", None, 1_103, None, false, &[]),
        proxy_score("clock-skew-end", None, 1_112, None, false, &[]),
        proxy_score("too-late", None, 1_120, None, false, &[]),
    ];

    let mapped = map_proxy_scores_to_clip_plays(&pending, &scores, true);

    assert!(mapped.pagination_ceiling_reached);
    assert_eq!(mapped.plays.len(), 2);
    assert_eq!(mapped.plays[0].external_id, "near-end");
    assert_eq!(mapped.plays[0].t_start_s, 100.0);
    assert_eq!(mapped.plays[0].t_end_s, None);
    assert!(mapped.plays[0].derived_start);
    assert_eq!(mapped.plays[1].external_id, "clock-skew-end");
    assert_eq!(mapped.plays[1].t_start_s, 100.0);
    assert_eq!(mapped.plays[1].t_end_s, None);
    assert!(mapped.plays[1].derived_start);
}

#[test]
fn failed_scores_without_started_at_map_to_end_marker() {
    let pending = OsuPendingEnrichment {
        schema_version: 1,
        clip_path: "session.mp4".into(),
        recording_start_unix: 1_000,
        recording_end_unix: 1_100,
        clip_duration_s: 100.0,
        status: OsuEnrichmentStatus::Pending,
        attempts: 0,
        pagination_ceiling_reached: false,
        title_events: Vec::new(),
        message: None,
    };
    let scores = vec![proxy_score(
        "failed-derived",
        None,
        1_047,
        Some(120.0),
        false,
        &[],
    )];

    let mapped = map_proxy_scores_to_clip_plays(&pending, &scores, false);

    assert_eq!(mapped.plays.len(), 1);
    assert_eq!(mapped.plays[0].t_start_s, 47.0);
    assert_eq!(mapped.plays[0].t_end_s, None);
    assert!(mapped.plays[0].derived_start);
}

#[test]
fn passed_scores_keep_results_screen_in_play_block() {
    let pending = OsuPendingEnrichment {
        schema_version: 1,
        clip_path: "session.mp4".into(),
        recording_start_unix: 1_000,
        recording_end_unix: 1_110,
        clip_duration_s: 103.849,
        status: OsuEnrichmentStatus::Pending,
        attempts: 0,
        pagination_ceiling_reached: false,
        title_events: Vec::new(),
        message: None,
    };
    let scores = vec![proxy_score(
        "passed-derived",
        None,
        1_097,
        Some(43.0),
        true,
        &[],
    )];

    let mapped = map_proxy_scores_to_clip_plays(&pending, &scores, false);

    assert_eq!(mapped.plays.len(), 1);
    assert_eq!(mapped.plays[0].t_start_s, 54.0);
    assert_eq!(mapped.plays[0].t_end_s, Some(98.0));
    assert!(mapped.plays[0].derived_start);
}

#[test]
fn missing_started_at_prefers_matching_window_title_event() {
    let pending: OsuPendingEnrichment = serde_json::from_value(serde_json::json!({
        "schema_version": 1,
        "clip_path": "session.mp4",
        "recording_start_unix": 1_000,
        "recording_end_unix": 1_110,
        "clip_duration_s": 110.0,
        "status": "pending",
        "attempts": 0,
        "title_events": [
            {
                "unix_s": 1_020,
                "title": "osu! - xi - Blue Zenith [FOUR DIMENSIONS]"
            }
        ]
    }))
    .unwrap();
    let scores = vec![proxy_score(
        "passed-title-derived",
        None,
        1_080,
        Some(120.0),
        true,
        &[],
    )];

    let mapped = map_proxy_scores_to_clip_plays(&pending, &scores, false);

    assert_eq!(mapped.plays.len(), 1);
    assert_eq!(mapped.plays[0].t_start_s, 20.0);
    assert_eq!(mapped.plays[0].t_end_s, Some(81.0));
    assert!(mapped.plays[0].derived_start);
}

#[test]
fn failed_scores_with_matching_window_title_event_keep_interval() {
    let pending: OsuPendingEnrichment = serde_json::from_value(serde_json::json!({
        "schema_version": 1,
        "clip_path": "session.mp4",
        "recording_start_unix": 1_000,
        "recording_end_unix": 1_110,
        "clip_duration_s": 110.0,
        "status": "pending",
        "attempts": 0,
        "title_events": [
            {
                "unix_s": 1_012,
                "title": "osu! - xi - Blue Zenith [FOUR DIMENSIONS]"
            }
        ]
    }))
    .unwrap();
    let scores = vec![proxy_score(
        "failed-title-derived",
        None,
        1_047,
        Some(120.0),
        false,
        &[],
    )];

    let mapped = map_proxy_scores_to_clip_plays(&pending, &scores, false);

    assert_eq!(mapped.plays.len(), 1);
    assert_eq!(mapped.plays[0].t_start_s, 12.0);
    assert_eq!(mapped.plays[0].t_end_s, Some(47.0));
    assert!(mapped.plays[0].derived_start);
}

fn proxy_score(
    id: &str,
    started_at_unix: Option<i64>,
    ended_at_unix: i64,
    beatmap_total_length_s: Option<f64>,
    passed: bool,
    mods: &[&str],
) -> OsuProxyScore {
    OsuProxyScore {
        id: id.into(),
        url: Some(format!("https://osu.ppy.sh/scores/{id}")),
        beatmap_id: Some(123),
        beatmapset_id: Some(456),
        cover_url: None,
        title: "Blue Zenith".into(),
        artist: "xi".into(),
        difficulty: "FOUR DIMENSIONS".into(),
        mapper: Some("Asphyxia".into()),
        star_rating: None,
        mods: mods.iter().map(|value| value.to_string()).collect(),
        rank: Some(if passed { "S" } else { "F" }.into()),
        passed,
        accuracy: Some(0.9912),
        max_combo: Some(777),
        total_score: Some(1_234_567),
        pp: if passed { Some(321.4) } else { None },
        started_at_unix,
        ended_at_unix,
        beatmap_total_length_s,
    }
}
