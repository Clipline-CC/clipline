use super::*;

#[tauri::command]
#[allow(clippy::too_many_arguments)] // Tauri exposes these as named invoke fields.
pub async fn export_clip<R: Runtime>(
    app: AppHandle<R>,
    path: String,
    start_s: f64,
    end_s: f64,
    title: Option<String>,
    include_markers: Option<bool>,
    group: Option<String>,
    settings: tauri::State<'_, StorageSettings>,
) -> Result<ExportedClipInfo, String> {
    let scope_root = settings.clips_dir()?;
    let source = validate_clip_path(&settings, &path)?;
    let include_markers = include_markers.unwrap_or(true);
    let group_root = scope_root.clone();
    let exported = tauri::async_runtime::spawn_blocking(move || {
        let group = group
            .map(|name| groups::group_for_export(&group_root, &name))
            .transpose()?;
        export_clip_file(
            source,
            start_s,
            end_s,
            title,
            include_markers,
            group,
            &group_root,
        )
    })
    .await
    .map_err(|e| format!("export clip task: {e}"))??;
    allow_local_clip_asset(&app, &scope_root, Path::new(&exported.path))?;
    Ok(exported)
}

#[tauri::command]

pub(crate) fn export_clip_file(
    source: PathBuf,
    start_s: f64,
    end_s: f64,
    title: Option<String>,
    include_markers: bool,
    group: Option<ClipGroup>,
    media_root: &Path,
) -> Result<ExportedClipInfo, String> {
    let tmp = unique_temp_export_path(&source)?;
    let info = match trim_keyframe_aligned_file(&source, &tmp, start_s, end_s) {
        Ok(info) => info,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.to_string());
        }
    };
    let target = unique_export_path(
        &source,
        info.aligned_start_s,
        info.aligned_end_s,
        title.clone(),
    )?;
    if let Err(error) = std::fs::rename(&tmp, &target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error.to_string());
    }

    let exported_markers = match export_markers_for_range(
        &source,
        info.aligned_start_s,
        info.aligned_end_s,
        include_markers,
    ) {
        Ok(markers) => markers,
        Err(error) => {
            let _ = remove_clip_files(&target, media_root);
            return Err(error);
        }
    };
    let sidecars = (|| {
        if let Some(markers) = &exported_markers {
            let json = serde_json::to_string_pretty(markers).map_err(|e| e.to_string())?;
            std::fs::write(target.with_extension("markers.json"), json)
                .map_err(|e| e.to_string())?;
        }
        if group.is_some() {
            write_clip_metadata(
                &target,
                &ClipMetadata {
                    title,
                    kind: Some("trim".to_string()),
                    group: group.clone(),
                    source_group: None,
                    source_group_fingerprint: None,
                },
            )?;
        }
        Ok::<(), String>(())
    })();
    if let Err(error) = sidecars {
        let _ = remove_clip_files(&target, media_root);
        return Err(error);
    }
    let meta =
        std::fs::metadata(&target).map_err(|e| format!("read exported clip metadata: {e}"))?;
    let modified_unix = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok(ExportedClipInfo {
        path: target.display().to_string(),
        name: target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        size_mb: meta.len() as f64 / (1024.0 * 1024.0),
        modified_unix,
        requested_start_s: info.requested_start_s,
        requested_end_s: info.requested_end_s,
        aligned_start_s: info.aligned_start_s,
        aligned_end_s: info.aligned_end_s,
        duration_s: info.duration_s,
        markers: exported_markers,
        group,
    })
}

pub(crate) fn filter_review_markers(mut markers: ClipMarkers) -> ClipMarkers {
    markers.markers.retain(|m| is_review_event(&m.event));
    markers
}

pub(crate) fn has_marker_sidecar_content(markers: &ClipMarkers) -> bool {
    !markers.markers.is_empty()
        || !markers.bookmarks.is_empty()
        || markers.player_summary.is_some()
        || !markers.audio_tracks.is_empty()
        || !markers.plays.is_empty()
}

pub(crate) fn crop_markers(markers: &ClipMarkers, start_s: f64, end_s: f64) -> ClipMarkers {
    let cropped = markers
        .markers
        .iter()
        .filter(|m| m.t_s >= start_s && m.t_s < end_s)
        .map(|m| ClipMarker {
            t_s: m.t_s - start_s,
            event: m.event.clone(),
        })
        .collect();
    let plays = markers
        .plays
        .iter()
        .filter_map(|play| crop_play(play, start_s, end_s))
        .collect();
    let bookmarks = markers
        .bookmarks
        .iter()
        .filter(|bookmark| bookmark.t_s >= start_s && bookmark.t_s < end_s)
        .map(|bookmark| ClipBookmark {
            t_s: bookmark.t_s - start_s,
        })
        .collect();
    ClipMarkers {
        recording_start_s: markers.recording_start_s + start_s,
        duration_s: end_s - start_s,
        player_summary: markers.player_summary.clone(),
        audio_tracks: markers.audio_tracks.clone(),
        plays,
        markers: cropped,
        bookmarks,
    }
}

pub(crate) fn crop_play(play: &ClipPlay, start_s: f64, end_s: f64) -> Option<ClipPlay> {
    if let Some(play_end_s) = play.t_end_s {
        if play_end_s <= start_s || play.t_start_s >= end_s {
            return None;
        }
        let mut cropped = play.clone();
        cropped.t_start_s = play.t_start_s.max(start_s) - start_s;
        cropped.t_end_s = Some(play_end_s.min(end_s) - start_s);
        Some(cropped)
    } else if play.t_start_s >= start_s && play.t_start_s < end_s {
        let mut cropped = play.clone();
        cropped.t_start_s -= start_s;
        Some(cropped)
    } else {
        None
    }
}

pub(crate) fn export_markers_for_range(
    source: &Path,
    start_s: f64,
    end_s: f64,
    include_markers: bool,
) -> Result<Option<ClipMarkers>, String> {
    if !include_markers {
        return Ok(None);
    }
    let Some(markers) = util::read_markers_raw(source).map(filter_review_markers) else {
        return Ok(None);
    };
    let cropped = crop_markers(&markers, start_s, end_s);
    Ok(has_marker_sidecar_content(&cropped).then_some(cropped))
}

pub(crate) fn unique_temp_export_path(source: &Path) -> Result<PathBuf, String> {
    let parent = source
        .parent()
        .ok_or_else(|| "source clip has no parent directory".to_string())?;
    let stem = source
        .file_stem()
        .map(|s| s.to_string_lossy())
        .ok_or_else(|| "source clip has no file stem".to_string())?;
    for suffix in 0..1000u32 {
        let name = format!("{stem}_trim_pending_{suffix:03}.mp4.tmp");
        let candidate = parent.join(name);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err("could not choose an unused temporary export filename".into())
}

pub(crate) fn unique_export_path(
    source: &Path,
    start_s: f64,
    end_s: f64,
    title: Option<String>,
) -> Result<PathBuf, String> {
    let parent = source
        .parent()
        .ok_or_else(|| "source clip has no parent directory".to_string())?;
    let stem = source
        .file_stem()
        .map(|s| s.to_string_lossy())
        .ok_or_else(|| "source clip has no file stem".to_string())?;
    let start_ms = (start_s * 1000.0).round().max(0.0) as u64;
    let end_ms = (end_s * 1000.0).round().max(0.0) as u64;
    let titled_stem = title.as_deref().and_then(export_title_stem);
    for suffix in 0..1000u32 {
        let name = if let Some(titled_stem) = titled_stem.as_deref() {
            if suffix == 0 {
                format!("{titled_stem}.mp4")
            } else {
                format!("{titled_stem}_{suffix}.mp4")
            }
        } else if suffix == 0 {
            format!("{stem}_trim_{start_ms:06}_{end_ms:06}.mp4")
        } else {
            format!("{stem}_trim_{start_ms:06}_{end_ms:06}_{suffix}.mp4")
        };
        let candidate = parent.join(name);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err("could not choose an unused export filename".into())
}

pub(crate) fn export_title_stem(title: &str) -> Option<String> {
    let sanitized: String = title
        .chars()
        .map(|ch| {
            if ch.is_ascii_control()
                || matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*' | '/' | '\\')
            {
                ' '
            } else {
                ch
            }
        })
        .collect();
    let collapsed = sanitized.split_whitespace().collect::<Vec<_>>().join(" ");
    let stem = collapsed.trim().trim_end_matches(['.', ' ']);
    if stem.is_empty() || stem == "." || stem == ".." || is_reserved_windows_file_name(stem) {
        None
    } else {
        Some(stem.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_test_utils::TestDir;
    use clipline_events::{ClipAudioTrack, ClipMarkers, EventKind, PlayerSummary};
        #[test]
        fn crop_markers_rebases_times_and_recording_start() {
            let markers = ClipMarkers {
                bookmarks: Vec::new(),
                recording_start_s: 10.0,
                duration_s: 5.0,
                player_summary: Some(PlayerSummary {
                    champion_name: "Nautilus".into(),
                    kills: 3,
                    deaths: 4,
                    assists: 23,
                    creep_score: None,
                    game_time_s: None,
                    player_name: String::new(),
                    team: String::new(),
                    participants: Vec::new(),
                    summoner_spells: Vec::new(),
                    items: Vec::new(),
                }),
                audio_tracks: Vec::new(),
                plays: Vec::new(),
                markers: vec![marker(0.5), marker(1.5), marker(2.5)],
            };

            let cropped = crop_markers(&markers, 1.0, 2.0);

            assert_eq!(cropped.markers.len(), 1);
            assert!((cropped.markers[0].t_s - 0.5).abs() < 1e-9);
            assert!((cropped.recording_start_s - 11.0).abs() < 1e-9);
            assert!((cropped.duration_s - 1.0).abs() < 1e-9);
            assert_eq!(
                cropped.player_summary.as_ref().map(|summary| (
                    summary.champion_name.as_str(),
                    summary.kills,
                    summary.deaths,
                    summary.assists
                )),
                Some(("Nautilus", 3, 4, 23))
            );
        }
        #[test]
        fn crop_markers_crops_and_rebases_user_bookmarks() {
            let markers = ClipMarkers {
                recording_start_s: 10.0,
                duration_s: 5.0,
                player_summary: None,
                audio_tracks: Vec::new(),
                plays: Vec::new(),
                markers: Vec::new(),
                bookmarks: vec![
                    ClipBookmark { t_s: 0.5 },
                    ClipBookmark { t_s: 1.5 },
                    ClipBookmark { t_s: 2.0 },
                ],
            };

            let cropped = crop_markers(&markers, 1.0, 2.0);

            assert_eq!(
                cropped.bookmarks,
                [ClipBookmark { t_s: 0.5 }],
                "inclusive start, exclusive end, re-based like game markers"
            );
            // A bookmark-only trim still has content worth writing a sidecar for.
            assert!(has_marker_sidecar_content(&cropped));
        }
        #[test]
        fn filter_review_markers_keeps_match_event_sources_and_drops_noise() {
            let markers = ClipMarkers {
                bookmarks: Vec::new(),
                recording_start_s: 10.0,
                duration_s: 100.0,
                player_summary: Some(PlayerSummary {
                    champion_name: "Nautilus".into(),
                    kills: 3,
                    deaths: 4,
                    assists: 23,
                    creep_score: None,
                    game_time_s: None,
                    player_name: String::new(),
                    team: String::new(),
                    participants: Vec::new(),
                    summoner_spells: Vec::new(),
                    items: Vec::new(),
                }),
                audio_tracks: Vec::new(),
                plays: Vec::new(),
                markers: vec![
                    marker_with(1.0, EventKind::ChampionKill, true),
                    marker_with(2.0, EventKind::ChampionKill, false),
                    marker_with(2.5, EventKind::ChampionDeath, true),
                    marker_with(3.0, EventKind::TurretKilled, false),
                    marker_with(4.0, EventKind::DragonKill, false),
                    marker_with(5.0, EventKind::BaronKill, false),
                    marker_with(5.5, EventKind::HeraldKill, false),
                    marker_with(6.0, EventKind::MinionsSpawning, true),
                    marker_with(7.0, EventKind::FirstBlood, true),
                    marker_with(8.0, EventKind::FirstBrick, true),
                    marker_with(9.0, EventKind::Ace, true),
                ],
            };

            let filtered = filter_review_markers(markers);
            let kinds: Vec<_> = filtered.markers.iter().map(|m| m.event.kind).collect();

            assert_eq!(
                kinds,
                vec![
                    EventKind::ChampionKill,
                    EventKind::ChampionKill,
                    EventKind::ChampionDeath,
                    EventKind::TurretKilled,
                    EventKind::DragonKill,
                    EventKind::BaronKill,
                    EventKind::HeraldKill,
                ]
            );
            assert!(filtered.markers[0].event.involves_local_player);
            assert!(!filtered.markers[1].event.involves_local_player);
            assert_eq!(
                filtered.player_summary.as_ref().map(|summary| (
                    summary.champion_name.as_str(),
                    summary.kills,
                    summary.deaths,
                    summary.assists
                )),
                Some(("Nautilus", 3, 4, 23))
            );
        }
        #[test]
        fn summary_only_markers_are_export_sidecar_content() {
            let markers = ClipMarkers {
                bookmarks: Vec::new(),
                recording_start_s: 10.0,
                duration_s: 20.0,
                player_summary: Some(PlayerSummary {
                    champion_name: "Nautilus".into(),
                    kills: 3,
                    deaths: 4,
                    assists: 23,
                    creep_score: None,
                    game_time_s: None,
                    player_name: String::new(),
                    team: String::new(),
                    participants: Vec::new(),
                    summoner_spells: Vec::new(),
                    items: Vec::new(),
                }),
                audio_tracks: Vec::new(),
                plays: Vec::new(),
                markers: Vec::new(),
            };

            assert!(has_marker_sidecar_content(&markers));
        }
        #[test]
        fn empty_markers_are_not_export_sidecar_content() {
            let markers = ClipMarkers {
                bookmarks: Vec::new(),
                recording_start_s: 10.0,
                duration_s: 20.0,
                player_summary: None,
                audio_tracks: Vec::new(),
                plays: Vec::new(),
                markers: Vec::new(),
            };

            assert!(!has_marker_sidecar_content(&markers));
        }
        #[test]
        fn play_only_markers_are_export_sidecar_content() {
            let markers = ClipMarkers {
                bookmarks: Vec::new(),
                recording_start_s: 10.0,
                duration_s: 20.0,
                player_summary: None,
                audio_tracks: Vec::new(),
                plays: vec![osu_play(2.0, Some(8.0), "score-1")],
                markers: Vec::new(),
            };

            assert!(has_marker_sidecar_content(&markers));
        }
        #[test]
        fn export_markers_can_be_suppressed_for_play_exports() {
            let dir = TestDir::new("clipline-library", "export-no-markers");
            let source = dir.path().join("session.mp4");
            std::fs::write(&source, b"mp4").unwrap();
            let markers = ClipMarkers {
                bookmarks: Vec::new(),
                recording_start_s: 10.0,
                duration_s: 20.0,
                player_summary: None,
                audio_tracks: Vec::new(),
                plays: vec![osu_play(2.0, Some(8.0), "score-1")],
                markers: Vec::new(),
            };
            std::fs::write(
                source.with_extension("markers.json"),
                serde_json::to_string(&markers).unwrap(),
            )
            .unwrap();

            assert!(export_markers_for_range(&source, 2.0, 8.0, false)
                .unwrap()
                .is_none());
        }
        #[test]
        fn crop_markers_keeps_and_clamps_overlapping_plays() {
            let markers = ClipMarkers {
                bookmarks: Vec::new(),
                recording_start_s: 10.0,
                duration_s: 20.0,
                player_summary: None,
                audio_tracks: Vec::new(),
                plays: vec![
                    osu_play(0.0, Some(2.0), "before"),
                    osu_play(2.0, Some(8.0), "overlap"),
                    osu_play(5.0, None, "point"),
                    osu_play(8.0, Some(12.0), "after"),
                ],
                markers: Vec::new(),
            };

            let cropped = crop_markers(&markers, 4.0, 6.0);

            let ids: Vec<_> = cropped
                .plays
                .iter()
                .map(|play| play.external_id.as_str())
                .collect();
            assert_eq!(ids, vec!["overlap", "point"]);
            assert_eq!(cropped.plays[0].t_start_s, 0.0);
            assert_eq!(cropped.plays[0].t_end_s, Some(2.0));
            assert_eq!(cropped.plays[1].t_start_s, 1.0);
            assert_eq!(cropped.plays[1].t_end_s, None);
        }
        #[test]
        fn audio_tracks_are_export_sidecar_content_and_survive_cropping() {
            let tracks = vec![ClipAudioTrack {
                id: "microphone".into(),
                track_index: 1,
                label: "Microphone".into(),
                kind: Some("microphone".into()),
            }];
            let markers = ClipMarkers {
                bookmarks: Vec::new(),
                recording_start_s: 10.0,
                duration_s: 20.0,
                player_summary: None,
                audio_tracks: tracks.clone(),
                plays: Vec::new(),
                markers: Vec::new(),
            };

            assert!(has_marker_sidecar_content(&markers));
            let cropped = crop_markers(&markers, 3.0, 7.0);

            assert_eq!(cropped.audio_tracks, tracks);
            assert_eq!(cropped.markers.len(), 0);
            assert!((cropped.duration_s - 4.0).abs() < 1e-9);
        }
        #[test]
        fn unique_export_path_appends_suffix_when_needed() {
            let dir = TestDir::new("clipline-library", "export-name");
            let source = dir.path().join("clip_1.mp4");
            let first = dir.path().join("clip_1_trim_001000_002000.mp4");
            std::fs::write(&source, b"source").unwrap();
            std::fs::write(&first, b"existing").unwrap();

            let path = unique_export_path(&source, 1.0, 2.0, None).unwrap();

            assert_eq!(
                path.file_name().unwrap().to_string_lossy(),
                "clip_1_trim_001000_002000_1.mp4"
            );
        }
        #[test]
        fn unique_export_path_uses_requested_clip_title_when_present() {
            let dir = TestDir::new("clipline-library", "export-title");
            let source = dir.path().join("session_123.mp4");
            std::fs::write(&source, b"source").unwrap();

            let path = unique_export_path(
                &source,
                145.783,
                188.167,
                Some("I MY ME MINE - Trouble".to_string()),
            )
            .unwrap();

            assert_eq!(
                path.file_name().unwrap().to_string_lossy(),
                "I MY ME MINE - Trouble.mp4"
            );
        }
}
