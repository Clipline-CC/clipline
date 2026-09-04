use super::*;

#[tauri::command]
pub async fn list_clips<R: Runtime>(
    app: AppHandle<R>,
    settings: tauri::State<'_, StorageSettings>,
) -> Result<LocalClipScan, String> {
    let dir = settings.clips_dir()?;
    let retry_root = dir.clone();
    let enrichment_app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = crate::osu_api::retry_pending_enrichment(&enrichment_app, retry_root).await
        {
            tracing::warn!(event = "library_osu_enrichment_retry_failed", error = %e);
        }
    });
    let scope_root = dir.clone();
    let scan = tauri::async_runtime::spawn_blocking(move || list_clips_from_dir(dir))
        .await
        .map_err(|e| format!("list clips task: {e}"))??;
    let canonical_scope_root = canonical_media_root(&scope_root)?;
    for clip in &scan.clips {
        allow_local_clip_asset_from_canonical_root(
            &app,
            &canonical_scope_root,
            Path::new(&clip.path),
        )?;
    }
    Ok(scan)
}

pub(crate) fn list_clips_from_dir(dir: PathBuf) -> Result<LocalClipScan, String> {
    groups::recover_group_order_transaction(&dir)?;
    list_clips_from_dir_with_child_reader(dir, push_clips_from)
}

pub(crate) fn list_clips_from_dir_with_child_reader(
    dir: PathBuf,
    mut read_child: impl FnMut(&Path, Option<String>, &mut Vec<ClipInfo>) -> Result<(), String>,
) -> Result<LocalClipScan, String> {
    let mut clips = Vec::new();
    let mut warnings = Vec::new();
    push_clips_from(&dir, None, &mut clips)?;
    for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warnings.push(format!("Skipped an unreadable Library entry: {error}"));
                continue;
            }
        };
        let session = entry.file_name().to_string_lossy().into_owned();
        let is_dir = match entry.metadata() {
            Ok(metadata) => metadata.is_dir(),
            Err(error) => {
                warnings.push(format!(
                    "Skipped Library entry \"{session}\" because its metadata is unavailable: {error}"
                ));
                continue;
            }
        };
        if is_dir {
            if let Err(error) = read_child(&entry.path(), Some(session.clone()), &mut clips) {
                warnings.push(format!(
                    "Skipped Library session \"{session}\" because it could not be read: {error}"
                ));
            }
        }
    }
    for warning in &warnings {
        tracing::warn!(event = "library_scan_partial", message = %warning);
    }
    clips.sort_by_key(|c| std::cmp::Reverse(c.modified_unix));
    Ok(LocalClipScan { clips, warnings })
}

pub(crate) fn push_clips_from(
    dir: &Path,
    session: Option<String>,
    clips: &mut Vec<ClipInfo>,
) -> Result<(), String> {
    // One game tag per session folder, shared by every clip inside it.
    let session_game: Option<ClipGame> = std::fs::read_to_string(dir.join("clipline-session.json"))
        .ok()
        .and_then(|json| serde_json::from_str::<ClipGame>(&json).ok());
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("mp4") {
            continue;
        }
        let meta = entry.metadata().ok();
        if meta.as_ref().is_some_and(|m| !m.is_file()) {
            continue;
        }
        let modified_unix = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let size_mb = meta
            .map(|m| m.len() as f64 / (1024.0 * 1024.0))
            .unwrap_or(0.0);
        let raw_markers = util::read_markers_raw(&path).map(filter_review_markers);
        let clip_metadata = read_clip_metadata(&path).unwrap_or_default();
        let duration_s = raw_markers
            .as_ref()
            .map(|markers| markers.duration_s)
            .filter(|duration| duration.is_finite() && *duration >= 0.0);
        let markers = util::markers_with_inferred_audio_tracks(&path, raw_markers);
        let title = clip_title_from_metadata(&clip_metadata);
        let kind = clip_kind_from_metadata(&path, &clip_metadata).to_string();
        let group = clip_metadata.group.clone();
        let source_group = clip_metadata.source_group.clone();
        let source_group_fingerprint = clip_metadata.source_group_fingerprint.clone();
        // Prefer the session sidecar; fall back to the game named in markers
        // so clips recorded before session tagging still show an icon.
        let game = session_game
            .clone()
            .or_else(|| game_from_markers(markers.as_ref()));
        clips.push(ClipInfo {
            path: path.display().to_string(),
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            title,
            kind,
            favorite: is_favorite_clip(&path),
            session: session.clone(),
            size_mb,
            modified_unix,
            duration_s,
            markers,
            game,
            group,
            source_group,
            source_group_fingerprint,
        });
    }
    Ok(())
}

/// Fall back to the game named in a clip's markers when its session folder has
/// no game sidecar (clips recorded before session tagging existed). Only games
/// with a matching plugin resolve to an icon in the UI.
pub(crate) fn game_from_markers(markers: Option<&ClipMarkers>) -> Option<ClipGame> {
    let game_id = markers?.markers.first()?.event.game_id;
    let plugin_id = crate::game_plugins::plugin_id_for_game_id(game_id);
    let name = crate::game_plugins::all()
        .iter()
        .find(|plugin| plugin.id() == plugin_id)
        .map(|plugin| plugin.manifest.name.clone())?;
    Some(ClipGame {
        id: plugin_id.to_string(),
        name,
        queue: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_test_utils::TestDir;
    use clipline_events::ClipMarkers;
        #[test]
        fn list_clips_uses_marker_duration_without_parsing_mp4() {
            let dir = TestDir::new("clipline-library", "list-marker-duration");
            let media = dir.path().join("media");
            let clip = media.join("broken-but-listed.mp4");
            touch_mp4(&clip);
            let markers = ClipMarkers {
                bookmarks: Vec::new(),
                recording_start_s: 0.0,
                duration_s: 42.5,
                player_summary: None,
                audio_tracks: Vec::new(),
                plays: Vec::new(),
                markers: vec![marker(1.0)],
            };
            std::fs::write(
                clip.with_extension("markers.json"),
                serde_json::to_string(&markers).unwrap(),
            )
            .unwrap();

            let clips = list_clips_from_dir(media).unwrap().clips;

            assert_eq!(clips.len(), 1);
            assert_eq!(clips[0].duration_s, Some(42.5));
            assert_eq!(clips[0].markers.as_ref().unwrap().markers.len(), 1);
        }
        #[test]
        fn list_clips_preserves_optional_league_queue_metadata() {
            let dir = TestDir::new("clipline-library", "league-queue-metadata");
            let media = dir.path().join("media");
            let session = media.join("ranked-match");
            touch_mp4(&session.join("session_1.mp4"));
            std::fs::write(
                session.join("clipline-session.json"),
                r#"{
                  "id":"league_of_legends",
                  "name":"League of Legends",
                  "queue":{"id":420,"category":"ranked-solo-duo","label":"Ranked Solo/Duo"}
                }"#,
            )
            .unwrap();

            let clips = list_clips_from_dir(media).unwrap().clips;
            let queue = clips[0]
                .game
                .as_ref()
                .and_then(|game| game.queue.as_ref())
                .expect("queue metadata should survive the library scan");
            assert_eq!(queue.id, 420);
            assert_eq!(queue.label, "Ranked Solo/Duo");
        }
        #[test]
        fn local_library_scan_keeps_readable_sessions_and_warns_about_denied_children() {
            let dir = TestDir::new("clipline-library", "partial-session-scan");
            let media = dir.path().join("media");
            let readable = media.join("readable-session");
            let denied = media.join("denied-session");
            touch_mp4(&readable.join("kept.mp4"));
            std::fs::create_dir_all(&denied).unwrap();

            let result = list_clips_from_dir_with_child_reader(media, |path, session, clips| {
                if path.ends_with("denied-session") {
                    Err("access denied by test".into())
                } else {
                    push_clips_from(path, session, clips)
                }
            })
            .unwrap();

            assert_eq!(result.clips.len(), 1);
            assert_eq!(result.clips[0].name, "kept.mp4");
            assert_eq!(result.warnings.len(), 1);
            assert!(result.warnings[0].contains("denied-session"));
            assert!(result.warnings[0].contains("access denied by test"));
        }
        #[test]
        fn local_library_scan_keeps_root_failures_fatal() {
            let dir = TestDir::new("clipline-library", "missing-root-scan");
            let missing = dir.path().join("missing");

            assert!(list_clips_from_dir(missing).is_err());
        }
        #[test]
        fn list_clips_infers_audio_tracks_for_legacy_multitrack_clip() {
            let dir = TestDir::new("clipline-library", "list-infer-audio-tracks");
            let clip = dir.path().join("legacy.mp4");
            std::fs::write(&clip, two_real_opus_audio_mp4()).unwrap();

            let mut clips = Vec::new();
            push_clips_from(dir.path(), None, &mut clips).unwrap();

            assert_eq!(clips[0].duration_s, None);
            let tracks = &clips[0]
                .markers
                .as_ref()
                .expect("legacy clip gets inferred audio metadata")
                .audio_tracks;
            assert_eq!(tracks.len(), 2);
            assert_eq!(tracks[0].id, "audio:0");
            assert_eq!(tracks[0].track_index, 0);
            assert_eq!(tracks[0].label, "Audio Track 1");
            assert_eq!(tracks[1].id, "audio:1");
            assert_eq!(tracks[1].track_index, 1);
            assert_eq!(tracks[1].label, "Audio Track 2");
        }
        #[test]
        fn list_clips_does_not_sweep_emptied_session_folders() {
            let dir = TestDir::new("clipline-library", "list-no-sweep-empty-session");
            let media = dir.path().join("media");
            let leftover = media.join("2026-06-13 02-31");
            std::fs::create_dir_all(&leftover).unwrap();
            std::fs::write(leftover.join("clipline-session.json"), b"{}").unwrap();
            let keep = media.join("2026-06-13 03-00").join("clip.mp4");
            touch_mp4(&keep);

            let clips = list_clips_from_dir(media).unwrap().clips;

            assert_eq!(clips.len(), 1);
            assert!(
                leftover.exists(),
                "Library listing is a read path and must not restage or delete session folders"
            );
            assert!(keep.exists());
        }
}
