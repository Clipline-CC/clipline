use super::*;

const MAX_GROUP_NAME_CHARS: usize = 80;
const MAX_COMPILATION_CLIPS: usize = 64;
const GROUP_COMPILATION_VERSION: u32 = 2;

#[derive(Clone, Debug, PartialEq)]
struct GroupMember {
    path: PathBuf,
    group: ClipGroup,
    modified_unix: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GroupMove {
    Up,
    Down,
}

#[derive(serde::Serialize)]
pub struct GroupOrderUpdate {
    pub path: String,
    pub order: u32,
}

#[derive(Clone, Debug, PartialEq)]
struct CompilationInput {
    path: PathBuf,
    audio_tracks: usize,
    duration_s: f64,
}

pub(super) fn group_for_export(root: &Path, requested: &str) -> Result<ClipGroup, String> {
    let name = normalized_group_name(requested)?;
    let existing: Vec<ClipGroup> = list_clips_from_dir(root.to_path_buf())?
        .clips
        .into_iter()
        .filter_map(|clip| clip.group)
        .collect();
    let (name, order) = canonical_group(&existing, &name);
    Ok(ClipGroup { name, order })
}

fn normalized_group_name(input: &str) -> Result<String, String> {
    let name = input.trim();
    if name.is_empty() {
        return Err("group name is required".into());
    }
    if name.chars().any(char::is_control) {
        return Err("group name contains a control character".into());
    }
    if name.chars().count() > MAX_GROUP_NAME_CHARS {
        return Err(format!(
            "group name cannot exceed {MAX_GROUP_NAME_CHARS} characters"
        ));
    }
    Ok(name.to_string())
}

fn canonical_group(existing: &[ClipGroup], requested: &str) -> (String, u32) {
    let matching: Vec<&ClipGroup> = existing
        .iter()
        .filter(|group| group.name.eq_ignore_ascii_case(requested))
        .collect();
    let name = matching
        .first()
        .map(|group| group.name.clone())
        .unwrap_or_else(|| requested.to_string());
    let order = matching
        .into_iter()
        .map(|group| group.order)
        .max()
        .and_then(|order| order.checked_add(1))
        .unwrap_or(0);
    (name, order)
}

fn group_members(root: &Path, name: &str) -> Result<Vec<GroupMember>, String> {
    let mut members: Vec<GroupMember> = list_clips_from_dir(root.to_path_buf())?
        .clips
        .into_iter()
        .filter_map(|clip| {
            let group = clip.group?;
            if !group.name.eq_ignore_ascii_case(name) {
                return None;
            }
            Some(GroupMember {
                path: PathBuf::from(clip.path),
                group,
                modified_unix: clip.modified_unix,
            })
        })
        .collect();
    sort_group_members(&mut members);
    Ok(members)
}

fn sort_group_members(members: &mut [GroupMember]) {
    members.sort_by(|left, right| {
        left.group
            .order
            .cmp(&right.group.order)
            .then(left.modified_unix.cmp(&right.modified_unix))
            .then_with(|| left.path.cmp(&right.path))
    });
}

fn moved_members(
    mut members: Vec<GroupMember>,
    target: &str,
    direction: GroupMove,
) -> Result<Vec<GroupMember>, String> {
    let index = members
        .iter()
        .position(|member| member.path.to_string_lossy() == target)
        .ok_or_else(|| "clip is not in this group".to_string())?;
    let neighbor = match direction {
        GroupMove::Up => index.checked_sub(1),
        GroupMove::Down => (index + 1 < members.len()).then_some(index + 1),
    };
    if let Some(neighbor) = neighbor {
        members.swap(index, neighbor);
    }
    for (order, member) in members.iter_mut().enumerate() {
        member.group.order = order as u32;
    }
    Ok(members)
}

fn parsed_move(direction: &str) -> Result<GroupMove, String> {
    match direction {
        "up" => Ok(GroupMove::Up),
        "down" => Ok(GroupMove::Down),
        _ => Err("group move direction must be up or down".into()),
    }
}

#[tauri::command]
pub async fn move_group_clip(
    path: String,
    direction: String,
    settings: tauri::State<'_, StorageSettings>,
) -> Result<Vec<GroupOrderUpdate>, String> {
    let target = validate_clip_path(&settings, &path)?;
    let group = read_clip_metadata(&target)
        .and_then(|metadata| metadata.group)
        .ok_or_else(|| "clip is not in a group".to_string())?;
    let root = settings.clips_dir()?;
    let direction = parsed_move(&direction)?;
    tauri::async_runtime::spawn_blocking(move || {
        let members = moved_members(group_members(&root, &group.name)?, &path, direction)?;
        for member in &members {
            let mut metadata = read_clip_metadata(&member.path).unwrap_or_default();
            metadata.group = Some(member.group.clone());
            write_clip_metadata(&member.path, &metadata)?;
        }
        Ok(members
            .into_iter()
            .map(|member| GroupOrderUpdate {
                path: member.path.display().to_string(),
                order: member.group.order,
            })
            .collect())
    })
    .await
    .map_err(|error| format!("move group clip task: {error}"))?
}

#[tauri::command]
pub async fn export_group<R: Runtime>(
    app: AppHandle<R>,
    name: String,
    settings: tauri::State<'_, StorageSettings>,
) -> Result<ClipInfo, String> {
    let name = normalized_group_name(&name)?;
    let root = settings.clips_dir()?;
    let scope_root = root.clone();
    let exported = tauri::async_runtime::spawn_blocking(move || export_group_file(&root, &name))
        .await
        .map_err(|error| format!("export group task: {error}"))??;
    allow_local_clip_asset(&app, &scope_root, Path::new(&exported.path))?;
    Ok(exported)
}

fn export_group_file(root: &Path, name: &str) -> Result<ClipInfo, String> {
    let members = group_members(root, name)?;
    validate_compilation_size(&members)?;
    let inputs = compilation_inputs(&members)?;
    let target = unique_compilation_path(root, name)?;
    let tmp = crate::settings::persistence::sibling_tmp_path(&target)?;
    if let Err(error) = run_compilation_ffmpeg(&inputs, &tmp) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&tmp, &target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("publish compilation: {error}"));
    }

    let title = format!("{name} compilation");
    let duration_s = clipline_mp4::movie_duration_s_file(&target)
        .map_err(|error| format!("inspect compilation duration: {error}"))?
        .unwrap_or_else(|| inputs.iter().map(|input| input.duration_s).sum());
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
                compilation_version: Some(GROUP_COMPILATION_VERSION),
            },
        )?;
        let json = serde_json::to_vec_pretty(&markers)
            .map_err(|error| format!("serialize compilation markers: {error}"))?;
        std::fs::write(target.with_extension("markers.json"), json)
            .map_err(|error| format!("write compilation markers: {error}"))
    })();
    if let Err(error) = publish_metadata {
        let _ = remove_clip_files(&target);
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
        session: None,
        size_mb: metadata.len() as f64 / (1024.0 * 1024.0),
        modified_unix,
        duration_s: Some(duration_s),
        markers: Some(markers),
        game: None,
        group: None,
        source_group: Some(name.to_string()),
        compilation_version: Some(GROUP_COMPILATION_VERSION),
    })
}

fn validate_compilation_size(members: &[GroupMember]) -> Result<(), String> {
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

fn compilation_inputs(members: &[GroupMember]) -> Result<Vec<CompilationInput>, String> {
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

fn unique_compilation_path(root: &Path, name: &str) -> Result<PathBuf, String> {
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

fn available_h264_encoders() -> Vec<(String, EncoderBackend)> {
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

fn run_compilation_ffmpeg(inputs: &[CompilationInput], target: &Path) -> Result<(), String> {
    let ffmpeg = clipline_capture::ffmpeg::locate()
        .ok_or_else(|| "ffmpeg is not available for group compilation export".to_string())?;
    let encoders = available_h264_encoders();
    if encoders.is_empty() {
        return Err(
            "no usable FFmpeg H.264 encoder is available for group compilation export".into(),
        );
    }
    let duration_s: f64 = inputs.iter().map(|input| input.duration_s).sum();
    let timeout = Duration::from_secs((duration_s * 4.0 + 60.0).ceil().max(120.0) as u64)
        .min(Duration::from_secs(6 * 60 * 60));
    let mut last_error = String::new();
    for (encoder, backend) in encoders {
        let _ = std::fs::remove_file(target);
        let mut command = Command::new(&ffmpeg);
        suppress_console(&mut command);
        command
            .args(ffmpeg_compilation_args(inputs, target, &encoder, backend))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        match run_share_ffmpeg(&mut command, timeout, || false) {
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
    Err(format!("export group compilation: {last_error}"))
}

fn ffmpeg_compilation_args(
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
        filters.push(format!(
            "[{video_input}:v:0]scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2:color=black,setsar=1,fps=60,format=nv12,setpts=PTS-STARTPTS[v{index}]"
        ));
        if *audio_tracks == 1 {
            filters.push(format!(
                "[{audio_input}:a:0]aresample=48000:async=1:first_pts=0,aformat=sample_fmts=fltp:channel_layouts=stereo,asetpts=PTS-STARTPTS[a{index}]"
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
                "{mix_inputs}amix=inputs={audio_tracks}:duration=longest:dropout_transition=0:normalize=1,asetpts=N/SR/TB[a{index}]"
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
impl GroupMember {
    fn test(path: &str, order: u32) -> Self {
        Self {
            path: PathBuf::from(path),
            group: ClipGroup {
                name: "Highlights".into(),
                order,
            },
            modified_unix: 0,
        }
    }
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
fn member_paths(members: &[GroupMember]) -> Vec<&str> {
    members
        .iter()
        .map(|member| member.path.to_str().unwrap())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_names_are_trimmed_bounded_and_control_free() {
        assert_eq!(
            normalized_group_name("  Highlights  ").unwrap(),
            "Highlights"
        );
        assert!(normalized_group_name("   ").is_err());
        assert!(normalized_group_name("bad\nname").is_err());
        assert!(normalized_group_name(&"x".repeat(81)).is_err());
    }

    #[test]
    fn existing_group_spelling_wins_and_new_order_appends() {
        let groups = vec![
            ClipGroup {
                name: "Highlights".into(),
                order: 0,
            },
            ClipGroup {
                name: "Highlights".into(),
                order: 4,
            },
            ClipGroup {
                name: "Other".into(),
                order: 9,
            },
        ];

        assert_eq!(
            canonical_group(&groups, "highlights"),
            ("Highlights".into(), 5)
        );
        assert_eq!(canonical_group(&groups, "Fresh"), ("Fresh".into(), 0));
    }

    #[test]
    fn moving_a_group_member_swaps_only_with_its_neighbor() {
        let members = vec![
            GroupMember::test("a.mp4", 0),
            GroupMember::test("b.mp4", 1),
            GroupMember::test("c.mp4", 2),
        ];

        let moved = moved_members(members.clone(), "b.mp4", GroupMove::Down).unwrap();
        assert_eq!(member_paths(&moved), ["a.mp4", "c.mp4", "b.mp4"]);
        assert_eq!(
            moved_members(members.clone(), "a.mp4", GroupMove::Up).unwrap(),
            members
        );
        assert!(moved_members(members, "missing.mp4", GroupMove::Down).is_err());
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
        assert!(joined.contains("anullsrc=channel_layout=stereo:sample_rate=48000"));
        assert!(joined.contains("[0:a:0]aresample=48000"));
        assert!(joined.contains("[0:a:1]aresample=48000"));
        assert!(joined.contains(
            "amix=inputs=2:duration=longest:dropout_transition=0:normalize=1,asetpts=N/SR/TB[a0]"
        ));
        assert!(joined.contains("concat=n=2:v=1:a=1"));
        assert!(joined.contains("-c:v h264_mf"));
        assert!(joined.contains("-c:a libopus"));
        assert!(joined.ends_with("out.mp4"));
    }
}
