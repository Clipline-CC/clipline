use super::groups::{MAX_COMPILATION_CLIPS, group_fingerprint, group_members, GroupMember};
use super::*;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CompilationInput {
    pub(crate) path: PathBuf,
    pub(crate) audio_tracks: usize,
    pub(crate) duration_s: f64,
}

pub(crate) fn export_group_file(
    root: &Path,
    name: &str,
    job: &ClipboardExportJob,
) -> Result<ClipInfo, String> {
    let members = group_members(root, name)?;
    validate_compilation_size(&members)?;
    let fingerprint = group_fingerprint(&members);
    let inputs = compilation_inputs(&members)?;
    let target = unique_compilation_path(root, name)?;
    let tmp = crate::settings::persistence::sibling_tmp_path(&target)?;
    if let Err(error) = run_compilation_ffmpeg(&inputs, &tmp, job) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    if job.is_cancelled() {
        let _ = std::fs::remove_file(&tmp);
        return Err("group compilation cancelled".into());
    }
    if let Err(error) = std::fs::rename(&tmp, &target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("publish compilation: {error}"));
    }

    let title = format!("{name} compilation");
    let duration_s = compilation_duration_s(&target, &inputs);
    let markers = ClipMarkers {
        recording_start_s: 0.0,
        duration_s,
        player_summary: None,
        audio_tracks: Vec::new(),
        plays: Vec::new(),
        markers: Vec::new(),
        bookmarks: Vec::new(),
    };
    let publish_metadata = (|| {
        write_clip_metadata(
            &target,
            &ClipMetadata {
                title: Some(title.clone()),
                kind: Some("compilation".to_string()),
                group: None,
                source_group: Some(name.to_string()),
                source_group_fingerprint: Some(fingerprint.clone()),
            },
        )?;
        let json = serde_json::to_vec_pretty(&markers)
            .map_err(|error| format!("serialize compilation markers: {error}"))?;
        std::fs::write(target.with_extension("markers.json"), json)
            .map_err(|error| format!("write compilation markers: {error}"))
    })();
    if let Err(error) = publish_metadata {
        let _ = remove_clip_files(&target, root);
        return Err(error);
    }

    let metadata = std::fs::metadata(&target)
        .map_err(|error| format!("read compilation metadata: {error}"))?;
    let modified_unix = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    Ok(ClipInfo {
        path: target.display().to_string(),
        name: target
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        title: Some(title),
        kind: "compilation".to_string(),
        favorite: false,
        session: None,
        size_mb: metadata.len() as f64 / (1024.0 * 1024.0),
        modified_unix,
        duration_s: Some(duration_s),
        markers: Some(markers),
        game: None,
        group: None,
        source_group: Some(name.to_string()),
        source_group_fingerprint: Some(fingerprint),
    })
}

pub(crate) fn compilation_duration_s(target: &Path, inputs: &[CompilationInput]) -> f64 {
    clipline_mp4::movie_duration_s_file(target)
        .ok()
        .flatten()
        .filter(|duration| duration.is_finite() && *duration > 0.0)
        .unwrap_or_else(|| inputs.iter().map(|input| input.duration_s).sum())
}

pub(crate) fn validate_compilation_size(members: &[GroupMember]) -> Result<(), String> {
    if members.is_empty() {
        return Err("group has no clips".into());
    }
    // ponytail: direct inputs keep this one FFmpeg job; switch to staged concat files if groups
    // routinely need more than the Windows command line can hold.
    if members.len() > MAX_COMPILATION_CLIPS {
        return Err(format!(
            "a compilation can contain at most {MAX_COMPILATION_CLIPS} clips"
        ));
    }
    Ok(())
}

pub(crate) fn validate_compilation_command_line(ffmpeg: &Path, args: &[String]) -> Result<(), String> {
    const SAFE_WINDOWS_COMMAND_LINE_CHARS: usize = 32_000;
    let chars = ffmpeg.as_os_str().encode_wide().count()
        + 1
        + args
            .iter()
            .map(|arg| arg.encode_utf16().count() + 3)
            .sum::<usize>();
    if chars > SAFE_WINDOWS_COMMAND_LINE_CHARS {
        return Err(
            "group compilation command is too long; use fewer clips or a shorter media folder path"
                .into(),
        );
    }
    Ok(())
}

pub(crate) fn compilation_inputs(members: &[GroupMember]) -> Result<Vec<CompilationInput>, String> {
    members
        .iter()
        .map(|member| {
            let counts = clipline_mp4::media_track_counts_file(&member.path)
                .map_err(|error| format!("inspect group clip {:?}: {error}", member.path))?;
            if counts.video != 1 {
                return Err(format!(
                    "group clip {:?} must contain exactly one video track",
                    member.path
                ));
            }
            let duration_s = clipline_mp4::movie_duration_s_file(&member.path)
                .map_err(|error| format!("inspect group clip duration {:?}: {error}", member.path))?
                .filter(|duration| duration.is_finite() && *duration > 0.0)
                .ok_or_else(|| format!("group clip {:?} has no valid duration", member.path))?;
            Ok(CompilationInput {
                path: member.path.clone(),
                audio_tracks: counts.audio,
                duration_s,
            })
        })
        .collect()
}

pub(crate) fn unique_compilation_path(root: &Path, name: &str) -> Result<PathBuf, String> {
    let stem = export_title_stem(name).unwrap_or_else(|| "Group".to_string());
    for suffix in 0..1000_u32 {
        let file_name = if suffix == 0 {
            format!("{stem} compilation.mp4")
        } else {
            format!("{stem} compilation {suffix}.mp4")
        };
        let candidate = root.join(file_name);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err("could not choose an unused compilation filename".into())
}

pub(crate) fn run_compilation_ffmpeg(
    inputs: &[CompilationInput],
    target: &Path,
    job: &ClipboardExportJob,
) -> Result<(), String> {
    let ffmpeg = clipline_capture::ffmpeg::locate()
        .ok_or_else(|| "ffmpeg is not available for group compilation export".to_string())?;
    let encoders = available_h264_encoders();
    if encoders.is_empty() {
        return Err(
            "no usable FFmpeg H.264 encoder is available for group compilation export".into(),
        );
    }
    let duration_s: f64 = inputs.iter().map(|input| input.duration_s).sum();
    for (encoder, backend) in &encoders {
        validate_compilation_command_line(
            &ffmpeg,
            &ffmpeg_compilation_args(inputs, target, encoder, *backend),
        )?;
    }
    run_ffmpeg_fallback(
        &ffmpeg,
        target,
        share_export_timeout_for_duration(duration_s),
        encoders,
        "group compilation",
        || job.is_cancelled(),
        |(encoder, backend)| ffmpeg_compilation_args(inputs, target, encoder, *backend),
    )
}

pub(crate) fn ffmpeg_compilation_args(
    inputs: &[CompilationInput],
    target: &Path,
    encoder: &str,
    backend: EncoderBackend,
) -> Vec<String> {
    let mut args = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
        "-y".into(),
    ];
    let mut stream_inputs = Vec::with_capacity(inputs.len());
    let mut next_input = 0_usize;
    for input in inputs {
        let video_input = next_input;
        args.extend(["-i".into(), input.path.display().to_string()]);
        next_input += 1;
        let audio_input = if input.audio_tracks > 0 {
            video_input
        } else {
            let audio_input = next_input;
            args.extend([
                "-f".into(),
                "lavfi".into(),
                "-t".into(),
                format!("{:.6}", input.duration_s),
                "-i".into(),
                "anullsrc=channel_layout=stereo:sample_rate=48000".into(),
            ]);
            next_input += 1;
            audio_input
        };
        stream_inputs.push((video_input, audio_input, input.audio_tracks.max(1)));
    }

    let mut filters = Vec::with_capacity(inputs.len() * 2 + 1);
    for (index, (video_input, audio_input, audio_tracks)) in stream_inputs.iter().enumerate() {
        let duration_s = inputs[index].duration_s;
        filters.push(format!(
            "[{video_input}:v:0]scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2:color=black,setsar=1,fps=60,format=nv12,tpad=stop_mode=clone:stop_duration={duration_s:.6},trim=duration={duration_s:.6},setpts=PTS-STARTPTS[v{index}]"
        ));
        if *audio_tracks == 1 {
            filters.push(format!(
                "[{audio_input}:a:0]aresample=48000:async=1:first_pts=0,aformat=sample_fmts=fltp:channel_layouts=stereo,apad=whole_dur={duration_s:.6},atrim=duration={duration_s:.6},asetpts=N/SR/TB[a{index}]"
            ));
        } else {
            let mut mix_inputs = String::new();
            for track in 0..*audio_tracks {
                filters.push(format!(
                    "[{audio_input}:a:{track}]aresample=48000:async=1:first_pts=0,aformat=sample_fmts=fltp:channel_layouts=stereo[a{index}_{track}]"
                ));
                mix_inputs.push_str(&format!("[a{index}_{track}]"));
            }
            filters.push(format!(
                "{mix_inputs}amix=inputs={audio_tracks}:duration=longest:dropout_transition=0:normalize=1,apad=whole_dur={duration_s:.6},atrim=duration={duration_s:.6},asetpts=N/SR/TB[a{index}]"
            ));
        }
    }
    let concat_inputs: String = (0..inputs.len())
        .map(|index| format!("[v{index}][a{index}]"))
        .collect();
    filters.push(format!(
        "{concat_inputs}concat=n={}:v=1:a=1[v][a]",
        inputs.len()
    ));
    args.extend([
        "-filter_complex".into(),
        filters.join(";"),
        "-map".into(),
        "[v]".into(),
        "-map".into(),
        "[a]".into(),
        "-map_metadata".into(),
        "-1".into(),
        "-map_chapters".into(),
        "-1".into(),
        "-c:v".into(),
        encoder.to_string(),
    ]);
    args.extend(clipline_capture::ffmpeg_encoder::backend_rate_control(
        backend,
        SHARE_H264_BITRATE_BPS,
        SHARE_H264_BUFSIZE_BITS,
    ));
    args.extend([
        "-pix_fmt".into(),
        "nv12".into(),
        "-c:a".into(),
        "libopus".into(),
        "-b:a".into(),
        "192k".into(),
        "-ac".into(),
        "2".into(),
        "-ar".into(),
        "48000".into(),
        "-movflags".into(),
        "+faststart".into(),
        "-f".into(),
        "mp4".into(),
        target.display().to_string(),
    ]);
    args
}

#[cfg(test)]
impl CompilationInput {
    fn test(path: &str, audio_tracks: usize, duration_s: f64) -> Self {
        Self {
            path: PathBuf::from(path),
            audio_tracks,
            duration_s,
        }
    }
}
#[cfg(test)]
pub(crate) fn member_paths(members: &[GroupMember]) -> Vec<&str> {
    members
        .iter()
        .map(|member| member.path.to_str().unwrap())
        .collect()
}
#[cfg(test)]
mod tests {
    use super::groups::sort_group_members;
    use super::*;
        #[test]
        fn compilation_duration_falls_back_when_published_file_cannot_be_inspected() {
            let inputs = vec![
                CompilationInput::test("a.mp4", 1, 1.25),
                CompilationInput::test("b.mp4", 1, 2.5),
            ];
            assert_eq!(
                compilation_duration_s(Path::new("missing.mp4"), &inputs),
                3.75
            );
        }
        #[test]
        fn compilation_is_bounded_and_uses_group_order() {
            let mut members = vec![
                GroupMember::test("later.mp4", 4),
                GroupMember::test("first.mp4", 0),
            ];
            sort_group_members(&mut members);
            assert_eq!(member_paths(&members), ["first.mp4", "later.mp4"]);
            assert!(validate_compilation_size(&members).is_ok());
            assert!(validate_compilation_size(&vec![GroupMember::test("x.mp4", 0); 65]).is_err());
            assert!(
                validate_compilation_command_line(Path::new("ffmpeg.exe"), &["short".into()]).is_ok()
            );
            assert!(
                validate_compilation_command_line(Path::new("ffmpeg.exe"), &["x".repeat(32_000)])
                    .is_err()
            );
        }
        #[test]
        fn ffmpeg_compilation_args_normalize_video_and_supply_silent_audio() {
            let inputs = vec![
                CompilationInput::test("with.mp4", 2, 2.5),
                CompilationInput::test("silent.mp4", 0, 1.25),
            ];
            let args = ffmpeg_compilation_args(
                &inputs,
                Path::new("out.mp4"),
                "h264_mf",
                EncoderBackend::MfSoftware,
            );
            let joined = args.join(" ");

            assert!(joined.contains("scale=1920:1080:force_original_aspect_ratio=decrease"));
            assert!(joined.contains("tpad=stop_mode=clone"));
            assert!(joined.contains("trim=duration=2.500000"));
            assert!(joined.contains("anullsrc=channel_layout=stereo:sample_rate=48000"));
            assert!(joined.contains("[0:a:0]aresample=48000"));
            assert!(joined.contains("[0:a:1]aresample=48000"));
            assert!(joined.contains("amix=inputs=2:duration=longest:dropout_transition=0:normalize=1"));
            assert!(joined.contains("apad=whole_dur=2.500000,atrim=duration=2.500000"));
            assert!(joined.contains("asetpts=N/SR/TB[a0]"));
            assert!(joined.contains("concat=n=2:v=1:a=1"));
            assert!(joined.contains("-c:v h264_mf"));
            assert!(joined.contains("-c:a libopus"));
            assert!(joined.ends_with("out.mp4"));
        }
}
