use super::*;

#[derive(serde::Deserialize)]
pub struct SetClipAudioSelectionRequest {
    pub path: String,
    #[serde(default, rename = "audioTrackIds")]
    pub audio_track_ids: Vec<String>,
}

#[tauri::command]
pub fn set_clip_audio_selection(
    request: SetClipAudioSelectionRequest,
    settings: tauri::State<'_, StorageSettings>,
) -> Result<Vec<String>, String> {
    let source = validate_clip_path(&settings, &request.path)?;
    set_clip_audio_selection_file(&source, request.audio_track_ids)
}

pub(crate) fn set_clip_audio_selection_file(
    source: &Path,
    audio_track_ids: Vec<String>,
) -> Result<Vec<String>, String> {
    let _guard = crate::gc::lock_clip_mutations();
    let mut markers = util::markers_with_inferred_audio_tracks(source, util::read_markers_raw(source))
        .ok_or_else(|| "this clip has no selectable audio tracks".to_string())?;
    if markers.audio_tracks.is_empty() {
        return Err("this clip has no selectable audio tracks".into());
    }
    if !markers.duration_s.is_finite() || markers.duration_s <= 0.0 {
        markers.duration_s = clipline_mp4::movie_duration_s_file(source)
            .map_err(|error| format!("inspect clip duration: {error}"))?
            .filter(|duration| duration.is_finite() && *duration > 0.0)
            .ok_or_else(|| "this clip has no valid duration".to_string())?;
    }
    let _ = util::selected_audio_track_indices(&markers, &audio_track_ids)?;
    markers.selected_audio_track_ids = Some(audio_track_ids.clone());
    write_marker_sidecar_atomically(source, &markers)?;
    Ok(audio_track_ids)
}

fn write_marker_sidecar_atomically(source: &Path, markers: &ClipMarkers) -> Result<(), String> {
    let target = source.with_extension("markers.json");
    let bytes = serde_json::to_vec_pretty(markers)
        .map_err(|error| format!("serialize audio selection: {error}"))?;
    let tmp = crate::settings::persistence::sibling_tmp_path(&target)?;
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|error| format!("create audio selection sidecar: {error}"))?;
        file.write_all(&bytes)
            .map_err(|error| format!("write audio selection sidecar: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("sync audio selection sidecar: {error}"))?;
        crate::windows::replace_file(&tmp, &target)
            .map_err(|error| format!("publish audio selection sidecar: {error}"))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_events::ClipAudioTrack;
    use clipline_test_utils::TestDir;

    fn write_markers(source: &Path) {
        std::fs::write(source, b"mp4").unwrap();
        let markers = ClipMarkers {
            recording_start_s: 0.0,
            duration_s: 10.0,
            player_summary: None,
            audio_tracks: vec![
                ClipAudioTrack {
                    id: "playback:0".into(),
                    track_index: 0,
                    label: "Game".into(),
                    kind: Some("playback_endpoint".into()),
                },
                ClipAudioTrack {
                    id: "playback:1".into(),
                    track_index: 1,
                    label: "Chat".into(),
                    kind: Some("playback_endpoint".into()),
                },
            ],
            selected_audio_track_ids: None,
            plays: Vec::new(),
            markers: Vec::new(),
            bookmarks: Vec::new(),
        };
        std::fs::write(
            source.with_extension("markers.json"),
            serde_json::to_vec_pretty(&markers).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn audio_selection_persists_subset_and_explicit_mute() {
        let dir = TestDir::new("clipline-library", "audio-selection");
        let source = dir.path().join("clip.mp4");
        write_markers(&source);

        set_clip_audio_selection_file(&source, vec!["playback:1".into()]).unwrap();
        assert_eq!(
            util::read_markers_raw(&source).unwrap().selected_audio_track_ids,
            Some(vec!["playback:1".into()])
        );

        set_clip_audio_selection_file(&source, Vec::new()).unwrap();
        assert_eq!(
            util::read_markers_raw(&source).unwrap().selected_audio_track_ids,
            Some(Vec::new())
        );
    }

    #[test]
    fn invalid_audio_selection_does_not_replace_existing_sidecar() {
        let dir = TestDir::new("clipline-library", "invalid-audio-selection");
        let source = dir.path().join("clip.mp4");
        write_markers(&source);
        let before = std::fs::read(source.with_extension("markers.json")).unwrap();

        assert!(set_clip_audio_selection_file(&source, vec!["missing".into()]).is_err());
        assert_eq!(
            std::fs::read(source.with_extension("markers.json")).unwrap(),
            before
        );
    }
}
