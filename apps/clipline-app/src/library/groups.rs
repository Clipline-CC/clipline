use super::*;

const MAX_GROUP_NAME_CHARS: usize = 80;
const MAX_COMPILATION_CLIPS: usize = 64;

#[derive(Clone, Debug, PartialEq)]
struct GroupMember {
    path: PathBuf,
    group: ClipGroup,
    modified_unix: u64,
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

fn group_name_key(name: &str) -> String {
    name.to_lowercase()
}

fn canonical_group(existing: &[ClipGroup], requested: &str) -> (String, u32) {
    let requested_key = group_name_key(requested);
    let matching: Vec<&ClipGroup> = existing
        .iter()
        .filter(|group| group_name_key(&group.name) == requested_key)
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
    let name_key = group_name_key(name);
    let mut members: Vec<GroupMember> = list_clips_from_dir(root.to_path_buf())?
        .clips
        .into_iter()
        .filter_map(|clip| {
            let group = clip.group?;
            if group_name_key(&group.name) != name_key {
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

fn windows_clip_path_key(path: &Path) -> String {
    let text = path.display().to_string().replace('/', r"\");
    let lower = text.to_ascii_lowercase();
    let normalized = if let Some(path) = lower.strip_prefix(r"\\?\unc\") {
        format!(r"\\{path}")
    } else if let Some(path) = lower.strip_prefix(r"\\?\") {
        path.to_string()
    } else {
        lower
    };
    format!("windows:{normalized}")
}

fn group_fingerprint(members: &[GroupMember]) -> String {
    members
        .iter()
        .map(|member| windows_clip_path_key(&member.path))
        .collect::<Vec<_>>()
        .join("\0")
}

fn reordered_members(
    mut members: Vec<GroupMember>,
    ordered_paths: &[PathBuf],
) -> Result<Vec<GroupMember>, String> {
    if ordered_paths.len() != members.len() {
        return Err("group reorder must include every member exactly once".into());
    }
    let mut by_path = HashMap::with_capacity(members.len());
    for member in members.drain(..) {
        if by_path
            .insert(windows_clip_path_key(&member.path), member)
            .is_some()
        {
            return Err("group contains duplicate clip paths".into());
        }
    }
    let mut seen = HashSet::with_capacity(ordered_paths.len());
    let mut reordered = Vec::with_capacity(ordered_paths.len());
    for path in ordered_paths {
        let key = windows_clip_path_key(path);
        if !seen.insert(key.clone()) {
            return Err("group reorder contains a duplicate clip".into());
        }
        let member = by_path
            .remove(&key)
            .ok_or_else(|| "group reorder contains a clip outside this group".to_string())?;
        reordered.push(member);
    }
    if !by_path.is_empty() {
        return Err("group reorder omitted a member".into());
    }
    for (order, member) in reordered.iter_mut().enumerate() {
        member.group.order = order as u32;
    }
    Ok(reordered)
}

fn persist_group_order(members: &[GroupMember]) -> Result<(), String> {
    let mut updates = Vec::new();
    for member in members {
        let metadata_path = clip_metadata_path(&member.path);
        let json = std::fs::read_to_string(&metadata_path)
            .map_err(|error| format!("read group clip metadata {metadata_path:?}: {error}"))?;
        let previous: ClipMetadata = serde_json::from_str(&json)
            .map_err(|error| format!("parse group clip metadata {metadata_path:?}: {error}"))?;
        if previous.group.as_ref() == Some(&member.group) {
            continue;
        }
        let mut next = previous.clone();
        next.group = Some(member.group.clone());
        updates.push((member.path.clone(), previous, next));
    }

    for index in 0..updates.len() {
        let (path, _, next) = &updates[index];
        if let Err(error) = write_clip_metadata(path, next) {
            let mut rollback_errors = Vec::new();
            for (rollback_path, previous, _) in updates[..index].iter().rev() {
                if let Err(rollback) = write_clip_metadata(rollback_path, previous) {
                    rollback_errors.push(rollback);
                }
            }
            return if rollback_errors.is_empty() {
                Err(error)
            } else {
                Err(format!(
                    "{error}; rollback group order: {}",
                    rollback_errors.join("; ")
                ))
            };
        }
    }
    Ok(())
}

fn remove_from_group_file(path: &Path) -> Result<(), String> {
    let mut metadata = read_clip_metadata(path).unwrap_or_default();
    if metadata.group.take().is_none() {
        return Err("clip is not in a group".into());
    }
    write_clip_metadata(path, &metadata)
}

#[tauri::command]
pub async fn reorder_group(
    name: String,
    ordered_paths: Vec<String>,
    settings: tauri::State<'_, StorageSettings>,
) -> Result<Vec<GroupOrderUpdate>, String> {
    let name = normalized_group_name(&name)?;
    let ordered_paths = ordered_paths
        .iter()
        .map(|path| validate_clip_path(&settings, path))
        .collect::<Result<Vec<_>, _>>()?;
    let root = settings.clips_dir()?;
    tauri::async_runtime::spawn_blocking(move || {
        let members = reordered_members(group_members(&root, &name)?, &ordered_paths)?;
        persist_group_order(&members)?;
        Ok(members
            .into_iter()
            .map(|member| GroupOrderUpdate {
                path: member.path.display().to_string(),
                order: member.group.order,
            })
            .collect())
    })
    .await
    .map_err(|error| format!("reorder group task: {error}"))?
}

#[tauri::command]
pub async fn remove_from_group(
    path: String,
    settings: tauri::State<'_, StorageSettings>,
) -> Result<(), String> {
    let target = validate_clip_path(&settings, &path)?;
    tauri::async_runtime::spawn_blocking(move || remove_from_group_file(&target))
        .await
        .map_err(|error| format!("remove from group task: {error}"))?
}

#[tauri::command]
pub async fn export_group<R: Runtime>(
    app: AppHandle<R>,
    name: String,
    settings: tauri::State<'_, StorageSettings>,
    exports: tauri::State<'_, ClipboardExportState>,
) -> Result<ClipInfo, String> {
    let name = normalized_group_name(&name)?;
    let root = settings.clips_dir()?;
    let scope_root = root.clone();
    let job = exports.begin();
    let exported =
        tauri::async_runtime::spawn_blocking(move || export_group_file(&root, &name, &job))
            .await
            .map_err(|error| format!("export group task: {error}"))??;
    allow_local_clip_asset(&app, &scope_root, Path::new(&exported.path))?;
    Ok(exported)
}

fn export_group_file(
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
        source_group_fingerprint: Some(fingerprint),
    })
}

fn compilation_duration_s(target: &Path, inputs: &[CompilationInput]) -> f64 {
    clipline_mp4::movie_duration_s_file(target)
        .ok()
        .flatten()
        .filter(|duration| duration.is_finite() && *duration > 0.0)
        .unwrap_or_else(|| inputs.iter().map(|input| input.duration_s).sum())
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

fn validate_compilation_command_line(ffmpeg: &Path, args: &[String]) -> Result<(), String> {
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

fn run_compilation_ffmpeg(
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
        assert_eq!(
            canonical_group(
                &[ClipGroup {
                    name: "Éclair".into(),
                    order: 2,
                }],
                "éCLAIR",
            ),
            ("Éclair".into(), 3)
        );
    }

    #[test]
    fn reordering_matches_verbatim_windows_paths_and_rejects_bad_payloads() {
        let members = vec![
            GroupMember::test(r"D:\Clips\a.mp4", 0),
            GroupMember::test(r"D:\Clips\b.mp4", 1),
            GroupMember::test(r"D:\Clips\c.mp4", 2),
        ];

        let reordered = reordered_members(
            members.clone(),
            &[
                PathBuf::from(r"\\?\D:\Clips\c.mp4"),
                PathBuf::from(r"\\?\D:\Clips\a.mp4"),
                PathBuf::from(r"\\?\D:\Clips\b.mp4"),
            ],
        )
        .unwrap();
        assert_eq!(
            member_paths(&reordered),
            [r"D:\Clips\c.mp4", r"D:\Clips\a.mp4", r"D:\Clips\b.mp4"]
        );
        assert_eq!(
            reordered
                .iter()
                .map(|member| member.group.order)
                .collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert!(reordered_members(
            members.clone(),
            &[
                PathBuf::from(r"D:\Clips\a.mp4"),
                PathBuf::from(r"D:\Clips\a.mp4"),
                PathBuf::from(r"D:\Clips\c.mp4"),
            ],
        )
        .is_err());
        assert!(reordered_members(
            members,
            &[
                PathBuf::from(r"D:\Clips\a.mp4"),
                PathBuf::from(r"D:\Clips\b.mp4"),
                PathBuf::from(r"D:\Clips\missing.mp4"),
            ],
        )
        .is_err());
    }

    #[test]
    fn group_fingerprint_is_path_spelling_independent_and_order_sensitive() {
        let plain = vec![
            GroupMember::test(r"D:\Clips\A.mp4", 0),
            GroupMember::test(r"D:\Clips\b.mp4", 1),
        ];
        let verbatim = vec![
            GroupMember::test(r"\\?\d:\clips\a.mp4", 0),
            GroupMember::test(r"\\?\D:\Clips\B.mp4", 1),
        ];
        let reversed = vec![plain[1].clone(), plain[0].clone()];

        assert_eq!(
            group_fingerprint(&plain),
            "windows:d:\\clips\\a.mp4\0windows:d:\\clips\\b.mp4"
        );
        assert_eq!(group_fingerprint(&plain), group_fingerprint(&verbatim));
        assert_ne!(group_fingerprint(&plain), group_fingerprint(&reversed));
    }

    #[test]
    fn failed_group_order_write_rolls_back_earlier_sidecars() {
        let dir = clipline_test_utils::TestDir::new("clipline-groups", "reorder-rollback");
        let a = dir.path().join("a.mp4");
        let b = dir.path().join("b.mp4");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        write_clip_metadata(
            &a,
            &ClipMetadata {
                group: Some(ClipGroup {
                    name: "Highlights".into(),
                    order: 0,
                }),
                ..ClipMetadata::default()
            },
        )
        .unwrap();
        write_clip_metadata(
            &b,
            &ClipMetadata {
                group: Some(ClipGroup {
                    name: "Highlights".into(),
                    order: 1,
                }),
                ..ClipMetadata::default()
            },
        )
        .unwrap();
        std::fs::create_dir(clip_metadata_path(&a).with_extension("clipline.json.tmp")).unwrap();
        let reordered = vec![
            GroupMember::test(b.to_str().unwrap(), 0),
            GroupMember::test(a.to_str().unwrap(), 1),
        ];

        assert!(persist_group_order(&reordered).is_err());
        assert_eq!(
            read_clip_metadata(&b)
                .and_then(|metadata| metadata.group)
                .map(|group| group.order),
            Some(1)
        );
    }

    #[test]
    fn reorder_refuses_to_overwrite_corrupt_member_metadata() {
        let dir = clipline_test_utils::TestDir::new("clipline-groups", "reorder-corrupt");
        let clip = dir.path().join("clip.mp4");
        std::fs::write(&clip, b"clip").unwrap();
        std::fs::write(clip_metadata_path(&clip), b"{").unwrap();
        let members = vec![GroupMember::test(clip.to_str().unwrap(), 1)];

        assert!(persist_group_order(&members).is_err());
        assert_eq!(std::fs::read(clip_metadata_path(&clip)).unwrap(), b"{");
    }

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
    fn removing_a_member_clears_only_its_group_metadata() {
        let dir = clipline_test_utils::TestDir::new("clipline-groups", "ungroup");
        let clip = dir.path().join("clip.mp4");
        std::fs::write(&clip, b"clip").unwrap();
        write_clip_metadata(
            &clip,
            &ClipMetadata {
                title: Some("Keep me".into()),
                group: Some(ClipGroup {
                    name: "Highlights".into(),
                    order: 2,
                }),
                ..ClipMetadata::default()
            },
        )
        .unwrap();

        remove_from_group_file(&clip).unwrap();
        let metadata = read_clip_metadata(&clip).unwrap();
        assert_eq!(metadata.title.as_deref(), Some("Keep me"));
        assert_eq!(metadata.group, None);
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
