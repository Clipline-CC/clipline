use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use clipline_library::{
    load_clip_detail, load_marker_sidecar_with_probe, marker_sidecar_path, parse_marker_sidecar,
    parse_marker_sidecar_preserving_all, read_marker_sidecar, ClipDetailRequest, ClipPathIdentity,
    ForegroundGeneration, LegacyAudioTrackProbe, LocalSidecarError, RequestGeneration,
    WindowAttachmentGeneration, WindowWorkToken, MAX_CATALOG_STRING_BYTES,
    MAX_CLIP_DETAIL_AUDIO_TRACKS, MAX_CLIP_DETAIL_FIELD_BYTES, MAX_CLIP_DETAIL_MARKERS,
    MAX_CLIP_DETAIL_SIDECAR_BYTES, MAX_CLIP_SIDECAR_JSON_DEPTH, MAX_CLIP_SIDECAR_JSON_ENTRIES,
    MAX_CLIP_SIDECAR_NESTED_ENTRIES, MAX_CLIP_SIDECAR_PLAYS, MAX_CLIP_SIDECAR_STRING_BYTES,
};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "clipline-library-detail-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn marker(kind: &str, tick: f64) -> serde_json::Value {
    serde_json::json!({
        "t_s": tick,
        "game_id": "LeagueOfLegends",
        "kind": kind,
        "actor": "Dain",
        "victim": "Opponent",
        "assisters": ["Support"],
        "subtype": null,
        "game_time_s": 42.0,
        "recording_offset_s": 142.0,
        "importance": 7,
        "involves_local_player": true
    })
}

fn sidecar() -> serde_json::Value {
    serde_json::json!({
        "recording_start_s": 100.0,
        "duration_s": 20.0,
        "player_summary": {
            "champion_name": "Nautilus",
            "kills": 3,
            "deaths": 4,
            "assists": 23,
            "player_name": "Dain",
            "team": "Blue",
            "participants": [{
                "player_name": "Dain",
                "champion_name": "Nautilus",
                "team": "Blue"
            }],
            "summoner_spells": [{"name": "Flash", "asset_key": "flash"}],
            "items": [{"id": 1001, "name": "Boots", "slot": 0}]
        },
        "audio_tracks": [{
            "id": "output",
            "track_index": 0,
            "label": "Desktop audio",
            "kind": "output"
        }],
        "plays": [{
            "game_id": "Osu",
            "source": "osu_api",
            "external_id": "score:1",
            "url": "https://osu.ppy.sh/scores/1",
            "title": "Everything will freeze",
            "artist": "UNDEAD CORPORATION",
            "difficulty": "Time Freeze",
            "mapper": "Ekoro",
            "star_rating": 6.42,
            "mods": ["HD", "DT"],
            "rank": "A",
            "passed": true,
            "accuracy": 0.98,
            "max_combo": 789,
            "total_score": 1234567,
            "pp": 400.5,
            "started_at": "2026-08-02T00:00:00Z",
            "ended_at": "2026-08-02T00:02:00Z",
            "derived_start": false,
            "t_start_s": 2.0,
            "t_end_s": 18.0
        }],
        "markers": [marker("ChampionKill", 4.25), marker("GameStart", 0.0)]
    })
}

fn json_bytes(value: &serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(value).unwrap()
}

fn request(path: &Path, request: u64) -> ClipDetailRequest {
    ClipDetailRequest::new(
        ClipPathIdentity::from_text(&path.display().to_string()).unwrap(),
        WindowWorkToken {
            attachment: WindowAttachmentGeneration::new(2),
            foreground: ForegroundGeneration::new(3),
            request: RequestGeneration::new(request),
        },
    )
}

#[derive(Clone, Copy)]
struct FixedProbe(Result<usize, &'static str>);

impl LegacyAudioTrackProbe for FixedProbe {
    fn audio_track_count(&self, _clip_path: &Path) -> Result<usize, String> {
        self.0.map_err(str::to_owned)
    }
}

#[test]
fn one_bounded_parse_supports_full_compatibility_and_compact_projections() {
    let parsed = parse_marker_sidecar(&json_bytes(&sidecar())).unwrap();

    let full = parsed.markers();
    assert_eq!(full.markers.len(), 1, "review policy drops GameStart");
    assert_eq!(full.audio_tracks.len(), 1);
    assert_eq!(full.plays.len(), 1);

    let summary = parsed.summary();
    assert_eq!(summary.duration_s, 20.0);
    assert_eq!(summary.review_marker_count, 1);
    assert_eq!(summary.marker_digest, "1 kill");
    assert_eq!(summary.audio_track_count, 1);
    assert_eq!((summary.plays.total, summary.plays.passed), (1, 1));
    assert_eq!((summary.plays.failed, summary.plays.incomplete), (0, 0));
    assert_eq!(
        summary.player_summary.as_ref().map(|player| (
            player.champion_name.as_str(),
            player.kills,
            player.deaths,
            player.assists
        )),
        Some(("Nautilus", 3, 4, 23))
    );
    for term in [
        "Nautilus",
        "Dain",
        "Everything will freeze",
        "UNDEAD CORPORATION",
        "Time Freeze",
    ] {
        assert!(summary.search_text.contains(term), "missing {term:?}");
    }
    assert!(summary.search_text.len() <= MAX_CLIP_DETAIL_FIELD_BYTES);
}

#[test]
fn preserving_parse_keeps_non_review_markers_for_field_replacement() {
    let preserved = parse_marker_sidecar_preserving_all(&json_bytes(&sidecar())).unwrap();
    assert_eq!(preserved.markers.len(), 2);
    assert!(preserved
        .markers
        .iter()
        .any(|marker| marker.event.kind == clipline_events::EventKind::GameStart));

    let projected = parse_marker_sidecar(&json_bytes(&sidecar())).unwrap();
    assert_eq!(projected.markers().markers.len(), 1);
}

#[test]
fn compact_projection_truncates_full_sidecar_text_to_the_catalog_row_bound() {
    let mut value = sidecar();
    let oversized = "é".repeat(MAX_CATALOG_STRING_BYTES);
    value["player_summary"]["champion_name"] = serde_json::Value::String(oversized);

    let parsed = parse_marker_sidecar(&json_bytes(&value)).unwrap();
    assert!(
        parsed
            .markers()
            .player_summary
            .as_ref()
            .unwrap()
            .champion_name
            .len()
            > MAX_CATALOG_STRING_BYTES,
        "the full compatibility projection retains the larger valid sidecar field"
    );
    let compact = parsed.summary();
    assert_eq!(
        compact.player_summary.as_ref().unwrap().champion_name.len(),
        MAX_CATALOG_STRING_BYTES
    );
    assert!(compact.search_text.len() <= MAX_CATALOG_STRING_BYTES);
    assert!(compact.marker_digest.len() <= MAX_CATALOG_STRING_BYTES);
}

#[test]
fn corrupt_json_and_excessive_nesting_are_rejected_before_projection() {
    assert!(matches!(
        parse_marker_sidecar(br#"{"duration_s": nope}"#),
        Err(LocalSidecarError::InvalidJson { .. })
    ));

    let nested = format!(
        "{{\"recording_start_s\":0,\"duration_s\":0,\"markers\":[],\"extra\":{}{}}}",
        "[".repeat(MAX_CLIP_SIDECAR_JSON_DEPTH),
        "]".repeat(MAX_CLIP_SIDECAR_JSON_DEPTH)
    );
    assert!(matches!(
        parse_marker_sidecar(nested.as_bytes()),
        Err(LocalSidecarError::TooLarge {
            field: "sidecar.json_depth",
            ..
        })
    ));
}

#[test]
fn marker_play_audio_and_nested_entry_counts_are_bounded_before_use() {
    let mut value = sidecar();
    value["markers"] = serde_json::Value::Array(
        (0..=MAX_CLIP_DETAIL_MARKERS)
            .map(|_| marker("ChampionKill", 0.0))
            .collect(),
    );
    assert!(matches!(
        parse_marker_sidecar(&json_bytes(&value)),
        Err(LocalSidecarError::TooLarge {
            field: "sidecar.markers",
            ..
        })
    ));

    let mut value = sidecar();
    value["plays"] = serde_json::Value::Array(
        (0..=MAX_CLIP_SIDECAR_PLAYS)
            .map(|_| sidecar()["plays"][0].clone())
            .collect(),
    );
    assert!(matches!(
        parse_marker_sidecar(&json_bytes(&value)),
        Err(LocalSidecarError::TooLarge {
            field: "sidecar.plays",
            ..
        })
    ));

    let mut value = sidecar();
    value["audio_tracks"] = serde_json::Value::Array(
        (0..=MAX_CLIP_DETAIL_AUDIO_TRACKS)
            .map(|index| {
                serde_json::json!({
                    "id": format!("audio:{index}"),
                    "track_index": index,
                    "label": "Audio"
                })
            })
            .collect(),
    );
    assert!(matches!(
        parse_marker_sidecar(&json_bytes(&value)),
        Err(LocalSidecarError::TooLarge {
            field: "sidecar.audio_tracks",
            ..
        })
    ));

    let mut value = sidecar();
    value["markers"][0]["assisters"] = serde_json::Value::Array(
        (0..=MAX_CLIP_SIDECAR_NESTED_ENTRIES)
            .map(|_| serde_json::Value::String(String::new()))
            .collect(),
    );
    assert!(matches!(
        parse_marker_sidecar(&json_bytes(&value)),
        Err(LocalSidecarError::TooLarge {
            field: "sidecar.nested_entries",
            ..
        })
    ));

    let mut value = sidecar();
    value["unknown_entries"] = serde_json::Value::Array(
        (0..=MAX_CLIP_SIDECAR_JSON_ENTRIES)
            .map(|_| serde_json::Value::Null)
            .collect(),
    );
    assert!(matches!(
        parse_marker_sidecar(&json_bytes(&value)),
        Err(LocalSidecarError::TooLarge {
            field: "sidecar.json_entries",
            ..
        })
    ));
}

#[test]
fn individual_and_aggregate_decoded_string_bytes_are_bounded() {
    let mut individual = sidecar();
    individual["markers"][0]["actor"] =
        serde_json::Value::String("x".repeat(MAX_CLIP_DETAIL_FIELD_BYTES + 1));
    assert!(matches!(
        parse_marker_sidecar(&json_bytes(&individual)),
        Err(LocalSidecarError::TooLarge {
            field: "sidecar.string",
            ..
        })
    ));

    let chunk = "x".repeat(MAX_CLIP_DETAIL_FIELD_BYTES);
    let count = MAX_CLIP_SIDECAR_STRING_BYTES / chunk.len() + 1;
    let mut aggregate = sidecar();
    aggregate["extra"] = serde_json::Value::Array(
        (0..count)
            .map(|_| serde_json::Value::String(chunk.clone()))
            .collect(),
    );
    assert!(matches!(
        parse_marker_sidecar(&json_bytes(&aggregate)),
        Err(LocalSidecarError::TooLarge {
            field: "sidecar.string_bytes",
            ..
        })
    ));
}

#[test]
fn hostile_timeline_values_are_rejected() {
    for (field, value) in [
        ("duration_s", serde_json::json!(-0.01)),
        ("duration_s", serde_json::json!(1.0e300)),
        ("recording_start_s", serde_json::json!(-1.0)),
    ] {
        let mut hostile = sidecar();
        hostile[field] = value;
        assert!(matches!(
            parse_marker_sidecar(&json_bytes(&hostile)),
            Err(LocalSidecarError::InvalidTimeline { .. })
        ));
    }

    let mut marker_after_end = sidecar();
    marker_after_end["markers"][0]["t_s"] = serde_json::json!(20.01);
    assert!(matches!(
        parse_marker_sidecar(&json_bytes(&marker_after_end)),
        Err(LocalSidecarError::InvalidTimeline { .. })
    ));

    let mut inverted_play = sidecar();
    inverted_play["plays"][0]["t_start_s"] = serde_json::json!(10.0);
    inverted_play["plays"][0]["t_end_s"] = serde_json::json!(9.0);
    assert!(matches!(
        parse_marker_sidecar(&json_bytes(&inverted_play)),
        Err(LocalSidecarError::InvalidTimeline { .. })
    ));
}

#[test]
fn file_reader_accepts_the_exact_byte_ceiling_and_rejects_one_byte_more() {
    let temp = TempDir::new();
    let clip = temp.join("clip.mp4");
    std::fs::write(&clip, []).unwrap();
    let marker_path = marker_sidecar_path(&clip);
    let mut exact = br#"{"recording_start_s":0,"duration_s":0,"markers":[]}"#.to_vec();
    exact.resize(MAX_CLIP_DETAIL_SIDECAR_BYTES, b' ');
    std::fs::write(&marker_path, &exact).unwrap();

    let parsed = read_marker_sidecar(&clip).unwrap().unwrap();
    assert_eq!(parsed.sidecar_bytes(), MAX_CLIP_DETAIL_SIDECAR_BYTES);

    exact.push(b' ');
    std::fs::write(&marker_path, exact).unwrap();
    assert!(matches!(
        read_marker_sidecar(&clip),
        Err(LocalSidecarError::TooLarge {
            field: "sidecar.bytes",
            ..
        })
    ));
}

#[test]
fn sidecar_escape_is_rejected_and_successful_reads_release_the_file() {
    let temp = TempDir::new();
    let clip = temp.join("clip.mp4");
    std::fs::write(&clip, []).unwrap();
    let sidecar_path = marker_sidecar_path(&clip);
    std::fs::write(&sidecar_path, json_bytes(&sidecar())).unwrap();

    assert!(read_marker_sidecar(&clip).unwrap().is_some());
    let released_path = temp.join("released.markers.json");
    std::fs::rename(&sidecar_path, &released_path)
        .expect("the bounded reader must release its handle before returning");

    let outside_temp = TempDir::new();
    let outside = outside_temp.join("outside.json");
    std::fs::write(&outside, json_bytes(&sidecar())).unwrap();
    if create_file_symlink(&outside, &sidecar_path).is_ok() {
        assert!(matches!(
            read_marker_sidecar(&clip),
            Err(LocalSidecarError::UnsafeFileType { .. })
        ));
    }
}

#[cfg(unix)]
fn create_file_symlink(source: &Path, target: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(source, target)
}

#[cfg(windows)]
fn create_file_symlink(source: &Path, target: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(source, target)
}

#[test]
fn legacy_audio_probe_is_skipped_for_declared_tracks_and_used_otherwise() {
    let temp = TempDir::new();
    let clip = temp.join("clip.mp4");
    std::fs::write(&clip, []).unwrap();
    let mut no_audio = sidecar();
    no_audio["audio_tracks"] = serde_json::json!([]);
    std::fs::write(marker_sidecar_path(&clip), json_bytes(&no_audio)).unwrap();

    let parsed = load_marker_sidecar_with_probe(&clip, &FixedProbe(Ok(2)))
        .unwrap()
        .unwrap();
    assert_eq!(parsed.summary().audio_track_count, 2);
    assert_eq!(parsed.markers().audio_tracks[0].id, "audio:0");
    assert_eq!(parsed.markers().audio_tracks[1].label, "Audio Track 2");

    std::fs::write(marker_sidecar_path(&clip), json_bytes(&sidecar())).unwrap();
    assert!(load_marker_sidecar_with_probe(&clip, &FixedProbe(Err("must not run"))).is_ok());

    std::fs::remove_file(marker_sidecar_path(&clip)).unwrap();
    let inferred_without_sidecar = load_marker_sidecar_with_probe(&clip, &FixedProbe(Ok(2)))
        .unwrap()
        .unwrap();
    assert_eq!(inferred_without_sidecar.sidecar_bytes(), 0);
    assert_eq!(inferred_without_sidecar.summary().duration_s, 0.0);
    assert_eq!(inferred_without_sidecar.summary().audio_track_count, 2);

    let too_many = FixedProbe(Ok(MAX_CLIP_DETAIL_AUDIO_TRACKS + 1));
    std::fs::write(marker_sidecar_path(&clip), json_bytes(&no_audio)).unwrap();
    assert!(matches!(
        load_marker_sidecar_with_probe(&clip, &too_many),
        Err(LocalSidecarError::TooLarge {
            field: "sidecar.audio_tracks",
            ..
        })
    ));

    std::fs::write(marker_sidecar_path(&clip), b"not json").unwrap();
    assert!(matches!(
        load_marker_sidecar_with_probe(&clip, &FixedProbe(Ok(2))),
        Err(LocalSidecarError::InvalidJson { .. })
    ));
}

#[test]
fn detail_loading_is_token_fenced_and_retains_only_bounded_projection() {
    let temp = TempDir::new();
    let clip = temp.join("clip.mp4");
    std::fs::write(&clip, []).unwrap();
    std::fs::write(marker_sidecar_path(&clip), json_bytes(&sidecar())).unwrap();
    let initial = request(&clip, 10);

    let result = load_clip_detail(
        &initial,
        &clip,
        "Round win",
        "A close finish",
        &FixedProbe(Err("declared audio must skip probe")),
    )
    .unwrap();

    assert!(result.matches_request(&initial));
    assert!(!result.matches_request(&request(&clip, 11)));
    assert_eq!(result.detail().marker_ticks()[0].seconds(), 4.25);
    assert_eq!(result.detail().marker_digest(), "1 kill");
    assert_eq!(result.detail().audio_tracks()[0].id(), "output");
    assert_eq!(result.detail().upload().title(), "Round win");
    assert_eq!(result.detail().upload().marker_summary(), "1 kill");
    assert_eq!(result.detail().upload().audio_summary(), "1 audio track");
    assert!(result.detail().sidecar_bytes() > 0);
}
