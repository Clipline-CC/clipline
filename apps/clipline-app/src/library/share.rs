use super::*;

pub(crate) fn clipboard_copy_path(
    source: &Path,
    selected_audio_track_ids: Option<&[String]>,
    original: bool,
    job: &ClipboardExportJob,
) -> Result<PathBuf, String> {
    clipboard_copy_path_with_exporter(
        source,
        selected_audio_track_ids,
        original,
        &crate::settings::share_export_cache_dir(),
        |source, target, mode| export_share_compatible_file(source, target, mode.as_ref(), job),
    )
}

pub(crate) fn clipboard_copy_path_with_exporter(
    source: &Path,
    selected_audio_track_ids: Option<&[String]>,
    original: bool,
    export_dir: &Path,
    export_audio: impl FnMut(&Path, &Path, Option<ShareAudioExportMode>) -> Result<(), String>,
) -> Result<PathBuf, String> {
    if original {
        return Ok(source.to_path_buf());
    }
    clipboard_share_path_with_exporter(source, selected_audio_track_ids, export_dir, export_audio)
}

pub(crate) fn clipboard_share_path_with_exporter(
    source: &Path,
    selected_audio_track_ids: Option<&[String]>,
    export_dir: &Path,
    mut export_audio: impl FnMut(&Path, &Path, Option<ShareAudioExportMode>) -> Result<(), String>,
) -> Result<PathBuf, String> {
    let mode = clipboard_share_export_mode(source, selected_audio_track_ids)?;

    let meta = std::fs::metadata(source).map_err(|e| format!("read clip metadata: {e}"))?;
    std::fs::create_dir_all(export_dir).map_err(|e| format!("create share export cache: {e}"))?;
    prune_old_share_exports(export_dir);
    let export = share_export_path(
        export_dir,
        source,
        &meta,
        selected_audio_track_ids,
        mode.as_ref(),
    );
    if export.exists() {
        return Ok(export);
    }

    let tmp = share_export_tmp_path(&export)?;
    if let Err(error) = export_audio(source, &tmp, mode) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    match std::fs::rename(&tmp, &export) {
        Ok(()) => {}
        Err(_) if export.exists() => {
            let _ = std::fs::remove_file(&tmp);
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("finalize share export: {e}"));
        }
    }
    Ok(export)
}

pub(crate) fn clipboard_share_export_mode(
    source: &Path,
    selected_audio_track_ids: Option<&[String]>,
) -> Result<Option<ShareAudioExportMode>, String> {
    let Some(selected_audio_track_ids) = selected_audio_track_ids else {
        return Ok(None);
    };
    let Some(markers) =
        util::markers_with_inferred_audio_tracks(source, util::read_markers_raw(source))
    else {
        return Ok(None);
    };
    let tracks = markers.audio_tracks.as_slice();
    if tracks.is_empty() {
        if selected_audio_track_ids.is_empty() {
            return Ok(Some(ShareAudioExportMode::Remux(Vec::new())));
        }
        return Err("this clip has no selectable audio track metadata".into());
    }
    let selected_indices = util::selected_audio_track_indices(&markers, selected_audio_track_ids)?;
    if selected_indices.len() > 1 {
        Ok(Some(ShareAudioExportMode::Mix(selected_indices)))
    } else {
        Ok(Some(ShareAudioExportMode::Remux(selected_indices)))
    }
}

pub(crate) fn share_export_path(
    export_dir: &Path,
    source: &Path,
    meta: &std::fs::Metadata,
    selected_audio_track_ids: Option<&[String]>,
    mode: Option<&ShareAudioExportMode>,
) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    "share-export-v3-aac-h264-cbr8m".hash(&mut hasher);
    source.display().to_string().hash(&mut hasher);
    meta.len().hash(&mut hasher);
    meta.modified().ok().hash(&mut hasher);
    mode.hash(&mut hasher);
    if let Some(ids) = selected_audio_track_ids {
        for id in ids {
            id.hash(&mut hasher);
        }
    }
    export_dir.join(format!("share-export-{:016x}.mp4", hasher.finish()))
}

pub(crate) fn export_share_compatible_file(
    source: &Path,
    target: &Path,
    audio_mode: Option<&ShareAudioExportMode>,
    job: &ClipboardExportJob,
) -> Result<(), String> {
    job.ensure_active()?;
    let mut intermediate = None;
    let (input, has_audio) = match audio_mode {
        Some(ShareAudioExportMode::Remux(indices)) => {
            let path = cached_export_tmp_path(target)?;
            remux_with_selected_audio_tracks_file(source, &path, indices)
                .map_err(|error| error.to_string())?;
            let has_audio = !indices.is_empty();
            intermediate = Some(path.clone());
            (path, has_audio)
        }
        Some(ShareAudioExportMode::Mix(indices)) => {
            let path = cached_export_tmp_path(target)?;
            remux_with_mixed_audio_track_file(source, &path, indices)
                .map_err(|error| error.to_string())?;
            intermediate = Some(path.clone());
            (path, true)
        }
        None => {
            let counts = clipline_mp4::media_track_counts_file(source)
                .map_err(|error| format!("inspect share audio tracks: {error}"))?;
            (source.to_path_buf(), counts.audio > 0)
        }
    };

    let result = job
        .ensure_active()
        .and_then(|()| transcode_share_file_with_ffmpeg(source, &input, target, has_audio, job));
    if let Some(intermediate) = intermediate {
        let _ = std::fs::remove_file(intermediate);
    }
    result
}

pub(crate) fn transcode_share_file_with_ffmpeg(
    source: &Path,
    input: &Path,
    target: &Path,
    has_audio: bool,
    job: &ClipboardExportJob,
) -> Result<(), String> {
    let ffmpeg = clipline_capture::ffmpeg::locate()
        .ok_or_else(|| "ffmpeg is not available for a shareable clipboard export".to_string())?;
    let video_modes = share_video_export_modes(source)?;
    let timeout = share_export_timeout(source);
    run_ffmpeg_fallback(
        &ffmpeg,
        target,
        timeout,
        video_modes,
        "shareable clipboard export",
        || job.is_cancelled(),
        |mode| ffmpeg_share_export_args(input, target, has_audio, mode),
    )
}

pub(crate) fn share_video_export_modes(source: &Path) -> Result<Vec<ShareVideoExportMode>, String> {
    let codecs = media_video_codecs_file(source)
        .map_err(|error| format!("inspect share video codec: {error}"))?;
    if codecs.as_slice() == [MediaVideoCodec::H264] {
        return Ok(vec![ShareVideoExportMode::Copy]);
    }
    if codecs.len() != 1 {
        return Err(format!(
            "shareable export requires exactly one video track, found {}",
            codecs.len()
        ));
    }

    let encoders = available_h264_encoders();
    if encoders.is_empty() {
        return Err("no usable FFmpeg H.264 encoder is available for this clip".into());
    }
    Ok(encoders
        .into_iter()
        .map(|(encoder, backend)| ShareVideoExportMode::Encode { encoder, backend })
        .collect())
}

pub(crate) fn available_h264_encoders() -> Vec<(String, EncoderBackend)> {
    let mut encoders = Vec::new();
    for capability in clipline_capture::ffmpeg::probe() {
        if !capability.codecs.contains(&Codec::H264) {
            continue;
        }
        let Some(name) = clipline_capture::ffmpeg::encoder_name(capability.backend, Codec::H264)
        else {
            continue;
        };
        if !encoders.iter().any(|(existing, _)| existing == name) {
            encoders.push((name.to_string(), capability.backend));
        }
    }
    encoders
}

pub(crate) fn ffmpeg_share_export_args(
    input: &Path,
    target: &Path,
    has_audio: bool,
    video_mode: &ShareVideoExportMode,
) -> Vec<String> {
    let mut args = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
        "-y".into(),
        "-i".into(),
        input.display().to_string(),
        "-map".into(),
        "0:v:0".into(),
    ];
    if has_audio {
        args.extend(["-map".into(), "0:a:0".into()]);
    }
    args.extend([
        "-map_metadata".into(),
        "-1".into(),
        "-map_chapters".into(),
        "-1".into(),
        "-c:v".into(),
    ]);
    match video_mode {
        ShareVideoExportMode::Copy => args.push("copy".into()),
        ShareVideoExportMode::Encode { encoder, backend } => {
            args.push(encoder.clone());
            args.extend(clipline_capture::ffmpeg_encoder::backend_rate_control(
                *backend,
                SHARE_H264_BITRATE_BPS,
                SHARE_H264_BUFSIZE_BITS,
            ));
            args.extend(["-pix_fmt".into(), "nv12".into()]);
        }
    }
    if has_audio {
        args.extend([
            "-c:a".into(),
            "aac".into(),
            "-profile:a".into(),
            "aac_low".into(),
            "-b:a".into(),
            "192k".into(),
            "-ac".into(),
            "2".into(),
            "-ar".into(),
            "48000".into(),
        ]);
    }
    args.extend([
        "-movflags".into(),
        "+faststart".into(),
        "-f".into(),
        "mp4".into(),
        target.display().to_string(),
    ]);
    args
}

pub(crate) fn share_export_timeout(source: &Path) -> Duration {
    let duration = clipline_mp4::movie_duration_s_file(source)
        .ok()
        .flatten()
        .unwrap_or(60.0);
    share_export_timeout_for_duration(duration)
}

pub(crate) fn share_export_timeout_for_duration(duration: f64) -> Duration {
    const MIN_SECONDS: u64 = 2 * 60;
    const MAX_SECONDS: u64 = 6 * 60 * 60;
    let seconds = (duration * 4.0 + 60.0).ceil().max(0.0) as u64;
    Duration::from_secs(seconds.clamp(MIN_SECONDS, MAX_SECONDS))
}

pub(crate) fn remaining_share_export_timeout(deadline: Instant, now: Instant) -> Option<Duration> {
    deadline
        .checked_duration_since(now)
        .filter(|remaining| !remaining.is_zero())
}

pub(crate) fn run_ffmpeg_fallback<T>(
    ffmpeg: &Path,
    target: &Path,
    timeout: Duration,
    modes: impl IntoIterator<Item = T>,
    label: &str,
    is_cancelled: impl Fn() -> bool,
    mut args_for: impl FnMut(&T) -> Vec<String>,
) -> Result<(), String> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);
    let mut last_error = String::new();
    for mode in modes {
        if is_cancelled() {
            return Err(format!("{label} cancelled"));
        }
        let Some(remaining) = remaining_share_export_timeout(deadline, Instant::now()) else {
            last_error = format!("exhausted its {} second timeout", timeout.as_secs());
            break;
        };
        let _ = std::fs::remove_file(target);
        let mut command = Command::new(ffmpeg);
        suppress_console(&mut command);
        command
            .args(args_for(&mode))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        match run_export_ffmpeg(&mut command, remaining, label, &is_cancelled) {
            Ok(output) if output.status.success() => return Ok(()),
            Ok(output) => {
                last_error = String::from_utf8_lossy(&output.stderr).trim().to_string();
                if last_error.is_empty() {
                    last_error = format!("ffmpeg exited with {}", output.status);
                }
            }
            Err(error) => last_error = error,
        }
    }
    let _ = std::fs::remove_file(target);
    if last_error.is_empty() {
        last_error = "no encoder attempts were available".into();
    }
    Err(format!("{label}: {last_error}"))
}

pub(crate) struct ShareFfmpegOutput {
    pub(crate) status: ExitStatus,
    pub(crate) stderr: Vec<u8>,
}

pub(crate) fn run_export_ffmpeg(
    command: &mut Command,
    timeout: Duration,
    label: &str,
    is_cancelled: impl Fn() -> bool,
) -> Result<ShareFfmpegOutput, String> {
    const MAX_STDERR_BYTES: usize = 128 * 1024;

    let mut child = command
        .spawn()
        .map_err(|error| format!("spawn ffmpeg {label}: {error}"))?;
    let Some(stderr) = child.stderr.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("spawn ffmpeg {label}: stderr pipe unavailable"));
    };
    let reader = match std::thread::Builder::new()
        .name("clipline-share-ffmpeg-stderr".into())
        .spawn(move || read_bounded_share_stderr(stderr, MAX_STDERR_BYTES))
    {
        Ok(reader) => reader,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("spawn ffmpeg {label} stderr reader: {error}"));
        }
    };

    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);
    let status = loop {
        if is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            break Err(format!("{label} cancelled"));
        }
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(format!(
                    "{label} timed out after {} seconds",
                    timeout.as_secs()
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(format!("wait for ffmpeg {label}: {error}"));
            }
        }
    };
    let stderr = reader
        .join()
        .map_err(|_| format!("ffmpeg {label} stderr reader panicked"))?
        .map_err(|error| format!("read ffmpeg {label} stderr: {error}"))?;
    Ok(ShareFfmpegOutput {
        status: status?,
        stderr,
    })
}

pub(crate) fn read_bounded_share_stderr(mut reader: impl Read, max_bytes: usize) -> io::Result<Vec<u8>> {
    let mut retained = Vec::new();
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            return Ok(retained);
        }
        let keep = read.min(max_bytes.saturating_sub(retained.len()));
        retained.extend_from_slice(&chunk[..keep]);
    }
}

pub(crate) fn share_export_tmp_path(export: &Path) -> Result<PathBuf, String> {
    cached_export_tmp_path(export)
}

pub(crate) fn cached_export_tmp_path(target: &Path) -> Result<PathBuf, String> {
    crate::settings::persistence::sibling_tmp_path(target)
}

pub(crate) fn prune_old_share_exports(export_dir: &Path) {
    const MAX_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
    prune_cached_mp4_files(export_dir, MAX_AGE);
}

pub(crate) fn prune_cached_mp4_files(export_dir: &Path, max_age: std::time::Duration) {
    let Ok(entries) = std::fs::read_dir(export_dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_cached_mp4_file(&path) {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= max_age);
        if old {
            let _ = std::fs::remove_file(path);
        }
    }
}

pub(crate) fn is_cached_mp4_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if !name.starts_with("share-export-") {
        return false;
    }
    if name.ends_with(".mp4") || name.ends_with(".mp4.tmp") {
        return true;
    }
    let Some((_, suffix)) = name.split_once(".mp4.") else {
        return false;
    };
    let parts = suffix.split('.').collect::<Vec<_>>();
    !parts.is_empty()
        && parts.len().is_multiple_of(3)
        && parts.as_chunks::<3>().0.iter().all(|chunk| {
            !chunk[0].is_empty()
                && chunk[0].bytes().all(|byte| byte.is_ascii_digit())
                && !chunk[1].is_empty()
                && chunk[1].bytes().all(|byte| byte.is_ascii_digit())
                && chunk[2] == "tmp"
        })
}

pub(crate) fn extract_audio_sidecars_with_ffmpeg(
    source: &Path,
    outputs: &[AudioTrackSidecarOutput],
) -> Result<(), String> {
    let ffmpeg = clipline_capture::ffmpeg::locate()
        .ok_or_else(|| "ffmpeg is not available for audio sidecar extraction".to_string())?;
    let mut cmd = Command::new(ffmpeg);
    suppress_console(&mut cmd);
    let output = cmd
        .args(ffmpeg_audio_sidecar_args(source, outputs))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("spawn ffmpeg audio sidecar extraction: {e}"))?;
    if !output.status.success() {
        cleanup_audio_sidecar_temps(outputs);
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ffmpeg audio sidecar extraction failed: {stderr}"));
    }
    Ok(())
}

pub(crate) fn ffmpeg_audio_sidecar_args(source: &Path, outputs: &[AudioTrackSidecarOutput]) -> Vec<String> {
    let mut args = vec![
        "-hide_banner".to_string(),
        "-nostdin".to_string(),
        "-y".to_string(),
        "-i".to_string(),
        source.display().to_string(),
    ];
    for output in outputs {
        args.extend([
            "-map".to_string(),
            format!("0:a:{}", output.audio_stream_index),
            "-vn".to_string(),
            "-map_metadata".to_string(),
            "-1".to_string(),
            "-c:a".to_string(),
            "copy".to_string(),
            "-f".to_string(),
            "mp4".to_string(),
            output.tmp_path.display().to_string(),
        ]);
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_test_utils::TestDir;
    use clipline_events::{ClipAudioTrack, ClipMarkers};
        #[test]
        fn clipboard_share_export_mixes_multiple_selected_tracks() {
            let dir = TestDir::new("clipline-library", "clipboard-share-mix");
            let source = dir.path().join("clip.mp4");
            std::fs::write(&source, b"source mp4").unwrap();
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
            std::fs::write(
                source.with_extension("markers.json"),
                serde_json::to_string(&markers).unwrap(),
            )
            .unwrap();

            let selected = vec!["output".to_string(), "microphone".to_string()];
            let export_dir = dir.path().join("share-exports");
            let exported = clipboard_share_path_with_exporter(
                &source,
                Some(&selected),
                &export_dir,
                |input, target, mode| {
                    assert_eq!(input, source.as_path());
                    assert_eq!(mode, Some(ShareAudioExportMode::Mix(vec![0, 1])));
                    std::fs::write(target, b"mixed share mp4").unwrap();
                    Ok(())
                },
            )
            .unwrap();

            assert!(exported.starts_with(&export_dir));
            assert_eq!(std::fs::read(exported).unwrap(), b"mixed share mp4");
        }
        #[test]
        fn clipboard_share_without_audio_selection_prepares_compatibility_export() {
            let dir = TestDir::new("clipline-library", "clipboard-share-original");
            let source = dir.path().join("clip.mp4");
            std::fs::write(&source, b"source mp4").unwrap();

            let selected = None::<&[String]>;
            let chosen = clipboard_share_path_with_exporter(
                &source,
                selected,
                &dir.path().join("share"),
                |input, target, mode| {
                    assert_eq!(input, source);
                    assert_eq!(mode, None);
                    std::fs::write(target, b"compatible mp4").unwrap();
                    Ok(())
                },
            )
            .unwrap();

            assert_ne!(chosen, source);
            assert_eq!(std::fs::read(chosen).unwrap(), b"compatible mp4");
        }
        #[test]
        fn original_clipboard_copy_bypasses_share_export() {
            let dir = TestDir::new("clipline-library", "clipboard-original-bypass");
            let source = dir.path().join("clip.mp4");
            std::fs::write(&source, b"source mp4").unwrap();

            let chosen = clipboard_copy_path_with_exporter(
                &source,
                Some(&["output".to_string()]),
                true,
                &dir.path().join("share"),
                |_, _, _| panic!("original copy must not prepare a share export"),
            )
            .unwrap();

            assert_eq!(chosen, source);
        }
        #[test]
        fn ffmpeg_share_export_stream_copies_h264_and_encodes_aac_lc() {
            let args = ffmpeg_share_export_args(
                Path::new("selected.mp4"),
                Path::new("share.mp4.tmp"),
                true,
                &ShareVideoExportMode::Copy,
            );

            assert!(args.windows(2).any(|pair| pair == ["-c:v", "copy"]));
            assert!(args.windows(2).any(|pair| pair == ["-map", "0:a:0"]));
            assert!(args.windows(2).any(|pair| pair == ["-c:a", "aac"]));
            assert!(args
                .windows(2)
                .any(|pair| pair == ["-profile:a", "aac_low"]));
            assert!(args
                .windows(2)
                .any(|pair| pair == ["-movflags", "+faststart"]));
            assert_eq!(args.last().map(String::as_str), Some("share.mp4.tmp"));
        }
        #[test]
        fn ffmpeg_share_export_omits_audio_for_muted_selection() {
            let args = ffmpeg_share_export_args(
                Path::new("muted.mp4"),
                Path::new("share.mp4.tmp"),
                false,
                &ShareVideoExportMode::Copy,
            );

            assert!(!args.iter().any(|arg| arg == "0:a:0"));
            assert!(!args.iter().any(|arg| arg == "-c:a"));
        }
        #[test]
        fn ffmpeg_share_export_can_transcode_video_with_mf_fallback() {
            let args = ffmpeg_share_export_args(
                Path::new("av1.mp4"),
                Path::new("share.mp4.tmp"),
                true,
                &ShareVideoExportMode::Encode {
                    encoder: "h264_mf".into(),
                    backend: clipline_capture::EncoderBackend::MfSoftware,
                },
            );

            assert!(args.windows(2).any(|pair| pair == ["-c:v", "h264_mf"]));
            assert!(args.windows(2).any(|pair| pair == ["-hw_encoding", "0"]));
            assert!(args.windows(2).any(|pair| pair == ["-pix_fmt", "nv12"]));
            assert!(args.windows(2).any(|pair| pair == ["-b:v", "8000000"]));
        }
        #[test]
        fn ffmpeg_share_export_applies_backend_specific_rate_control() {
            use clipline_capture::EncoderBackend;

            for (encoder, backend, required) in [
                (
                    "h264_nvenc",
                    EncoderBackend::Nvenc,
                    ["-rc", "cbr", "-preset", "p4"],
                ),
                (
                    "h264_amf",
                    EncoderBackend::Amf,
                    ["-rc", "cbr", "-usage", "lowlatency"],
                ),
                (
                    "h264_qsv",
                    EncoderBackend::QuickSync,
                    ["-low_power", "0", "-maxrate", "8000000"],
                ),
            ] {
                let args = ffmpeg_share_export_args(
                    Path::new("source.mp4"),
                    Path::new("share.mp4.tmp"),
                    true,
                    &ShareVideoExportMode::Encode {
                        encoder: encoder.into(),
                        backend,
                    },
                );
                let joined = args.join(" ");
                for pair in required.as_chunks::<2>().0 {
                    assert!(
                        joined.contains(&pair.join(" ")),
                        "{encoder} missing {} in {joined}",
                        pair.join(" ")
                    );
                }
                assert!(joined.contains("-b:v 8000000"), "{encoder}: {joined}");
                assert!(joined.contains("-bufsize 16000000"), "{encoder}: {joined}");
            }
        }
        #[test]
        fn remaining_share_export_timeout_uses_one_deadline() {
            let start = Instant::now();
            let deadline = start + Duration::from_secs(10);

            assert_eq!(
                remaining_share_export_timeout(deadline, start + Duration::from_secs(3)),
                Some(Duration::from_secs(7))
            );
            assert_eq!(remaining_share_export_timeout(deadline, deadline), None);
            assert_eq!(
                remaining_share_export_timeout(deadline, deadline + Duration::from_secs(1)),
                None
            );
        }
        #[test]
        fn share_export_tmp_path_is_unique_per_writer() {
            let dir = TestDir::new("clipline-library", "share-export-temp");
            let export = dir.path().join("share-export-abc.mp4");

            let first = share_export_tmp_path(&export).unwrap();
            let second = share_export_tmp_path(&export).unwrap();

            assert_ne!(first, second);
            assert_ne!(first, export.with_extension("mp4.tmp"));
            assert_eq!(first.parent(), export.parent());
        }
        #[test]
        fn share_export_prune_removes_orphaned_tmp_files() {
            let dir = TestDir::new("clipline-library", "share-export-prune-tmp");
            let export = dir.path().join("share-export-old.mp4");
            let orphan = dir.path().join("share-export-old.mp4.tmp");
            let unique = share_export_tmp_path(&export).unwrap();
            let nested_unique = cached_export_tmp_path(&unique).unwrap();
            let malformed = dir.path().join("share-export-old.mp4.pid.counter.tmp");
            std::fs::write(&export, b"old export").unwrap();
            std::fs::write(&orphan, b"orphan").unwrap();
            std::fs::write(&unique, b"unique orphan").unwrap();
            std::fs::write(&nested_unique, b"nested unique orphan").unwrap();
            std::fs::write(&malformed, b"not an owned temp shape").unwrap();

            prune_cached_mp4_files(dir.path(), std::time::Duration::ZERO);

            assert!(!export.exists());
            assert!(!orphan.exists());
            assert!(!unique.exists());
            assert!(!nested_unique.exists());
            assert!(malformed.exists());
        }
}
