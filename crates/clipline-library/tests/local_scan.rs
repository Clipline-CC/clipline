use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use clipline_library::{
    ClipProjection, ClipProjectionOutput, ClipScanSource, CompactClipProjection,
    CompatibilityClipProjection, KnownGameIdentityResolver, LegacyAudioTrackProbe,
    LibraryDirectoryReader, LocalLibraryScanner, LOCAL_LIBRARY_TRUNCATED_WARNING,
    MAX_LOCAL_INDEX_ROWS,
};
use clipline_test_utils::TestDir;

use clipline_events::{ClipMarker, ClipMarkers, EventKind, GameEvent, GameId};

#[derive(Debug, PartialEq, Eq)]
struct Projected {
    path: String,
    title: Option<String>,
    kind: String,
    session: Option<String>,
    game: Option<(String, String)>,
}

struct SummaryProjection;

impl ClipProjection for SummaryProjection {
    type Output = Projected;

    fn project(&self, source: &ClipScanSource) -> ClipProjectionOutput<Self::Output> {
        ClipProjectionOutput::new(Projected {
            path: source.display_path().to_string(),
            title: source.title().map(ToOwned::to_owned),
            kind: source.kind().to_string(),
            session: source.session().map(ToOwned::to_owned),
            game: source
                .session_game()
                .map(|game| (game.id.clone(), game.name.clone())),
        })
    }
}

struct FixedAudioProbe(usize);

impl LegacyAudioTrackProbe for FixedAudioProbe {
    fn audio_track_count(&self, _clip_path: &Path) -> Result<usize, String> {
        Ok(self.0)
    }
}

#[derive(Debug)]
struct FailOneSessionReader {
    failed_session: &'static str,
}

impl LibraryDirectoryReader for FailOneSessionReader {
    fn read_dir(&self, path: &Path) -> io::Result<std::fs::ReadDir> {
        if path.file_name().and_then(|name| name.to_str()) == Some(self.failed_session) {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected session read failure",
            ))
        } else {
            std::fs::read_dir(path)
        }
    }
}

fn event(kind: EventKind) -> GameEvent {
    GameEvent {
        game_id: GameId::LeagueOfLegends,
        kind,
        actor: "Player".into(),
        victim: Some("Opponent".into()),
        assisters: Vec::new(),
        subtype: None,
        game_time_s: 12.0,
        recording_offset_s: Some(12.0),
        importance: 5,
        involves_local_player: true,
    }
}

fn touch(path: &Path, modified_seconds: u64) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, b"mp4").unwrap();
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(modified_seconds))
        .unwrap();
}

#[test]
fn scans_root_and_one_session_level_mp4_only_newest_first() {
    let dir = TestDir::new("clipline-library", "scan-levels");
    let root = dir.path().join("media");
    touch(&root.join("legacy.mp4"), 10);
    touch(&root.join("ignore.MP4"), 40);
    touch(&root.join("ignore.txt"), 50);
    std::fs::create_dir_all(root.join("directory.mp4")).unwrap();
    touch(&root.join("session-a").join("session_1.mp4"), 30);
    touch(
        &root.join("session-a").join("nested").join("too-deep.mp4"),
        60,
    );

    let scan = LocalLibraryScanner::open(&root)
        .unwrap()
        .scan(&SummaryProjection)
        .unwrap();

    assert_eq!(scan.clips.len(), 2);
    assert!(scan.clips[0].path.ends_with("session_1.mp4"));
    assert_eq!(scan.clips[0].session.as_deref(), Some("session-a"));
    assert!(scan.clips[1].path.ends_with("legacy.mp4"));
    assert_eq!(scan.clips[1].session, None);
    assert!(scan.warnings.is_empty(), "{:?}", scan.warnings);
    assert!(!scan.truncated);
}

#[test]
fn one_unreadable_session_warns_and_preserves_other_deterministic_results() {
    let dir = TestDir::new("clipline-library", "scan-partial-session");
    let root = dir.path().join("media");
    touch(&root.join("legacy.mp4"), 10);
    touch(&root.join("good-a").join("a.mp4"), 30);
    touch(&root.join("good-b").join("b.mp4"), 20);
    touch(&root.join("unreadable").join("hidden.mp4"), 40);
    let scanner = LocalLibraryScanner::open_with_directory_reader(
        &root,
        Arc::new(FailOneSessionReader {
            failed_session: "unreadable",
        }),
    )
    .unwrap();

    let first = scanner.scan(&SummaryProjection).unwrap();
    let second = scanner.scan(&SummaryProjection).unwrap();
    let names = |scan: &clipline_library::LocalScan<Projected>| {
        scan.clips
            .iter()
            .map(|clip| {
                Path::new(&clip.path)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>()
    };

    assert_eq!(names(&first), ["a.mp4", "b.mp4", "legacy.mp4"]);
    assert_eq!(names(&first), names(&second));
    assert!(first.clips.len() <= MAX_LOCAL_INDEX_ROWS);
    let expected_warning = "Skipped Library session \"unreadable\" because it could not be read: injected session read failure";
    assert_eq!(first.warnings, [expected_warning]);
    assert_eq!(first.warnings, second.warnings);
}

#[test]
fn equal_mtimes_have_a_stable_path_identity_tie_break() {
    let dir = TestDir::new("clipline-library", "scan-tie");
    let root = dir.path().join("media");
    touch(&root.join("z.mp4"), 10);
    touch(&root.join("a.mp4"), 10);
    touch(&root.join("m.mp4"), 10);

    let scanner = LocalLibraryScanner::open(&root).unwrap();
    let first = scanner.scan(&SummaryProjection).unwrap();
    let second = scanner.scan(&SummaryProjection).unwrap();
    let names = |scan: &clipline_library::LocalScan<Projected>| {
        scan.clips
            .iter()
            .map(|clip| {
                Path::new(&clip.path)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(names(&first), vec!["a.mp4", "m.mp4", "z.mp4"]);
    assert_eq!(names(&first), names(&second));
}

#[test]
fn metadata_and_session_game_are_bounded_scan_summaries() {
    let dir = TestDir::new("clipline-library", "scan-summary");
    let root = dir.path().join("media");
    let session = root.join("2026-08-02");
    let clip = session.join("session_1.mp4");
    touch(&clip, 10);
    std::fs::write(
        clip.with_extension("clipline.json"),
        br#"{"title":"  Ranked win  ","kind":"trim"}"#,
    )
    .unwrap();
    std::fs::write(
        session.join("clipline-session.json"),
        br#"{"id":"league_of_legends","name":"League of Legends"}"#,
    )
    .unwrap();

    let scan = LocalLibraryScanner::open(&root)
        .unwrap()
        .scan(&SummaryProjection)
        .unwrap();

    assert_eq!(scan.clips[0].title.as_deref(), Some("Ranked win"));
    assert_eq!(scan.clips[0].kind, "trim");
    assert_eq!(
        scan.clips[0].game,
        Some(("league_of_legends".into(), "League of Legends".into()))
    );
}

#[test]
fn compatibility_projection_preserves_exact_json_and_filtered_marker_duration() {
    let dir = TestDir::new("clipline-library", "scan-compatibility-json");
    let root = dir.path().join("media");
    let clip = root.join("session_1.mp4");
    touch(&clip, 10);
    let markers = ClipMarkers {
        recording_start_s: 0.0,
        duration_s: 42.5,
        player_summary: None,
        audio_tracks: Vec::new(),
        plays: Vec::new(),
        markers: vec![
            ClipMarker {
                t_s: 5.0,
                event: event(EventKind::ChampionKill),
            },
            ClipMarker {
                t_s: 6.0,
                event: event(EventKind::GameStart),
            },
        ],
    };
    std::fs::write(
        clip.with_extension("markers.json"),
        serde_json::to_vec(&markers).unwrap(),
    )
    .unwrap();
    let games = KnownGameIdentityResolver;
    let probe = FixedAudioProbe(0);

    let scan = LocalLibraryScanner::open(&root)
        .unwrap()
        .scan(&CompatibilityClipProjection::new(&probe, &games))
        .unwrap();

    assert_eq!(scan.clips[0].duration_s, Some(42.5));
    assert_eq!(scan.clips[0].markers.as_ref().unwrap().markers.len(), 1);
    assert_eq!(
        scan.clips[0].game.as_ref().map(|game| game.id.as_str()),
        Some("league_of_legends")
    );
    let json = serde_json::to_value(&scan).unwrap();
    let envelope = json.as_object().unwrap();
    assert_eq!(
        envelope.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["clips", "warnings"]
    );
    assert!(json.get("truncated").is_none());
    let row = json["clips"][0].as_object().unwrap();
    assert_eq!(
        row.keys().map(String::as_str).collect::<Vec<_>>(),
        vec![
            "duration_s",
            "game",
            "kind",
            "markers",
            "modified_unix",
            "name",
            "path",
            "session",
            "size_mb",
            "title",
        ]
    );
    assert!(json["clips"][0]["title"].is_null());
    assert!(json["clips"][0]["session"].is_null());
}

#[test]
fn compact_projection_keeps_only_aggregate_marker_card_data() {
    let dir = TestDir::new("clipline-library", "scan-compact");
    let root = dir.path().join("media");
    let clip = root.join("replay.mp4");
    touch(&clip, 10);
    let markers = ClipMarkers {
        recording_start_s: 0.0,
        duration_s: 12.0,
        player_summary: None,
        audio_tracks: Vec::new(),
        plays: Vec::new(),
        markers: vec![ClipMarker {
            t_s: 5.0,
            event: event(EventKind::ChampionKill),
        }],
    };
    std::fs::write(
        clip.with_extension("markers.json"),
        serde_json::to_vec(&markers).unwrap(),
    )
    .unwrap();
    let games = KnownGameIdentityResolver;
    let probe = FixedAudioProbe(0);

    let scan = LocalLibraryScanner::open(&root)
        .unwrap()
        .scan(&CompactClipProjection::new(&probe, &games))
        .unwrap();

    assert_eq!(scan.clips[0].marker_count, 1);
    assert_eq!(scan.clips[0].marker_summary.review_marker_count, 1);
    assert_eq!(scan.clips[0].marker_summary.plays.total, 0);
    assert!(scan.clips[0].marker_summary.search_text.len() <= 64 * 1024);
}

#[test]
fn compatibility_projection_infers_legacy_audio_without_inventing_duration() {
    let dir = TestDir::new("clipline-library", "scan-legacy-audio");
    let root = dir.path().join("media");
    let clip = root.join("legacy.mp4");
    touch(&clip, 10);
    let games = KnownGameIdentityResolver;
    let probe = FixedAudioProbe(2);

    let scan = LocalLibraryScanner::open(&root)
        .unwrap()
        .scan(&CompatibilityClipProjection::new(&probe, &games))
        .unwrap();

    let row = &scan.clips[0];
    assert_eq!(row.duration_s, None);
    let tracks = &row.markers.as_ref().unwrap().audio_tracks;
    assert_eq!(tracks.len(), 2);
    assert_eq!(tracks[0].id, "audio:0");
    assert_eq!(tracks[1].label, "Audio Track 2");
}

#[test]
fn corrupt_metadata_and_session_sidecars_warn_without_dropping_the_clip() {
    let dir = TestDir::new("clipline-library", "scan-corrupt-summary");
    let root = dir.path().join("media");
    let session = root.join("session-a");
    let clip = session.join("session_1.mp4");
    touch(&clip, 10);
    std::fs::write(clip.with_extension("clipline.json"), b"not json").unwrap();
    std::fs::write(session.join("clipline-session.json"), b"not json").unwrap();

    let scan = LocalLibraryScanner::open(&root)
        .unwrap()
        .scan(&SummaryProjection)
        .unwrap();

    assert_eq!(scan.clips.len(), 1);
    assert_eq!(scan.clips[0].kind, "session");
    assert_eq!(scan.warnings.len(), 2, "{:?}", scan.warnings);
    assert!(scan
        .warnings
        .iter()
        .any(|warning| warning.contains("clip metadata")));
    assert!(scan
        .warnings
        .iter()
        .any(|warning| warning.contains("session game")));
}

#[test]
fn shipping_scan_retains_only_the_deterministic_newest_ten_thousand() {
    let dir = TestDir::new("clipline-library", "scan-cap");
    let root = dir.path().join("media");
    std::fs::create_dir_all(&root).unwrap();
    for index in 0..=MAX_LOCAL_INDEX_ROWS {
        touch(
            &root.join(format!("clip-{index:05}.mp4")),
            u64::try_from(index).unwrap() + 1,
        );
    }

    let scan = LocalLibraryScanner::open(&root)
        .unwrap()
        .scan(&SummaryProjection)
        .unwrap();

    assert_eq!(scan.clips.len(), MAX_LOCAL_INDEX_ROWS);
    assert!(scan.clips[0].path.ends_with("clip-10000.mp4"));
    assert!(scan.clips.last().unwrap().path.ends_with("clip-00001.mp4"));
    assert!(!scan
        .clips
        .iter()
        .any(|clip| clip.path.ends_with("clip-00000.mp4")));
    assert_eq!(scan.warnings, vec![LOCAL_LIBRARY_TRUNCATED_WARNING]);
    assert!(scan.truncated);
}

#[test]
fn missing_root_is_a_fatal_scan_error() {
    let dir = TestDir::new("clipline-library", "scan-root-fatal");
    let missing = dir.path().join("missing");
    assert!(LocalLibraryScanner::open(&missing).is_err());
}

#[cfg(unix)]
#[test]
fn session_symlink_escape_is_rejected() {
    use std::os::unix::fs::symlink;

    let dir = TestDir::new("clipline-library", "scan-symlink");
    let root = dir.path().join("media");
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&root).unwrap();
    touch(&outside.join("escape.mp4"), 10);
    symlink(&outside, root.join("escaped-session")).unwrap();

    let scan = LocalLibraryScanner::open(&root)
        .unwrap()
        .scan(&SummaryProjection)
        .unwrap();
    assert!(scan.clips.is_empty());
    assert!(scan
        .warnings
        .iter()
        .any(|warning| warning.contains("link or reparse")));
}
