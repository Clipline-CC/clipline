use super::*;

#[derive(serde::Deserialize)]
pub struct PrepareClipAudioSidecarsRequest {
    pub path: String,
    #[serde(default, rename = "audioTrackIds")]
    pub audio_track_ids: Vec<String>,
    #[serde(default, rename = "protectedPreviewPaths")]
    pub protected_preview_paths: Vec<String>,
}

#[derive(serde::Serialize, Clone, Debug, PartialEq, Eq)]
pub struct PreparedClipAudioSidecar {
    #[serde(rename = "audioTrackId")]
    pub audio_track_id: String,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResolvedAudioTrackSidecar {
    pub(crate) audio_track_id: String,
    pub(crate) audio_stream_index: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AudioTrackSidecarOutput {
    pub(crate) audio_track_id: String,
    pub(crate) audio_stream_index: u32,
    pub(crate) final_path: PathBuf,
    pub(crate) tmp_path: PathBuf,
}

#[derive(Debug, Default)]
pub(crate) struct PublishedAudioSidecars {
    pub(crate) created_finals: Vec<PathBuf>,
    pub(crate) committed: bool,
}

impl PublishedAudioSidecars {
    pub(crate) fn record_created(&mut self, path: PathBuf) {
        self.created_finals.push(path);
    }

    pub(crate) fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for PublishedAudioSidecars {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        cleanup_created_audio_sidecar_finals(&self.created_finals);
    }
}

#[derive(Debug)]
pub(crate) struct PreparedAudioSidecarBatch {
    pub(crate) sidecars: Vec<PreparedClipAudioSidecar>,
    publication: Option<PublishedAudioSidecars>,
}

#[tauri::command]
pub async fn prepare_clip_audio_sidecars<R: Runtime>(
    app: AppHandle<R>,
    request: PrepareClipAudioSidecarsRequest,
    settings: tauri::State<'_, StorageSettings>,
) -> Result<Vec<PreparedClipAudioSidecar>, String> {
    let source = validate_clip_path(&settings, &request.path)?;
    let protected_preview_paths: Vec<PathBuf> = request
        .protected_preview_paths
        .into_iter()
        .map(PathBuf::from)
        .collect();
    let sidecars = tauri::async_runtime::spawn_blocking(move || {
        prepare_clip_audio_sidecars_file(source, request.audio_track_ids, protected_preview_paths)
    })
    .await
    .map_err(|e| format!("audio sidecar task: {e}"))??;
    finalize_prepared_audio_sidecars(sidecars, |sidecar| {
        allow_audio_preview_asset(&app, Path::new(&sidecar.path))
    })
}

pub(crate) fn allow_audio_preview_asset<R: Runtime>(app: &AppHandle<R>, preview: &Path) -> Result<(), String> {
    let preview_dir = crate::settings::audio_preview_cache_dir();
    if !preview.starts_with(&preview_dir) {
        return Ok(());
    }
    let canonical_dir = std::fs::canonicalize(&preview_dir)
        .map_err(|e| format!("canonicalize audio preview cache {preview_dir:?}: {e}"))?;
    let canonical_preview = std::fs::canonicalize(preview)
        .map_err(|e| format!("canonicalize audio preview {preview:?}: {e}"))?;
    if !canonical_preview.starts_with(&canonical_dir) {
        return Err(format!(
            "audio preview {canonical_preview:?} escaped cache {canonical_dir:?}"
        ));
    }
    if !canonical_preview
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("mp4"))
    {
        return Err(format!("audio preview {canonical_preview:?} is not an MP4"));
    }

    let preview = canonical_preview.as_path();
    app.asset_protocol_scope()
        .allow_file(preview)
        .map_err(|e| format!("scope audio preview {canonical_preview:?} for playback: {e}"))
}

pub(crate) fn prepare_clip_audio_sidecars_file(
    source: PathBuf,
    selected_audio_track_ids: Vec<String>,
    protected_preview_paths: Vec<PathBuf>,
) -> Result<PreparedAudioSidecarBatch, String> {
    prepare_clip_audio_sidecars_file_with_extractor(
        source,
        selected_audio_track_ids,
        protected_preview_paths
            .into_iter()
            .map(|path| path.display().to_string())
            .collect(),
        crate::settings::audio_preview_cache_dir(),
        extract_audio_sidecars_with_ffmpeg,
    )
}

pub(crate) fn prepare_clip_audio_sidecars_file_with_extractor(
    source: PathBuf,
    selected_audio_track_ids: Vec<String>,
    protected_preview_paths: Vec<String>,
    preview_dir: PathBuf,
    extract_audio_sidecars: impl FnMut(&Path, &[AudioTrackSidecarOutput]) -> Result<(), String>,
) -> Result<PreparedAudioSidecarBatch, String> {
    prepare_clip_audio_sidecars_file_with_extractor_and_limits(
        source,
        selected_audio_track_ids,
        protected_preview_paths,
        preview_dir,
        AUDIO_PREVIEW_CACHE_MAX_BYTES,
        extract_audio_sidecars,
    )
}

pub(crate) fn prepare_clip_audio_sidecars_file_with_extractor_and_limits(
    source: PathBuf,
    selected_audio_track_ids: Vec<String>,
    protected_preview_paths: Vec<String>,
    preview_dir: PathBuf,
    max_cache_bytes: u64,
    mut extract_audio_sidecars: impl FnMut(&Path, &[AudioTrackSidecarOutput]) -> Result<(), String>,
) -> Result<PreparedAudioSidecarBatch, String> {
    let resolved_tracks = resolve_audio_sidecar_tracks(&source, &selected_audio_track_ids)?;
    let source_meta = std::fs::metadata(&source).map_err(|e| format!("read clip metadata: {e}"))?;
    std::fs::create_dir_all(&preview_dir)
        .map_err(|e| format!("create audio preview cache: {e}"))?;

    let currently_active: Vec<PathBuf> = protected_preview_paths
        .into_iter()
        .map(PathBuf::from)
        .collect();
    let requested_final_paths: Vec<PathBuf> = resolved_tracks
        .iter()
        .map(|track| {
            audio_track_sidecar_path(&preview_dir, &source, &source_meta, &track.audio_track_id)
        })
        .collect();
    let protected_before_lookup = [
        currently_active.as_slice(),
        requested_final_paths.as_slice(),
    ]
    .concat();
    prune_audio_preview_cache_logged_with_limit(
        &preview_dir,
        &protected_before_lookup,
        max_cache_bytes,
    );

    let mut ordered = Vec::with_capacity(resolved_tracks.len());
    let mut missing_outputs = Vec::new();
    for (track, final_path) in resolved_tracks.iter().zip(requested_final_paths.iter()) {
        if final_path.exists() {
            match validate_audio_sidecar_file(final_path) {
                Ok(()) => {
                    if let Err(error) = touch_audio_preview(final_path) {
                        tracing::warn!(event = "audio_sidecar_cleanup_failed", error = %error);
                    }
                    ordered.push(Some(PreparedClipAudioSidecar {
                        audio_track_id: track.audio_track_id.clone(),
                        path: final_path.display().to_string(),
                    }));
                    continue;
                }
                Err(error) => {
                    tracing::warn!(event = "audio_sidecar_cleanup_failed", error = %error);
                    let _ = std::fs::remove_file(final_path);
                }
            }
        }

        missing_outputs.push(AudioTrackSidecarOutput {
            audio_track_id: track.audio_track_id.clone(),
            audio_stream_index: track.audio_stream_index,
            final_path: final_path.clone(),
            tmp_path: cached_export_tmp_path(final_path)?,
        });
        ordered.push(None);
    }

    let mut publication = None;

    if !missing_outputs.is_empty() {
        for output in &missing_outputs {
            let _ = std::fs::remove_file(&output.tmp_path);
        }
        if let Err(error) = extract_audio_sidecars(&source, &missing_outputs) {
            cleanup_audio_sidecar_temps(&missing_outputs);
            return Err(error);
        }
        publication = Some(validate_and_publish_audio_sidecars(&missing_outputs)?);
    }

    for ((prepared, track), final_path) in ordered
        .iter_mut()
        .zip(resolved_tracks.iter())
        .zip(requested_final_paths.iter())
    {
        if prepared.is_some() {
            continue;
        }
        validate_audio_sidecar_file(final_path)?;
        *prepared = Some(PreparedClipAudioSidecar {
            audio_track_id: track.audio_track_id.clone(),
            path: final_path.display().to_string(),
        });
    }
    let ordered = ordered
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| "audio sidecar preparation left an unresolved track".to_string())?;
    let protected_after: Vec<PathBuf> = ordered
        .iter()
        .map(|sidecar| PathBuf::from(&sidecar.path))
        .collect();
    let protected = [currently_active.as_slice(), protected_after.as_slice()].concat();
    prune_audio_preview_cache_logged_with_limit(&preview_dir, &protected, max_cache_bytes);
    Ok(PreparedAudioSidecarBatch {
        sidecars: ordered,
        publication,
    })
}

pub(crate) fn finalize_prepared_audio_sidecars(
    mut batch: PreparedAudioSidecarBatch,
    mut allow_audio_sidecar: impl FnMut(&PreparedClipAudioSidecar) -> Result<(), String>,
) -> Result<Vec<PreparedClipAudioSidecar>, String> {
    for sidecar in &batch.sidecars {
        allow_audio_sidecar(sidecar)?;
    }
    if let Some(publication) = batch.publication.take() {
        publication.commit();
    }
    Ok(batch.sidecars)
}

pub(crate) fn prune_audio_preview_cache_logged_with_limit(
    preview_dir: &Path,
    protected: &[PathBuf],
    max_cache_bytes: u64,
) {
    if let Err(error) = prune_audio_preview_cache(preview_dir, protected, max_cache_bytes) {
        tracing::warn!(event = "audio_preview_cache_prune_failed", error = %error);
    }
}

pub(crate) fn resolve_audio_sidecar_tracks(
    source: &Path,
    selected_audio_track_ids: &[String],
) -> Result<Vec<ResolvedAudioTrackSidecar>, String> {
    if selected_audio_track_ids.is_empty() {
        return Err("audio track selection must not be empty".into());
    }
    let Some(markers) =
        util::markers_with_inferred_audio_tracks(source, util::read_markers_raw(source))
    else {
        return Err("this clip has no selectable audio track metadata".into());
    };
    if markers.audio_tracks.is_empty() {
        return Err("this clip has no selectable audio track metadata".into());
    }
    let _ = util::selected_audio_track_indices(&markers, selected_audio_track_ids)?;
    let selected_id_set: std::collections::BTreeSet<&str> = selected_audio_track_ids
        .iter()
        .map(String::as_str)
        .collect();
    Ok(markers
        .audio_tracks
        .iter()
        .filter(|track| selected_id_set.contains(track.id.as_str()))
        .map(|track| ResolvedAudioTrackSidecar {
            audio_track_id: track.id.clone(),
            audio_stream_index: track.track_index,
        })
        .collect())
}

pub(crate) fn audio_track_sidecar_path(
    preview_dir: &Path,
    source: &Path,
    meta: &std::fs::Metadata,
    audio_track_id: &str,
) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    "audio-track-sidecar-v1".hash(&mut hasher);
    source.display().to_string().hash(&mut hasher);
    meta.len().hash(&mut hasher);
    meta.modified().ok().hash(&mut hasher);
    audio_track_id.hash(&mut hasher);
    preview_dir.join(format!("audio-preview-{:016x}.mp4", hasher.finish()))
}

pub(crate) fn validate_audio_sidecar_file(path: &Path) -> Result<(), String> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("read audio sidecar metadata {path:?}: {error}"))?;
    if metadata.len() == 0 {
        return Err(format!("audio sidecar {path:?} was empty"));
    }
    let counts = clipline_mp4::media_track_counts_file(path)
        .map_err(|error| format!("inspect audio sidecar {path:?}: {error}"))?;
    if counts != (MediaTrackCounts { video: 0, audio: 1 }) {
        return Err(format!(
            "audio sidecar {path:?} had unexpected tracks: video={}, audio={}",
            counts.video, counts.audio
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_test_utils::TestDir;
    use clipline_events::{ClipAudioTrack, ClipMarkers};
        #[test]
        fn selected_audio_track_indices_follow_sidecar_order_and_reject_unknown_ids() {
            let markers = ClipMarkers {
                bookmarks: Vec::new(),
                recording_start_s: 0.0,
                duration_s: 10.0,
                player_summary: None,
                audio_tracks: vec![
                    ClipAudioTrack {
                        id: "output".into(),
                        track_index: 0,
                        label: "Output Audio".into(),
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
                markers: Vec::new(),
            };

            assert_eq!(
                util::selected_audio_track_indices(&markers, &["microphone".into()]).unwrap(),
                vec![1]
            );
            assert_eq!(
                util::selected_audio_track_indices(&markers, &["microphone".into(), "output".into()])
                    .unwrap(),
                vec![0, 1]
            );

            let err = util::selected_audio_track_indices(&markers, &["discord".into()]).unwrap_err();
            assert!(err.contains("unknown audio track"), "{err}");
        }
        #[test]
        fn audio_sidecar_uncached_tracks_extract_once_and_return_marker_ordered_paths() {
            let dir = TestDir::new("clipline-library", "audio-sidecar-ordered");
            let source = dir.path().join("clip.mp4");
            std::fs::write(&source, two_real_opus_audio_mp4()).unwrap();
            write_audio_track_markers(
                &source,
                vec![
                    ("microphone", 1, "Microphone"),
                    ("output", 0, "Output Audio"),
                ],
            );
            let preview_dir = dir.path().join("previews");
            let calls = std::cell::RefCell::new(Vec::<Vec<(u32, PathBuf, PathBuf)>>::new());

            let sidecars = finalize_prepared_audio_sidecars(
                prepare_clip_audio_sidecars_file_with_extractor(
                    source.clone(),
                    vec!["output".into(), "microphone".into()],
                    Vec::new(),
                    preview_dir.clone(),
                    |input, outputs| {
                        assert_eq!(input, source.as_path());
                        calls.borrow_mut().push(
                            outputs
                                .iter()
                                .map(|output| {
                                    (
                                        output.audio_stream_index,
                                        output.final_path.clone(),
                                        output.tmp_path.clone(),
                                    )
                                })
                                .collect(),
                        );
                        for output in outputs {
                            let bytes = audio_only_opus_mp4_for_stream(output.audio_stream_index);
                            std::fs::write(&output.tmp_path, bytes).unwrap();
                        }
                        Ok(())
                    },
                )
                .expect("uncached sidecars should succeed"),
                |_| Ok(()),
            )
            .expect("successful sidecars should commit");

            assert_eq!(calls.borrow().len(), 1);
            assert_eq!(sidecars.len(), 2);
            assert_eq!(sidecars[0].audio_track_id, "microphone");
            assert_eq!(sidecars[1].audio_track_id, "output");
            assert_eq!(
                calls.borrow()[0]
                    .iter()
                    .map(|(index, _, _)| *index)
                    .collect::<Vec<_>>(),
                vec![1, 0]
            );
            assert!(Path::new(&sidecars[0].path).exists());
            assert!(Path::new(&sidecars[1].path).exists());
        }
        #[test]
        fn audio_sidecar_outputs_validate_as_audio_only_and_smaller_than_source() {
            let dir = TestDir::new("clipline-library", "audio-sidecar-audio-only");
            let source = dir.path().join("clip.mp4");
            std::fs::write(&source, two_real_opus_audio_mp4()).unwrap();
            write_audio_track_markers(
                &source,
                vec![
                    ("output", 0, "Output Audio"),
                    ("microphone", 1, "Microphone"),
                ],
            );

            let sidecars = finalize_prepared_audio_sidecars(
                prepare_clip_audio_sidecars_file_with_extractor(
                    source.clone(),
                    vec!["output".into(), "microphone".into()],
                    Vec::new(),
                    dir.path().join("previews"),
                    |_, outputs| {
                        for output in outputs {
                            let bytes = audio_only_opus_mp4_for_stream(output.audio_stream_index);
                            std::fs::write(&output.tmp_path, bytes).unwrap();
                        }
                        Ok(())
                    },
                )
                .unwrap(),
                |_| Ok(()),
            )
            .unwrap();

            let source_len = std::fs::metadata(&source).unwrap().len();
            for sidecar in sidecars {
                let bytes = std::fs::read(&sidecar.path).unwrap();
                assert_eq!(
                    clipline_mp4::media_track_counts(&bytes).unwrap(),
                    clipline_mp4::MediaTrackCounts { video: 0, audio: 1 }
                );
                assert!(std::fs::metadata(&sidecar.path).unwrap().len() < source_len);
            }
        }
        #[test]
        fn audio_sidecar_reuses_existing_tracks_and_extracts_only_missing_track() {
            let dir = TestDir::new("clipline-library", "audio-sidecar-reuse");
            let source = dir.path().join("clip.mp4");
            std::fs::write(&source, two_real_opus_audio_mp4()).unwrap();
            write_audio_track_markers(
                &source,
                vec![
                    ("output", 0, "Output Audio"),
                    ("microphone", 1, "Microphone"),
                    ("discord", 1, "Discord"),
                ],
            );
            let preview_dir = dir.path().join("previews");
            let calls = std::cell::RefCell::new(Vec::<Vec<u32>>::new());

            let first = finalize_prepared_audio_sidecars(
                prepare_clip_audio_sidecars_file_with_extractor(
                    source.clone(),
                    vec!["output".into()],
                    Vec::new(),
                    preview_dir.clone(),
                    |_, outputs| {
                        calls.borrow_mut().push(
                            outputs
                                .iter()
                                .map(|output| output.audio_stream_index)
                                .collect(),
                        );
                        for output in outputs {
                            let bytes = audio_only_opus_mp4_for_stream(output.audio_stream_index);
                            std::fs::write(&output.tmp_path, bytes).unwrap();
                        }
                        Ok(())
                    },
                )
                .unwrap(),
                |_| Ok(()),
            )
            .unwrap();

            let second = finalize_prepared_audio_sidecars(
                prepare_clip_audio_sidecars_file_with_extractor(
                    source,
                    vec!["output".into(), "microphone".into()],
                    Vec::new(),
                    preview_dir,
                    |_, outputs| {
                        calls.borrow_mut().push(
                            outputs
                                .iter()
                                .map(|output| output.audio_stream_index)
                                .collect(),
                        );
                        for output in outputs {
                            let bytes = audio_only_opus_mp4_for_stream(output.audio_stream_index);
                            std::fs::write(&output.tmp_path, bytes).unwrap();
                        }
                        Ok(())
                    },
                )
                .unwrap(),
                |_| Ok(()),
            )
            .unwrap();

            assert_eq!(&*calls.borrow(), &[vec![0], vec![1]]);
            assert_eq!(first[0].path, second[0].path);
        }
        #[test]
        fn audio_sidecar_key_is_per_track_not_selection_combination() {
            let dir = TestDir::new("clipline-library", "audio-sidecar-key");
            let source = dir.path().join("clip.mp4");
            std::fs::write(&source, two_real_opus_audio_mp4()).unwrap();
            write_audio_track_markers(
                &source,
                vec![
                    ("output", 0, "Output Audio"),
                    ("microphone", 1, "Microphone"),
                ],
            );
            let preview_dir = dir.path().join("previews");
            let meta = std::fs::metadata(&source).unwrap();

            let output_only = audio_track_sidecar_path(&preview_dir, &source, &meta, "output");
            let output_with_other = audio_track_sidecar_path(&preview_dir, &source, &meta, "output");
            let mic = audio_track_sidecar_path(&preview_dir, &source, &meta, "microphone");

            assert_eq!(output_only, output_with_other);
            assert_ne!(output_only, mic);
        }
        #[test]
        fn audio_sidecar_prune_protects_active_and_returned_paths() {
            let dir = TestDir::new("clipline-library", "audio-sidecar-prune-protect");
            let source = dir.path().join("clip.mp4");
            std::fs::write(&source, two_real_opus_audio_mp4()).unwrap();
            write_audio_track_markers(
                &source,
                vec![
                    ("output", 0, "Output Audio"),
                    ("microphone", 1, "Microphone"),
                ],
            );
            let preview_dir = dir.path().join("previews");
            std::fs::create_dir_all(&preview_dir).unwrap();
            let active = preview_dir.join("audio-preview-active.mp4");
            let stale = preview_dir.join("audio-preview-stale.mp4");
            std::fs::write(&active, [0_u8; 40]).unwrap();
            std::fs::write(&stale, [0_u8; 40]).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&stale)
                .unwrap()
                .set_modified(std::time::UNIX_EPOCH)
                .unwrap();

            let sidecars = finalize_prepared_audio_sidecars(
                prepare_clip_audio_sidecars_file_with_extractor_and_limits(
                    source,
                    vec!["output".into(), "microphone".into()],
                    vec![active.display().to_string()],
                    preview_dir.clone(),
                    120,
                    |_, outputs| {
                        for output in outputs {
                            let bytes = audio_only_opus_mp4_for_stream(output.audio_stream_index);
                            std::fs::write(&output.tmp_path, bytes).unwrap();
                        }
                        Ok(())
                    },
                )
                .unwrap(),
                |_| Ok(()),
            )
            .unwrap();

            assert!(
                active.exists(),
                "frontend-protected active sidecar must survive"
            );
            assert!(
                !stale.exists(),
                "unprotected stale cache entry should be pruned"
            );
            for sidecar in sidecars {
                assert!(
                    Path::new(&sidecar.path).exists(),
                    "returned sidecar must survive prune"
                );
            }
        }
        #[test]
        fn audio_sidecar_requested_cache_hit_survives_initial_prune_without_extraction() {
            let dir = TestDir::new("clipline-library", "audio-sidecar-requested-hit");
            let source = dir.path().join("clip.mp4");
            std::fs::write(&source, two_real_opus_audio_mp4()).unwrap();
            write_audio_track_markers(&source, vec![("output", 0, "Output Audio")]);

            let preview_dir = dir.path().join("previews");
            std::fs::create_dir_all(&preview_dir).unwrap();
            let meta = std::fs::metadata(&source).unwrap();
            let requested_hit = audio_track_sidecar_path(&preview_dir, &source, &meta, "output");
            std::fs::write(&requested_hit, audio_only_opus_mp4_for_stream(0)).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&requested_hit)
                .unwrap()
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
                .unwrap();

            let stale = preview_dir.join("audio-preview-stale.mp4");
            std::fs::write(&stale, [0_u8; 40]).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&stale)
                .unwrap()
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(2))
                .unwrap();

            let sidecars = finalize_prepared_audio_sidecars(
                prepare_clip_audio_sidecars_file_with_extractor_and_limits(
                    source,
                    vec!["output".into()],
                    Vec::new(),
                    preview_dir,
                    std::fs::metadata(&requested_hit).unwrap().len() + 39,
                    |_, _| panic!("extractor must not run for a valid requested cache hit"),
                )
                .unwrap(),
                |_| Ok(()),
            )
            .unwrap();

            assert!(
                requested_hit.exists(),
                "requested hit must survive initial prune"
            );
            assert!(!stale.exists(), "stale unrequested entry should be evicted");
            assert_eq!(sidecars.len(), 1);
            assert_eq!(sidecars[0].path, requested_hit.display().to_string());
        }
        #[test]
        fn audio_sidecar_failure_cleans_temps_and_publishes_nothing() {
            let dir = TestDir::new("clipline-library", "audio-sidecar-cleanup");
            let source = dir.path().join("clip.mp4");
            std::fs::write(&source, two_real_opus_audio_mp4()).unwrap();
            write_audio_track_markers(
                &source,
                vec![
                    ("output", 0, "Output Audio"),
                    ("microphone", 1, "Microphone"),
                ],
            );
            let preview_dir = dir.path().join("previews");

            let err = prepare_clip_audio_sidecars_file_with_extractor(
                source,
                vec!["output".into(), "microphone".into()],
                Vec::new(),
                preview_dir.clone(),
                |_, outputs| {
                    std::fs::write(&outputs[0].tmp_path, b"invalid").unwrap();
                    Err("forced extractor failure".into())
                },
            )
            .expect_err("extractor failure should bubble up");

            assert!(err.contains("forced extractor failure"), "{err}");
            assert!(
                preview_dir
                    .read_dir()
                    .unwrap_or_else(|_| panic!("preview dir should exist"))
                    .flatten()
                    .all(|entry| {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        !name.ends_with(".tmp") && !name.ends_with(".mp4")
                    }),
                "failure must not leave temp or final sidecars behind"
            );
        }
        #[test]
        fn audio_sidecar_ffmpeg_args_use_one_input_and_one_audio_only_output_per_missing_stream() {
            let dir = TestDir::new("clipline-library", "audio-sidecar-ffmpeg-args");
            let source = dir.path().join("clip.mp4");
            let outputs = vec![
                AudioTrackSidecarOutput {
                    audio_track_id: "output".into(),
                    audio_stream_index: 0,
                    final_path: dir.path().join("audio-preview-1.mp4"),
                    tmp_path: dir.path().join("audio-preview-1.mp4.tmp"),
                },
                AudioTrackSidecarOutput {
                    audio_track_id: "microphone".into(),
                    audio_stream_index: 2,
                    final_path: dir.path().join("audio-preview-2.mp4"),
                    tmp_path: dir.path().join("audio-preview-2.mp4.tmp"),
                },
            ];

            let args = ffmpeg_audio_sidecar_args(&source, &outputs);

            assert_eq!(args.iter().filter(|arg| **arg == "-i").count(), 1);
            assert!(args.windows(2).any(|pair| pair == ["-map", "0:a:0"]));
            assert!(args.windows(2).any(|pair| pair == ["-map", "0:a:2"]));
            assert_eq!(args.iter().filter(|arg| **arg == "-vn").count(), 2);
            assert_eq!(args.iter().filter(|arg| **arg == "-c:a").count(), 2);
            assert_eq!(args.iter().filter(|arg| **arg == "copy").count(), 2);
            assert_eq!(
                args.iter().filter(|arg| **arg == "-map_metadata").count(),
                2
            );
            assert_eq!(args.iter().filter(|arg| **arg == "-1").count(), 2);
            assert!(!args.windows(2).any(|pair| pair == ["-map", "0:v:0"]));
            assert!(!args.iter().any(|arg| *arg == "libopus"));
            assert!(!args.iter().any(|arg| arg.contains("amix")));
        }
}
