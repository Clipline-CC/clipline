use super::*;
use std::io::Write;

pub(crate) const MAX_GROUP_NAME_CHARS: usize = 80;

pub(crate) const MAX_COMPILATION_CLIPS: usize = 64;

pub(crate) const GROUP_ORDER_JOURNAL_FILE: &str = ".clipline-group-order.json";

pub(crate) const GROUP_ORDER_COMMITTED_FILE: &str = ".clipline-group-order.committed";

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GroupMember {
    pub(crate) path: PathBuf,
    pub(crate) group: ClipGroup,
    pub(crate) modified_unix: u64,
}

#[derive(serde::Serialize)]
pub struct GroupOrderUpdate {
    pub path: String,
    pub order: u32,
}

#[derive(Clone, Debug, PartialEq)]
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct GroupOrderJournal {
    pub(crate) entries: Vec<GroupOrderJournalEntry>,
}

#[derive(Clone, Debug, PartialEq)]
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct GroupOrderJournalEntry {
    pub(crate) relative_path: PathBuf,
    pub(crate) previous: ClipMetadata,
}

pub(crate) fn group_for_export(root: &Path, requested: &str) -> Result<ClipGroup, String> {
    let name = normalized_group_name(requested)?;
    let existing: Vec<ClipGroup> = list_clips_from_dir(root.to_path_buf())?
        .clips
        .into_iter()
        .filter_map(|clip| clip.group)
        .collect();
    let (name, order) = canonical_group(&existing, &name);
    Ok(ClipGroup { name, order })
}

pub(crate) fn normalized_group_name(input: &str) -> Result<String, String> {
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

pub(crate) fn group_name_key(name: &str) -> String {
    name.to_lowercase()
}

pub(crate) fn canonical_group(existing: &[ClipGroup], requested: &str) -> (String, u32) {
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

pub(crate) fn group_members(root: &Path, name: &str) -> Result<Vec<GroupMember>, String> {
    recover_group_order_transaction(root)?;
    group_members_unrecovered(root, name)
}

pub(crate) fn group_members_unrecovered(root: &Path, name: &str) -> Result<Vec<GroupMember>, String> {
    let name_key = group_name_key(name);
    let mut members: Vec<GroupMember> =
        list_clips_from_dir_with_child_reader(root.to_path_buf(), push_clips_from)?
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

pub(crate) fn sort_group_members(members: &mut [GroupMember]) {
    members.sort_by(|left, right| {
        left.group
            .order
            .cmp(&right.group.order)
            .then(left.modified_unix.cmp(&right.modified_unix))
            .then_with(|| left.path.cmp(&right.path))
    });
}

pub(crate) fn windows_clip_path_key(path: &Path) -> String {
    let text = path.display().to_string().replace('/', r"\");
    let lower = text.to_lowercase();
    let normalized = if let Some(path) = lower.strip_prefix(r"\\?\unc\") {
        format!(r"\\{path}")
    } else if let Some(path) = lower.strip_prefix(r"\\?\") {
        path.to_string()
    } else {
        lower
    };
    format!("windows:{normalized}")
}

pub(crate) fn group_fingerprint(members: &[GroupMember]) -> String {
    members
        .iter()
        .map(|member| windows_clip_path_key(&member.path))
        .collect::<Vec<_>>()
        .join("\0")
}

pub(crate) fn reordered_members(
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

pub(crate) fn group_order_journal_path(root: &Path) -> PathBuf {
    root.join(GROUP_ORDER_JOURNAL_FILE)
}

pub(crate) fn group_order_committed_path(root: &Path) -> PathBuf {
    root.join(GROUP_ORDER_COMMITTED_FILE)
}

pub(crate) fn canonical_group_clip(root: &Path, path: &Path) -> Result<(PathBuf, PathBuf), String> {
    let canonical_root = root
        .canonicalize()
        .map_err(|error| format!("resolve group root {root:?}: {error}"))?;
    let canonical_path = path
        .canonicalize()
        .map_err(|error| format!("resolve group clip {path:?}: {error}"))?;
    let relative = canonical_path
        .strip_prefix(&canonical_root)
        .map_err(|_| format!("group clip {canonical_path:?} escaped root {canonical_root:?}"))?
        .to_path_buf();
    let components = relative.components().collect::<Vec<_>>();
    if !(components.len() == 1 || components.len() == 2)
        || components
            .iter()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
        || canonical_path.extension().and_then(|value| value.to_str()) != Some("mp4")
    {
        return Err(format!("invalid group journal clip path {relative:?}"));
    }
    Ok((canonical_path, relative))
}

pub(crate) fn write_group_order_journal(
    root: &Path,
    entries: &[(PathBuf, ClipMetadata)],
) -> Result<(), String> {
    let target = group_order_journal_path(root);
    if target.exists() {
        return Err("a group reorder recovery is already pending".into());
    }
    let entries = entries
        .iter()
        .map(|(path, previous)| {
            let (_, relative_path) = canonical_group_clip(root, path)?;
            Ok(GroupOrderJournalEntry {
                relative_path,
                previous: previous.clone(),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let bytes = serde_json::to_vec_pretty(&GroupOrderJournal { entries })
        .map_err(|error| format!("serialize group order journal: {error}"))?;
    let tmp = crate::settings::persistence::sibling_tmp_path(&target)?;
    let result = (|| {
        let mut file = std::fs::File::create(&tmp)
            .map_err(|error| format!("create group order journal: {error}"))?;
        file.write_all(&bytes)
            .map_err(|error| format!("write group order journal: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("sync group order journal: {error}"))?;
        crate::windows::replace_file(&tmp, &target)
            .map_err(|error| format!("publish group order journal: {error}"))?;
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&target)
            .and_then(|file| file.sync_all())
            .map_err(|error| format!("sync published group order journal: {error}"))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

pub(crate) fn recover_group_order_transaction_unlocked(root: &Path) -> Result<(), String> {
    let committed_path = group_order_committed_path(root);
    if committed_path.is_file() {
        let _ = std::fs::remove_file(committed_path);
    } else if committed_path.exists() {
        return Err("group order commit marker is not a file".into());
    }
    let journal_path = group_order_journal_path(root);
    if !journal_path.exists() {
        return Ok(());
    }
    if !journal_path.is_file() {
        return Err("group order recovery path is not a file".into());
    }
    let json = std::fs::read_to_string(&journal_path)
        .map_err(|error| format!("read group order journal: {error}"))?;
    let journal: GroupOrderJournal = serde_json::from_str(&json)
        .map_err(|error| format!("parse group order journal: {error}"))?;
    for entry in journal.entries {
        let (path, _) = canonical_group_clip(root, &root.join(entry.relative_path))?;
        write_clip_metadata(&path, &entry.previous)?;
    }
    std::fs::remove_file(&journal_path)
        .map_err(|error| format!("finish group order recovery: {error}"))
}

pub(crate) fn recover_group_order_transaction(root: &Path) -> Result<(), String> {
    let _guard = crate::gc::lock_clip_mutations();
    recover_group_order_transaction_unlocked(root)
}

#[cfg(test)]
pub(crate) fn persist_group_order(root: &Path, members: &[GroupMember]) -> Result<(), String> {
    let _guard = crate::gc::lock_clip_mutations();
    recover_group_order_transaction_unlocked(root)?;
    persist_group_order_unlocked(root, members)
}

pub(crate) fn persist_group_order_unlocked(root: &Path, members: &[GroupMember]) -> Result<(), String> {
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
    if updates.is_empty() {
        return Ok(());
    }
    let previous = updates
        .iter()
        .map(|(path, previous, _)| (path.clone(), previous.clone()))
        .collect::<Vec<_>>();
    write_group_order_journal(root, &previous)?;

    for (path, _, next) in &updates {
        if let Err(error) = write_clip_metadata(path, next) {
            return match recover_group_order_transaction_unlocked(root) {
                Ok(()) => Err(error),
                Err(rollback) => Err(format!("{error}; rollback group order: {rollback}")),
            };
        }
    }
    let journal_path = group_order_journal_path(root);
    let committed_path = group_order_committed_path(root);
    if let Err(error) = crate::windows::replace_file(&journal_path, &committed_path) {
        return match recover_group_order_transaction_unlocked(root) {
            Ok(()) => Err(format!("finish group reorder: {error}")),
            Err(rollback) => Err(format!(
                "finish group reorder: {error}; rollback group order: {rollback}"
            )),
        };
    }
    let _ = std::fs::remove_file(committed_path);
    Ok(())
}

pub(crate) fn remove_from_group_file(path: &Path) -> Result<(), String> {
    let _guard = crate::gc::lock_clip_mutations();
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
        let _guard = crate::gc::lock_clip_mutations();
        recover_group_order_transaction_unlocked(&root)?;
        let members = reordered_members(group_members_unrecovered(&root, &name)?, &ordered_paths)?;
        persist_group_order_unlocked(&root, &members)?;
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

#[cfg(test)]
impl GroupMember {
    pub(crate) fn test(path: &str, order: u32) -> Self {
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
                GroupMember::test(r"D:\Clips\ÉCLAIR.mp4", 0),
                GroupMember::test(r"D:\Clips\b.mp4", 1),
            ];
            let verbatim = vec![
                GroupMember::test(r"\\?\d:\clips\éclair.mp4", 0),
                GroupMember::test(r"\\?\D:\Clips\B.mp4", 1),
            ];
            let reversed = vec![plain[1].clone(), plain[0].clone()];

            assert_eq!(
                group_fingerprint(&plain),
                "windows:d:\\clips\\éclair.mp4\0windows:d:\\clips\\b.mp4"
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

            assert!(persist_group_order(dir.path(), &reordered).is_err());
            assert_eq!(
                read_clip_metadata(&b)
                    .and_then(|metadata| metadata.group)
                    .map(|group| group.order),
                Some(1)
            );
        }
        #[test]
        fn durable_journal_blocks_scans_until_rollback_can_finish() {
            let dir = clipline_test_utils::TestDir::new("clipline-groups", "reorder-journal");
            let clip = dir.path().join("clip.mp4");
            std::fs::write(&clip, b"clip").unwrap();
            let previous = ClipMetadata {
                group: Some(ClipGroup {
                    name: "Highlights".into(),
                    order: 0,
                }),
                ..ClipMetadata::default()
            };
            write_clip_metadata(&clip, &previous).unwrap();
            write_group_order_journal(dir.path(), &[(clip.clone(), previous)]).unwrap();
            write_clip_metadata(
                &clip,
                &ClipMetadata {
                    group: Some(ClipGroup {
                        name: "Highlights".into(),
                        order: 1,
                    }),
                    ..ClipMetadata::default()
                },
            )
            .unwrap();
            let blocked_tmp = clip_metadata_path(&clip).with_extension("clipline.json.tmp");
            std::fs::create_dir(&blocked_tmp).unwrap();

            assert!(list_clips_from_dir(dir.path().to_path_buf()).is_err());
            assert!(group_order_journal_path(dir.path()).is_file());
            assert_eq!(
                read_clip_metadata(&clip)
                    .and_then(|metadata| metadata.group)
                    .map(|group| group.order),
                Some(1)
            );

            std::fs::remove_dir(blocked_tmp).unwrap();
            recover_group_order_transaction(dir.path()).unwrap();
            assert!(!group_order_journal_path(dir.path()).exists());
            assert_eq!(
                read_clip_metadata(&clip)
                    .and_then(|metadata| metadata.group)
                    .map(|group| group.order),
                Some(0)
            );

            write_clip_metadata(
                &clip,
                &ClipMetadata {
                    group: Some(ClipGroup {
                        name: "Highlights".into(),
                        order: 2,
                    }),
                    ..ClipMetadata::default()
                },
            )
            .unwrap();
            std::fs::write(group_order_committed_path(dir.path()), b"").unwrap();
            recover_group_order_transaction(dir.path()).unwrap();
            assert!(!group_order_committed_path(dir.path()).exists());
            assert_eq!(
                read_clip_metadata(&clip)
                    .and_then(|metadata| metadata.group)
                    .map(|group| group.order),
                Some(2)
            );
        }
        #[test]
        fn reorder_refuses_to_overwrite_corrupt_member_metadata() {
            let dir = clipline_test_utils::TestDir::new("clipline-groups", "reorder-corrupt");
            let clip = dir.path().join("clip.mp4");
            std::fs::write(&clip, b"clip").unwrap();
            std::fs::write(clip_metadata_path(&clip), b"{").unwrap();
            let members = vec![GroupMember::test(clip.to_str().unwrap(), 1)];

            assert!(persist_group_order(dir.path(), &members).is_err());
            assert_eq!(std::fs::read(clip_metadata_path(&clip)).unwrap(), b"{");
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
}
