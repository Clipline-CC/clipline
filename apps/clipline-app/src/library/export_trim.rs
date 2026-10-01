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
    // Keep the reserved final path and its sidecars owned by this job through
    // publication or rollback, excluding rename/delete/GC and other exports.
    let _guard = crate::gc::lock_clip_mutations();
    let target = match unique_export_path(
        &source,
        info.aligned_start_s,
        info.aligned_end_s,
        title.clone(),
    ) {
        Ok(target) => target,
        Err(error) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(error);
        }
    };
    if let Err(error) = std::fs::rename(&tmp, &target) {
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(&target);
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
            let _ = remove_clip_files_unlocked(&target, media_root);
            return Err(error);
        }
    };
    let sidecars = (|| {
        if let Some(markers) = &exported_markers {
            let json = util::serialize_json_sidecar(markers)?;
            std::fs::write(target.with_extension("markers.json"), json)
                .map_err(|e| e.to_string())?;
        }
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
        Ok::<(), String>(())
    })();
    if let Err(error) = sidecars {
        let _ = remove_clip_files_unlocked(&target, media_root);
        return Err(error);
    }
    let meta = match std::fs::metadata(&target) {
        Ok(meta) => meta,
        Err(error) => {
            let _ = remove_clip_files_unlocked(&target, media_root);
            return Err(format!("read exported clip metadata: {error}"));
        }
    };
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
        || markers.selected_audio_track_ids.is_some()
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
        selected_audio_track_ids: markers.selected_audio_track_ids.clone(),
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
    let Some(mut markers) =
        util::markers_with_inferred_audio_tracks(source, util::read_markers_checked(source)?)
    else {
        return Ok(None);
    };
    if include_markers {
        markers = filter_review_markers(markers);
    } else {
        markers.player_summary = None;
        markers.plays.clear();
        markers.markers.clear();
        markers.bookmarks.clear();
    }
    let cropped = crop_markers(&markers, start_s, end_s);
    Ok(has_marker_sidecar_content(&cropped).then_some(cropped))
}

/// Atomically reserve a pending path before the inner trim can replace it.
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
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("reserve temporary export filename: {error}")),
        }
    }
    Err("could not choose an unused temporary export filename".into())
}

/// Reserve the final destination; only its owner may replace this placeholder.
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
        // Orphaned sidecars belong to someone else even when the MP4 is absent.
        if clip_sidecar_paths(&candidate)
            .iter()
            .any(|path| path.symlink_metadata().is_ok())
            || clip_metadata_path(&candidate)
                .with_extension("clipline.json.tmp")
                .symlink_metadata()
                .is_ok()
        {
            continue;
        }
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("reserve export filename: {error}")),
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
    use clipline_events::{ClipAudioTrack, ClipMarkers, EventKind, PlayerSummary};
    use clipline_test_utils::TestDir;

    fn video_only_mp4() -> Vec<u8> {
        use clipline_mp4::{FragSample, HybridMp4Writer, VideoTrackConfig};
        let mut writer = HybridMp4Writer::new(
            std::io::Cursor::new(Vec::new()),
            VideoTrackConfig::h264(
                128,
                72,
                90_000,
                vec![0x67, 0x64, 0x00, 0x0A, 0xAC],
                vec![0x68, 0xEE, 0x38, 0x80],
            ),
        )
        .unwrap();
        let samples: Vec<_> = (0..20)
            .map(|i| FragSample {
                data: format!("V{i:05}").into_bytes(),
                duration: 9_000,
                is_sync: i % 5 == 0,
            })
            .collect();
        writer.write_fragment(&samples).unwrap();
        writer.finalize().unwrap().into_inner()
    }

    #[test]
    fn bh02_overlapping_pending_exports_preserve_each_range() {
        let dir = TestDir::new("clipline-library", "bh02-pending");
        let source = dir.path().join("import.mp4");
        let input = video_only_mp4();
        std::fs::write(&source, &input).unwrap();
        let barrier = std::sync::Barrier::new(2);
        let (first, second) = std::thread::scope(|scope| {
            let reserve = || {
                barrier.wait();
                unique_temp_export_path(&source).unwrap()
            };
            let first = scope.spawn(reserve);
            let second = scope.spawn(reserve);
            (first.join().unwrap(), second.join().unwrap())
        });
        // Both jobs have chosen their paths before either inner trim publishes.
        trim_keyframe_aligned_file(&source, &first, 0.0, 0.5).unwrap();
        trim_keyframe_aligned_file(&source, &second, 0.5, 1.5).unwrap();
        for (path, start, end) in [(&first, 0.0, 0.5), (&second, 0.5, 1.5)] {
            let (expected, _) = clipline_mp4::trim_keyframe_aligned(&input, start, end).unwrap();
            assert!(
                std::fs::read(path).unwrap() == expected,
                "job's pending MP4 was overwritten"
            );
        }
        assert_ne!(first, second);
        assert_eq!(std::fs::read(&source).unwrap(), input);
    }

    #[test]
    fn bh02_overlapping_final_names_preserve_each_range() {
        for title in [None, Some("Highlight".to_string())] {
            let dir = TestDir::new("clipline-library", "bh02-publication");
            let source = dir.path().join("import.mp4");
            let input = video_only_mp4();
            std::fs::write(&source, &input).unwrap();
            let first = unique_temp_export_path(&source).unwrap();
            trim_keyframe_aligned_file(&source, &first, 0.0, 0.5).unwrap();
            let second = unique_temp_export_path(&source).unwrap();
            trim_keyframe_aligned_file(&source, &second, 0.5, 1.5).unwrap();
            let barrier = std::sync::Barrier::new(2);
            let (first_target, second_target) = std::thread::scope(|scope| {
                // Equal name inputs exercise both titled and untitled collisions.
                let choose = || {
                    barrier.wait();
                    unique_export_path(&source, 0.0, 0.5, title.clone()).unwrap()
                };
                let first = scope.spawn(choose);
                let second = scope.spawn(choose);
                (first.join().unwrap(), second.join().unwrap())
            });
            std::fs::rename(&first, &first_target).unwrap();
            std::fs::rename(&second, &second_target).unwrap();
            assert_ne!(
                first_target, second_target,
                "jobs published to the same final path"
            );
            for (path, start, end) in [(&first_target, 0.0, 0.5), (&second_target, 0.5, 1.5)] {
                let (expected, _) =
                    clipline_mp4::trim_keyframe_aligned(&input, start, end).unwrap();
                assert_eq!(std::fs::read(path).unwrap(), expected);
            }
            assert_eq!(std::fs::read(&source).unwrap(), input);
        }
    }

    #[test]
    fn bh09_marker_and_audio_free_exports_rescan_as_owned_trims() {
        let dir = TestDir::new("clipline-library", "bh09-trim-identity");
        let source = dir.path().join("import.mp4");
        let input = video_only_mp4();
        std::fs::write(&source, &input).unwrap();
        for title in [None, Some("Highlight".to_string())] {
            let exported = export_clip_file(
                source.clone(),
                0.0,
                0.5,
                title.clone(),
                false,
                None,
                dir.path(),
            )
            .unwrap();
            let path = Path::new(&exported.path);
            assert!(exported.markers.is_none());
            assert!(!path.with_extension("markers.json").exists());
            assert!(clipline_storage::is_clip_owned(path));
            let scan = list_clips_from_dir(dir.path().to_path_buf()).unwrap();
            let clip = scan
                .clips
                .iter()
                .find(|clip| clip.path == exported.path)
                .unwrap();
            assert_eq!(clip.kind, "trim");
            assert_eq!(clip.title, title);
            assert!(clip.group.is_none());
            clipline_storage::delete_all_managed_media(dir.path()).unwrap();
            assert!(!path.exists());
            assert_eq!(std::fs::read(&source).unwrap(), input);
            assert!(!clipline_storage::is_clip_owned(&source));
        }
    }

    #[test]
    fn bh02_exhausted_final_names_clean_only_the_failed_jobs_pending_file() {
        let dir = TestDir::new("clipline-library", "bh02-exhaustion");
        let source = dir.path().join("import.mp4");
        let input = video_only_mp4();
        std::fs::write(&source, &input).unwrap();
        let other = unique_temp_export_path(&source).unwrap();
        trim_keyframe_aligned_file(&source, &other, 0.5, 1.5).unwrap();
        let other_bytes = std::fs::read(&other).unwrap();
        for suffix in 0..1000 {
            let name = if suffix == 0 {
                "Highlight.mp4".to_string()
            } else {
                format!("Highlight_{suffix}.mp4")
            };
            std::fs::write(dir.path().join(name), &input).unwrap();
        }
        let error = export_clip_file(
            source.clone(),
            0.0,
            0.5,
            Some("Highlight".into()),
            false,
            None,
            dir.path(),
        )
        .err()
        .unwrap();
        assert!(error.contains("export filename"));
        assert_eq!(std::fs::read(&source).unwrap(), input);
        assert_eq!(std::fs::read(&other).unwrap(), other_bytes);
        let pending: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "tmp"))
            .collect();
        assert_eq!(pending, [other], "failed export leaked its pending MP4");
    }

    #[test]
    fn bh02_concurrent_exports_preserve_video_markers_and_group_metadata() {
        let dir = TestDir::new("clipline-library", "bh02-complete-exports");
        let source = dir.path().join("import.mp4");
        let input = video_only_mp4();
        std::fs::write(&source, &input).unwrap();
        let markers = ClipMarkers {
            recording_start_s: 10.0,
            duration_s: 2.0,
            player_summary: None,
            audio_tracks: Vec::new(),
            selected_audio_track_ids: None,
            plays: Vec::new(),
            markers: vec![marker(0.25), marker(0.75), marker(1.25)],
            bookmarks: vec![ClipBookmark { t_s: 0.75 }],
        };
        std::fs::write(
            source.with_extension("markers.json"),
            util::serialize_json_sidecar(&markers).unwrap(),
        )
        .unwrap();
        let barrier = std::sync::Barrier::new(2);
        let exports = std::thread::scope(|scope| {
            let jobs: Vec<_> = (0..2)
                .map(|i| {
                    let source = &source;
                    let barrier = &barrier;
                    let root = dir.path();
                    scope.spawn(move || {
                        let group = ClipGroup {
                            name: "Highlights".into(),
                            order: i,
                        };
                        barrier.wait();
                        export_clip_file(
                            source.clone(),
                            i as f64 * 0.5,
                            0.5 + i as f64,
                            Some("Highlight".into()),
                            true,
                            Some(group),
                            root,
                        )
                        .unwrap()
                    })
                })
                .collect();
            jobs.into_iter()
                .map(|job| job.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_ne!(exports[0].path, exports[1].path);
        for (i, exported) in exports.iter().enumerate() {
            let path = Path::new(&exported.path);
            let (expected, info) =
                clipline_mp4::trim_keyframe_aligned(&input, i as f64 * 0.5, 0.5 + i as f64)
                    .unwrap();
            assert_eq!(std::fs::read(path).unwrap(), expected);
            assert_eq!(exported.aligned_start_s, info.aligned_start_s);
            assert_eq!(exported.duration_s, info.duration_s);
            let metadata = read_clip_metadata(path).unwrap();
            assert_eq!(metadata.kind.as_deref(), Some("trim"));
            assert_eq!(metadata.title.as_deref(), Some("Highlight"));
            assert_eq!(metadata.group, exported.group);
            assert_eq!(metadata.group.unwrap().order, i as u32);
            assert_eq!(
                serde_json::to_value(util::read_markers_checked(path).unwrap().unwrap()).unwrap(),
                serde_json::to_value(crop_markers(
                    &markers,
                    info.aligned_start_s,
                    info.aligned_end_s
                ))
                .unwrap()
            );
        }
        assert_eq!(std::fs::read(&source).unwrap(), input);
        assert!(!clipline_storage::is_clip_owned(&source));
        assert!(std::fs::read_dir(dir.path()).unwrap().all(|entry| entry
            .unwrap()
            .path()
            .extension()
            .is_none_or(|ext| ext != "tmp")));
    }

    #[test]
    fn bh02_failed_export_preserves_existing_clip_and_orphan_sidecars() {
        let dir = TestDir::new("clipline-library", "bh02-rollback");
        let source = dir.path().join("import.mp4");
        let input = video_only_mp4();
        std::fs::write(&source, &input).unwrap();
        let orphan = dir.path().join("Highlight.mp4");
        for path in clip_sidecar_paths(&orphan) {
            std::fs::write(path, b"unowned sidecar").unwrap();
        }
        let exported = export_clip_file(
            source.clone(),
            0.0,
            0.5,
            Some("Highlight".into()),
            false,
            None,
            dir.path(),
        )
        .unwrap();
        assert_eq!(exported.name, "Highlight_1.mp4");
        let exported_bytes = std::fs::read(&exported.path).unwrap();
        let exported_metadata =
            std::fs::read(clip_metadata_path(Path::new(&exported.path))).unwrap();
        std::fs::write(
            source.with_extension("markers.json"),
            b"invalid source markers",
        )
        .unwrap();
        let error = export_clip_file(
            source.clone(),
            0.5,
            1.5,
            Some("Highlight".into()),
            false,
            None,
            dir.path(),
        )
        .err()
        .unwrap();
        assert!(error.contains("parse sidecar"));
        assert_eq!(std::fs::read(&exported.path).unwrap(), exported_bytes);
        assert_eq!(
            std::fs::read(clip_metadata_path(Path::new(&exported.path))).unwrap(),
            exported_metadata
        );
        assert_eq!(std::fs::read(&source).unwrap(), input);
        assert_eq!(
            std::fs::read(source.with_extension("markers.json")).unwrap(),
            b"invalid source markers"
        );
        assert!(!dir.path().join("Highlight_2.mp4").exists());
        for path in clip_sidecar_paths(&orphan) {
            assert_eq!(std::fs::read(path).unwrap(), b"unowned sidecar");
        }
        assert!(std::fs::read_dir(dir.path()).unwrap().all(|entry| entry
            .unwrap()
            .path()
            .extension()
            .is_none_or(|ext| ext != "tmp")));
    }

    #[test]
    fn security_scan_ungrouped_exports_are_managed_and_imports_are_preserved() {
        let dir = TestDir::new("clipline-library", "trim-ownership");
        let source = dir.path().join("import.mp4");
        std::fs::write(&source, super::super::test_support::two_real_opus_audio_mp4()).unwrap();
        for title in [None, Some("Highlight".to_string())] {
            let exported = export_clip_file(source.clone(), 0.0, 0.5, title, true, None, dir.path()).unwrap();
            let path = Path::new(&exported.path);
            assert!(clipline_storage::is_clip_owned(path));
            assert_eq!(read_clip_metadata(path).unwrap().kind.as_deref(), Some("trim"));
            assert!(clipline_storage::storage_status(dir.path(), Some(0)).unwrap().total_bytes > 0);
            clipline_storage::delete_all_managed_media(dir.path()).unwrap();
            assert!(!path.exists());
            assert!(!path.with_extension("markers.json").exists());
            assert!(!clip_metadata_path(path).exists());
            assert!(source.exists());
        }
    }
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
                selected_audio_track_ids: None,
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
                selected_audio_track_ids: None,
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
                selected_audio_track_ids: None,
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
                selected_audio_track_ids: None,
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
                selected_audio_track_ids: None,
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
                selected_audio_track_ids: None,
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
                selected_audio_track_ids: None,
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
                selected_audio_track_ids: None,
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
                selected_audio_track_ids: Some(vec!["microphone".into()]),
                plays: Vec::new(),
                markers: Vec::new(),
            };

            assert!(has_marker_sidecar_content(&markers));
            let cropped = crop_markers(&markers, 3.0, 7.0);

            assert_eq!(cropped.audio_tracks, tracks);
            assert_eq!(
                cropped.selected_audio_track_ids,
                Some(vec!["microphone".into()])
            );
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
