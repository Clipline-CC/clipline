use super::*;

pub(crate) fn validate_and_publish_audio_sidecars(
    outputs: &[AudioTrackSidecarOutput],
) -> Result<PublishedAudioSidecars, String> {
    let result = (|| {
        for output in outputs {
            validate_audio_sidecar_file(&output.tmp_path)?;
        }

        let mut published = PublishedAudioSidecars::default();
        for output in outputs {
            if output.final_path.exists() {
                if let Err(error) = validate_audio_sidecar_file(&output.final_path) {
                    return Err(format!(
                        "finalize audio sidecar collision winner {path:?}: {error}",
                        path = output.final_path
                    ));
                }
                let _ = std::fs::remove_file(&output.tmp_path);
                continue;
            }

            match std::fs::rename(&output.tmp_path, &output.final_path) {
                Ok(()) => {
                    published.record_created(output.final_path.clone());
                }
                Err(_) if output.final_path.exists() => {
                    if let Err(error) = validate_audio_sidecar_file(&output.final_path) {
                        return Err(format!(
                            "finalize audio sidecar collision winner {path:?}: {error}",
                            path = output.final_path
                        ));
                    }
                    let _ = std::fs::remove_file(&output.tmp_path);
                }
                Err(error) => {
                    return Err(format!(
                        "finalize audio sidecar {tmp:?} -> {final_path:?}: {error}",
                        tmp = output.tmp_path,
                        final_path = output.final_path
                    ));
                }
            }
        }
        Ok(published)
    })();
    if result.is_err() {
        cleanup_audio_sidecar_temps(outputs);
    }
    result
}

pub(crate) fn cleanup_audio_sidecar_temps(outputs: &[AudioTrackSidecarOutput]) {
    for output in outputs {
        let _ = std::fs::remove_file(&output.tmp_path);
    }
}

pub(crate) fn cleanup_created_audio_sidecar_finals(paths: &[PathBuf]) {
    for path in paths {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_test_utils::TestDir;
        #[test]
        fn audio_sidecar_publication_guard_removes_owned_finals_but_keeps_collision_winner() {
            let dir = TestDir::new("clipline-library", "audio-sidecar-publication-guard");
            let owned_final = dir.path().join("audio-preview-owned.mp4");
            let owned_tmp = dir.path().join("audio-preview-owned.mp4.tmp");
            let collision_final = dir.path().join("audio-preview-collision.mp4");
            let collision_tmp = dir.path().join("audio-preview-collision.mp4.tmp");
            std::fs::write(&owned_tmp, audio_only_opus_mp4_for_stream(0)).unwrap();
            std::fs::write(&collision_tmp, audio_only_opus_mp4_for_stream(1)).unwrap();
            std::fs::write(&collision_final, audio_only_opus_mp4_for_stream(1)).unwrap();

            let outputs = vec![
                AudioTrackSidecarOutput {
                    audio_track_id: "owned".into(),
                    audio_stream_index: 0,
                    final_path: owned_final.clone(),
                    tmp_path: owned_tmp.clone(),
                },
                AudioTrackSidecarOutput {
                    audio_track_id: "collision".into(),
                    audio_stream_index: 1,
                    final_path: collision_final.clone(),
                    tmp_path: collision_tmp.clone(),
                },
            ];

            let guard = validate_and_publish_audio_sidecars(&outputs).unwrap();
            assert!(
                owned_final.exists(),
                "successful rename should publish owned final"
            );
            assert!(
                collision_final.exists(),
                "existing collision winner must remain"
            );
            assert!(!owned_tmp.exists(), "owned temp should be consumed");
            assert!(!collision_tmp.exists(), "collision temp should be removed");
            drop(guard);

            assert!(
                !owned_final.exists(),
                "dropping uncommitted guard should remove invocation-owned finals"
            );
            assert!(
                collision_final.exists(),
                "dropping uncommitted guard must not delete collision winners"
            );
        }
        #[test]
        fn audio_sidecar_validation_failure_owns_temp_cleanup() {
            let dir = TestDir::new("clipline-library", "audio-sidecar-validation-cleanup");
            let valid_tmp = dir.path().join("audio-preview-valid.mp4.tmp");
            let invalid_tmp = dir.path().join("audio-preview-invalid.mp4.tmp");
            std::fs::write(&valid_tmp, audio_only_opus_mp4_for_stream(0)).unwrap();
            std::fs::write(&invalid_tmp, b"invalid").unwrap();
            let outputs = vec![
                AudioTrackSidecarOutput {
                    audio_track_id: "valid".into(),
                    audio_stream_index: 0,
                    final_path: dir.path().join("audio-preview-valid.mp4"),
                    tmp_path: valid_tmp.clone(),
                },
                AudioTrackSidecarOutput {
                    audio_track_id: "invalid".into(),
                    audio_stream_index: 1,
                    final_path: dir.path().join("audio-preview-invalid.mp4"),
                    tmp_path: invalid_tmp.clone(),
                },
            ];

            validate_and_publish_audio_sidecars(&outputs)
                .expect_err("invalid extracted sidecar should fail validation");

            assert!(
                !valid_tmp.exists(),
                "validation failure must remove sibling temps"
            );
            assert!(
                !invalid_tmp.exists(),
                "validation failure must remove invalid temp"
            );
        }
        #[test]
        fn audio_sidecar_scope_failure_rolls_back_all_invocation_owned_finals() {
            let dir = TestDir::new("clipline-library", "audio-sidecar-scope-rollback");
            let source = dir.path().join("clip.mp4");
            touch_mp4(&source);
            write_audio_track_markers(
                &source,
                vec![
                    ("output", 0, "Output Audio"),
                    ("microphone", 1, "Microphone"),
                    ("discord", 2, "Discord"),
                ],
            );
            let preview_dir = dir.path().join("previews");
            std::fs::create_dir_all(&preview_dir).unwrap();

            let winner_path = audio_track_sidecar_path(
                &preview_dir,
                &source,
                &std::fs::metadata(&source).unwrap(),
                "output",
            );
            std::fs::write(&winner_path, audio_only_opus_mp4_for_stream(0)).unwrap();
            let winner_bytes = std::fs::read(&winner_path).unwrap();

            let batch = prepare_clip_audio_sidecars_file_with_extractor_and_limits(
                source,
                vec!["output".into(), "microphone".into(), "discord".into()],
                Vec::new(),
                preview_dir.clone(),
                AUDIO_PREVIEW_CACHE_MAX_BYTES,
                |_, outputs| {
                    for output in outputs {
                        let bytes = audio_only_opus_mp4_for_stream(output.audio_stream_index);
                        std::fs::write(&output.tmp_path, bytes).unwrap();
                    }
                    Ok(())
                },
            )
            .unwrap();

            let err = finalize_prepared_audio_sidecars(batch, |prepared| {
                if prepared.audio_track_id == "microphone" {
                    return Err("forced scope failure".into());
                }
                Ok(())
            })
            .unwrap_err();

            assert!(err.contains("forced scope failure"), "{err}");
            assert!(
                winner_path.exists(),
                "pre-existing collision winner must survive rollback"
            );
            assert_eq!(
                std::fs::read(&winner_path).unwrap(),
                winner_bytes,
                "collision winner contents must remain untouched"
            );

            let microphone_path = audio_track_sidecar_path(
                &preview_dir,
                &dir.path().join("clip.mp4"),
                &std::fs::metadata(dir.path().join("clip.mp4")).unwrap(),
                "microphone",
            );
            let discord_path = audio_track_sidecar_path(
                &preview_dir,
                &dir.path().join("clip.mp4"),
                &std::fs::metadata(dir.path().join("clip.mp4")).unwrap(),
                "discord",
            );
            assert!(
                !microphone_path.exists(),
                "scope failure must roll back invocation-owned finals"
            );
            assert!(
                !discord_path.exists(),
                "scope failure must remove every invocation-owned final"
            );
        }
}
