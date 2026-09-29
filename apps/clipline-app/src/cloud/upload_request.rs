//! Upload request construction, audio-selection payloads, and clip metadata.
use super::*;

pub(crate) struct UploadRequestInput<'a> {
    pub(crate) path: &'a Path,
    pub(crate) meta: &'a std::fs::Metadata,
    pub(crate) file_size_bytes: u64,
    pub(crate) duration_ms: Option<i64>,
    pub(crate) checksum: &'a str,
    pub(crate) visibility: &'a str,
    pub(crate) markers: Option<&'a ClipMarkers>,
    pub(crate) client_clip_id: &'a str,
    pub(crate) title: Option<&'a str>,
}

pub(crate) fn create_upload_request(input: UploadRequestInput<'_>) -> Result<CreateUploadRequest, String> {
    let game = read_clip_game(input.path, input.markers);
    Ok(CreateUploadRequest {
        client_clip_id: Some(input.client_clip_id.to_string()),
        title: upload_title(input.title, input.path),
        description: None,
        game_name: game.as_ref().map(|game| game.name.clone()),
        game_id: game.as_ref().map(|game| game.id.clone()),
        game_executable: None,
        source_type: Some(source_type(input.path)),
        recorded_at: input.meta.modified().ok().map(DateTime::<Utc>::from),
        duration_ms: input.duration_ms,
        file_size_bytes: input.file_size_bytes,
        checksum_sha256: input.checksum.to_string(),
        container: "mp4".to_string(),
        video_codec: None,
        audio_codec: None,
        width: None,
        height: None,
        fps: None,
        visibility: Some(input.visibility.to_string()),
        markers: None,
    })
}

pub(crate) fn upload_title(title: Option<&str>, path: &Path) -> String {
    title
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| crate::library::clip_title_for_path(path))
}

pub(crate) fn normalize_upload_description(description: Option<&str>) -> Option<String> {
    description
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UploadAudioSelectionPlan {
    Original,
    Remux(Vec<u32>),
    Mix(Vec<u32>),
}

pub(crate) struct UploadPayload {
    path: PathBuf,
    owned: bool,
}

impl UploadPayload {
    pub(crate) fn original(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            owned: false,
        }
    }

    pub(crate) fn owned(path: PathBuf) -> Self {
        Self { path, owned: true }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for UploadPayload {
    fn drop(&mut self) {
        if self.owned {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub(crate) async fn upload_payload_for_audio_selection_from_path(
    source_path: &Path,
    markers: Option<&ClipMarkers>,
    selected_audio_track_ids: Option<&[String]>,
) -> Result<UploadPayload, String> {
    let markers_with_audio = selected_audio_track_ids.and_then(|_| {
        crate::util::markers_with_inferred_audio_tracks(source_path, markers.cloned())
    });
    let selection_markers = markers_with_audio.as_ref().or(markers);
    match upload_audio_selection_plan(selection_markers, selected_audio_track_ids)? {
        UploadAudioSelectionPlan::Original => Ok(UploadPayload::original(source_path)),
        UploadAudioSelectionPlan::Remux(selected_indices) => {
            let target = reserve_upload_payload_path(source_path)?;
            let payload = UploadPayload::owned(target.clone());
            let source = source_path.to_path_buf();
            tokio::task::spawn_blocking(move || {
                clipline_mp4::remux_with_selected_audio_tracks_file(
                    &source,
                    &target,
                    &selected_indices,
                )
            })
            .await
            .map_err(|e| format!("audio remux task failed: {e}"))?
            .map_err(|e| e.to_string())?;
            Ok(payload)
        }
        UploadAudioSelectionPlan::Mix(selected_indices) => {
            let target = reserve_upload_payload_path(source_path)?;
            let payload = UploadPayload::owned(target.clone());
            let source = source_path.to_path_buf();
            tokio::task::spawn_blocking(move || {
                clipline_mp4::remux_with_mixed_audio_track_file(&source, &target, &selected_indices)
            })
            .await
            .map_err(|e| format!("audio mix task failed: {e}"))?
            .map_err(|e| e.to_string())?;
            Ok(payload)
        }
    }
}

pub(crate) fn reserve_upload_payload_path(source: &Path) -> Result<PathBuf, String> {
    if source.file_name().is_none() {
        return Err("clip path must include a file name".into());
    }
    let directory = std::env::temp_dir()
        .join("Clipline")
        .join("upload-payloads");
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("create upload payload directory {directory:?}: {error}"))?;
    prune_abandoned_upload_payloads(&directory);
    for _ in 0..128 {
        let suffix = CLOUD_CACHE_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(
            "{UPLOAD_PAYLOAD_PREFIX}{}-{suffix}.mp4.tmp",
            std::process::id()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => {
                drop(file);
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("reserve upload payload: {error}")),
        }
    }
    Err("could not reserve a unique upload payload path".into())
}

pub(crate) fn prune_abandoned_upload_payloads(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_upload_temp = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(UPLOAD_PAYLOAD_PREFIX) && name.ends_with(".tmp"));
        let abandoned = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= UPLOAD_PAYLOAD_MAX_AGE);
        if is_upload_temp && abandoned {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
fn upload_bytes_for_audio_selection(
    source_bytes: Vec<u8>,
    markers: Option<&ClipMarkers>,
    selected_audio_track_ids: Option<&[String]>,
) -> Result<Vec<u8>, String> {
    match upload_audio_selection_plan(markers, selected_audio_track_ids)? {
        UploadAudioSelectionPlan::Original => Ok(source_bytes),
        UploadAudioSelectionPlan::Remux(selected_indices) => {
            clipline_mp4::remux_with_selected_audio_tracks(&source_bytes, &selected_indices)
                .map_err(|e| e.to_string())
        }
        UploadAudioSelectionPlan::Mix(selected_indices) => {
            clipline_mp4::remux_with_mixed_audio_track(&source_bytes, &selected_indices)
                .map_err(|e| e.to_string())
        }
    }
}

pub(crate) fn upload_audio_selection_plan(
    markers: Option<&ClipMarkers>,
    selected_audio_track_ids: Option<&[String]>,
) -> Result<UploadAudioSelectionPlan, String> {
    let Some(selected_audio_track_ids) = selected_audio_track_ids else {
        return Ok(UploadAudioSelectionPlan::Original);
    };
    let tracks = markers.map(|m| m.audio_tracks.as_slice()).unwrap_or(&[]);
    if tracks.is_empty() {
        if selected_audio_track_ids.is_empty() {
            return Ok(UploadAudioSelectionPlan::Remux(Vec::new()));
        }
        return Err("this clip has no selectable audio track metadata".into());
    }

    let selected_indices =
        crate::util::selected_audio_track_indices(markers.unwrap(), selected_audio_track_ids)?;
    if selected_indices.len() > 1 {
        Ok(UploadAudioSelectionPlan::Mix(selected_indices))
    } else {
        Ok(UploadAudioSelectionPlan::Remux(selected_indices))
    }
}

pub(crate) fn read_clip_game(path: &Path, markers: Option<&ClipMarkers>) -> Option<crate::library::ClipGame> {
    path.parent()
        .and_then(|dir| std::fs::read_to_string(dir.join("clipline-session.json")).ok())
        .and_then(|json| serde_json::from_str::<crate::library::ClipGame>(&json).ok())
        .or_else(|| markers.and_then(game_from_markers))
}

pub(crate) fn game_from_markers(markers: &ClipMarkers) -> Option<crate::library::ClipGame> {
    let game_id = markers.markers.first()?.event.game_id;
    let id = crate::game_plugins::plugin_id_for_game_id(game_id);
    Some(crate::library::ClipGame {
        id: id.to_string(),
        name: crate::game_plugins::display_name_for_game_id(game_id).to_string(),
        queue: None,
    })
}

pub(crate) fn clip_duration_ms_file(path: &Path, markers: Option<&ClipMarkers>) -> Option<i64> {
    clipline_mp4::movie_duration_s_file(path)
        .ok()
        .flatten()
        .or_else(|| markers.map(|markers| markers.duration_s))
        .map(|seconds| (seconds * 1000.0).round())
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map(|value| value as i64)
}

pub(crate) fn source_type(path: &Path) -> String {
    crate::library::clip_kind_for_path(path)
}

pub(crate) fn local_clip_id(path: &Path, meta: &std::fs::Metadata, checksum: &str) -> Result<String, String> {
    let canonical = path
        .canonicalize()
        .map_err(|e| format!("resolve clip path: {e}"))?;
    let modified = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    let payload = format!(
        "clipline-local-v1\0{}\0{}\0{}\0{}",
        canonical.display(),
        meta.len(),
        modified,
        checksum
    );
    Ok(format!("clipline-local-{}", sha256_hex(payload.as_bytes())))
}

#[cfg(test)]
mod tests {
    use super::*;
    
    
    
    

    #[test]
    fn source_type_falls_back_to_replay() {
        assert_eq!(source_type(Path::new("clipline-2026-06-16.mp4")), "replay");
        assert_eq!(source_type(Path::new("full-session.mp4")), "replay");
        assert_eq!(source_type(Path::new("ranked-trim.mp4")), "replay");
        assert_eq!(source_type(Path::new("session_1781377615.mp4")), "session");
        assert_eq!(
            source_type(Path::new("clip_1_trim_001000_002000.mp4")),
            "trim"
        );
    }

    #[test]
    fn upload_audio_selection_plan_mixes_multiple_selected_tracks() {
        let markers = audio_markers();
        let selected = vec!["output".to_string(), "microphone".to_string()];

        assert_eq!(
            upload_audio_selection_plan(Some(&markers), Some(&selected)).unwrap(),
            UploadAudioSelectionPlan::Mix(vec![0, 1])
        );
    }

    #[test]
    fn upload_audio_selection_remuxes_only_selected_track() {
        let source = two_audio_mp4();
        let markers = audio_markers();
        let selected = vec!["microphone".to_string()];

        let out =
            upload_bytes_for_audio_selection(source, Some(&markers), Some(&selected)).unwrap();

        assert!(out.windows(6).any(|w| w == b"V00000"));
        assert!(!out.windows(6).any(|w| w == b"A00000"));
        assert!(out.windows(6).any(|w| w == b"B00000"));
    }

    #[test]
    fn upload_audio_selection_rejects_unknown_track_id() {
        let source = two_audio_mp4();
        let markers = audio_markers();
        let selected = vec!["discord".to_string()];

        let err = upload_bytes_for_audio_selection(source, Some(&markers), Some(&selected))
            .expect_err("unknown track");

        assert!(err.contains("unknown audio track"), "{err}");
    }

}