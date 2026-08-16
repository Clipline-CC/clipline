use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

const CLEANUP_ARGUMENT: &str = "--uninstall-cleanup";
const DELETE_RECORDINGS_ARGUMENT: &str = "--delete-recordings";
const CREDENTIAL_PREFIXES: [&str; 2] = ["Clipline Cloud:", "Clipline osu!:"];
const AUTOSTART_NAMES: [&str; 2] = ["clipline-app", "Clipline"];
const REPLAY_CACHE_OWNER_MAX_BYTES: u64 = 4096;

#[derive(Debug, Clone)]
struct CleanupLayout {
    config_dir: PathBuf,
    local_cache_dir: PathBuf,
    temp_dir: PathBuf,
    local_identifier_dir: PathBuf,
    roaming_identifier_dir: PathBuf,
    profile_dir: PathBuf,
    default_media_dir: PathBuf,
}

#[cfg(windows)]
impl CleanupLayout {
    fn system() -> Self {
        let config_dir = crate::settings::persistence::config_base();
        let local_cache_dir = crate::settings::persistence::local_cache_base();
        let roaming_base = config_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| config_dir.clone());
        let local_base = local_cache_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| local_cache_dir.clone());
        let profile_dir = std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_default();
        Self {
            config_dir,
            local_cache_dir,
            temp_dir: std::env::temp_dir().join("Clipline"),
            local_identifier_dir: local_base.join("io.clipline.app"),
            roaming_identifier_dir: roaming_base.join("io.clipline.app"),
            profile_dir,
            default_media_dir: crate::service::default_clips_dir(),
        }
    }
}

#[derive(Debug, Default)]
struct SavedCleanupSettings {
    media_dir: Option<PathBuf>,
    replay_dir: Option<PathBuf>,
    credential_targets: Vec<String>,
}

#[derive(Debug)]
struct CleanupPlan {
    remove_trees: Vec<PathBuf>,
    replay_trees: Vec<PathBuf>,
    media_dirs: Vec<PathBuf>,
    default_media_dir: Option<PathBuf>,
    delete_recordings: bool,
    credential_targets: Vec<String>,
}

fn cleanup_request<I, S>(args: I) -> Option<bool>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args: Vec<S> = args.into_iter().collect();
    args.iter()
        .any(|arg| arg.as_ref() == CLEANUP_ARGUMENT)
        .then(|| {
            args.iter()
                .any(|arg| arg.as_ref() == DELETE_RECORDINGS_ARGUMENT)
        })
}

#[cfg(windows)]
pub(crate) fn run_if_requested() -> bool {
    let Some(delete_recordings) = cleanup_request(std::env::args()) else {
        return false;
    };
    let plan = build_cleanup_plan(&CleanupLayout::system(), delete_recordings);
    let swept =
        crate::windows::credential_targets_with_prefixes(&CREDENTIAL_PREFIXES).unwrap_or_default();
    execute_cleanup_plan_with(
        &plan,
        swept.iter().map(String::as_str),
        |target| {
            crate::windows::CredentialStore::new("Clipline credential").delete_if_present(target)
        },
        crate::windows::delete_autostart_value,
    );
    true
}

fn build_cleanup_plan(layout: &CleanupLayout, delete_recordings: bool) -> CleanupPlan {
    let settings = load_saved_settings(&layout.config_dir);
    let configured_media = settings
        .media_dir
        .unwrap_or_else(|| layout.default_media_dir.clone());
    let default_media_dir =
        safe_media_dir(&layout.default_media_dir, layout).then(|| layout.default_media_dir.clone());
    let mut media_dirs = [configured_media, layout.default_media_dir.clone()]
        .into_iter()
        .filter(|media| safe_media_dir(media, layout))
        .collect::<Vec<_>>();
    dedup_paths(&mut media_dirs);

    let mut remove_trees = vec![layout.config_dir.clone(), layout.temp_dir.clone()];
    remove_trees.extend(
        [
            "ffmpeg",
            "ffmpeg-staging",
            "cloud-cache",
            "support-staging",
            "EBWebView",
        ]
        .map(|child| layout.local_cache_dir.join(child)),
    );
    remove_trees.push(layout.local_identifier_dir.clone());
    remove_trees.push(layout.roaming_identifier_dir.clone());

    let mut replay_trees = Vec::new();
    if let Some(replay_dir) = settings.replay_dir.as_deref() {
        let overlaps_media = media_dirs
            .iter()
            .any(|media| same_or_nested(replay_dir, media));
        if safe_replay_dir(replay_dir, layout) && (!overlaps_media || delete_recordings) {
            collect_replay_run_dirs(replay_dir, &mut replay_trees);
        }
    }

    dedup_paths(&mut remove_trees);
    dedup_paths(&mut replay_trees);
    CleanupPlan {
        remove_trees,
        replay_trees,
        media_dirs,
        default_media_dir,
        delete_recordings,
        credential_targets: settings.credential_targets,
    }
}

fn load_saved_settings(config_dir: &Path) -> SavedCleanupSettings {
    [
        config_dir.join("settings.json"),
        config_dir.join("settings.json.bak"),
    ]
    .into_iter()
    .find_map(|path| {
        let bytes = fs::read(path).ok()?;
        let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        value.as_object()?;
        Some(saved_settings_from_json(&value))
    })
    .unwrap_or_default()
}

fn saved_settings_from_json(value: &serde_json::Value) -> SavedCleanupSettings {
    let path_at = |pointer: &str| {
        value
            .pointer(pointer)
            .and_then(serde_json::Value::as_str)
            .filter(|path| !path.trim().is_empty())
            .map(PathBuf::from)
    };
    let replay_dir = (value
        .pointer("/replay_storage/mode")
        .and_then(|v| v.as_str())
        == Some("disk"))
    .then(|| path_at("/replay_storage/disk_dir"))
    .flatten();
    let mut credential_targets = Vec::new();
    for section in ["cloud", "osu"] {
        if let Some(target) = value
            .pointer(&format!("/{section}/credential_target"))
            .and_then(serde_json::Value::as_str)
        {
            push_unique_credential(&mut credential_targets, target);
        }
        if let Some(targets) = value
            .pointer(&format!("/{section}/credential_cleanup_targets"))
            .and_then(serde_json::Value::as_array)
        {
            for target in targets.iter().filter_map(serde_json::Value::as_str) {
                push_unique_credential(&mut credential_targets, target);
            }
        }
    }
    SavedCleanupSettings {
        media_dir: path_at("/media_dir"),
        replay_dir,
        credential_targets,
    }
}

fn safe_media_dir(path: &Path, layout: &CleanupLayout) -> bool {
    safe_absolute_non_root(path)
        && ![
            &layout.config_dir,
            &layout.local_cache_dir,
            &layout.temp_dir,
            &layout.local_identifier_dir,
            &layout.roaming_identifier_dir,
            &layout.profile_dir,
        ]
        .into_iter()
        .any(|protected| same_path(path, protected) || same_or_nested(protected, path))
}

fn safe_replay_dir(path: &Path, layout: &CleanupLayout) -> bool {
    safe_absolute_non_root(path)
        && ![
            &layout.config_dir,
            &layout.local_cache_dir,
            &layout.profile_dir,
        ]
        .into_iter()
        .any(|protected| same_path(path, protected) || same_or_nested(protected, path))
}

fn safe_absolute_non_root(path: &Path) -> bool {
    path.is_absolute() && path.parent().is_some()
}

fn collect_replay_run_dirs(root: &Path, output: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_dir()
            || is_link_or_reparse_point(&metadata)
            || !has_valid_replay_cache_owner(&path, name)
        {
            continue;
        }
        output.push(path);
    }
}

fn has_valid_replay_cache_owner(run_dir: &Path, run_name: &str) -> bool {
    let Some(run_pid) = clipline_storage::replay_cache_run_pid(run_name) else {
        return false;
    };
    let owner_path = run_dir.join(clipline_storage::REPLAY_CACHE_OWNER_FILE);
    let Ok(metadata) = fs::symlink_metadata(&owner_path) else {
        return false;
    };
    if !metadata.is_file()
        || is_link_or_reparse_point(&metadata)
        || metadata.len() > REPLAY_CACHE_OWNER_MAX_BYTES
    {
        return false;
    }
    let Ok(bytes) = fs::read(owner_path) else {
        return false;
    };
    let Ok(owner) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    owner
        .get("created_at_unix")
        .and_then(serde_json::Value::as_u64)
        .is_some()
        && owner
            .get("process_instance_id")
            .and_then(serde_json::Value::as_str)
            .and_then(clipline_storage::replay_cache_owner_pid)
            == Some(run_pid)
}

fn execute_cleanup_plan_with<'a, I>(
    plan: &CleanupPlan,
    enumerated_credentials: I,
    mut delete_credential: impl FnMut(&str) -> Result<(), String>,
    mut delete_autostart: impl FnMut(&str) -> Result<(), String>,
) where
    I: IntoIterator<Item = &'a str>,
{
    if plan.delete_recordings {
        for media_dir in &plan.media_dirs {
            let _ = clipline_storage::delete_all_managed_media(media_dir);
        }
    }
    for path in &plan.replay_trees {
        remove_tree_best_effort(path);
    }
    for path in &plan.remove_trees {
        remove_tree_best_effort_except(path, &plan.media_dirs);
    }
    if plan.delete_recordings {
        if let Some(default_media_dir) = plan.default_media_dir.as_deref() {
            let _ = fs::remove_dir(default_media_dir);
        }
    }

    let mut seen = HashSet::new();
    for target in &plan.credential_targets {
        if is_clipline_credential_target(target) && seen.insert(target.to_owned()) {
            let _ = delete_credential(target);
        }
    }
    for target in enumerated_credentials {
        if is_clipline_credential_target(target) && seen.insert(target.to_owned()) {
            let _ = delete_credential(target);
        }
    }
    for name in AUTOSTART_NAMES {
        let _ = delete_autostart(name);
    }
}

fn remove_tree_best_effort(path: &Path) {
    remove_tree_best_effort_except(path, &[]);
}

fn remove_tree_best_effort_except(path: &Path, protected_roots: &[PathBuf]) {
    if protected_roots
        .iter()
        .any(|protected| same_or_nested(path, protected))
    {
        return;
    }
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if is_link_or_reparse_point(&metadata) {
        let _ = fs::remove_file(path).or_else(|_| fs::remove_dir(path));
    } else if metadata.is_dir() {
        let contains_protected = protected_roots
            .iter()
            .any(|protected| same_or_nested(protected, path));
        if contains_protected {
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    remove_tree_best_effort_except(&entry.path(), protected_roots);
                }
            }
            let _ = fs::remove_dir(path);
        } else {
            let _ = fs::remove_dir_all(path);
        }
    } else {
        let _ = fs::remove_file(path);
    }
}

fn is_link_or_reparse_point(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    path_key(left) == path_key(right)
}

fn same_or_nested(path: &Path, root: &Path) -> bool {
    let path = path_key(path);
    let root = path_key(root);
    path == root
        || path
            .strip_prefix(&root)
            .is_some_and(|suffix| suffix.starts_with(['/', '\\']))
}

fn path_key(path: &Path) -> String {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    path.to_string_lossy()
        .trim_end_matches(['/', '\\'])
        .to_ascii_lowercase()
}

fn dedup_paths(paths: &mut Vec<PathBuf>) {
    let mut seen = HashSet::new();
    paths.retain(|path| seen.insert(path_key(path)));
}

fn is_clipline_credential_target(target: &str) -> bool {
    CREDENTIAL_PREFIXES
        .iter()
        .any(|prefix| target.starts_with(prefix))
}

fn push_unique_credential(targets: &mut Vec<String>, target: &str) {
    if is_clipline_credential_target(target) && !targets.iter().any(|existing| existing == target) {
        targets.push(target.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_test_utils::TestDir;
    use std::cell::RefCell;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn layout(root: &Path) -> CleanupLayout {
        CleanupLayout {
            config_dir: root.join("Roaming/Clipline"),
            local_cache_dir: root.join("Local/Clipline"),
            temp_dir: root.join("Temp/Clipline"),
            local_identifier_dir: root.join("Local/io.clipline.app"),
            roaming_identifier_dir: root.join("Roaming/io.clipline.app"),
            profile_dir: root.join("Users/Alice"),
            default_media_dir: root.join("Users/Alice/Videos/Clipline"),
        }
    }

    fn write_settings(layout: &CleanupLayout, json: &str) {
        fs::create_dir_all(&layout.config_dir).unwrap();
        fs::write(layout.config_dir.join("settings.json"), json).unwrap();
    }

    fn contains_path(paths: &[PathBuf], expected: &Path) -> bool {
        paths.iter().any(|path| path == expected)
    }

    #[test]
    fn cleanup_request_is_routed_before_normal_app_startup() {
        assert_eq!(
            cleanup_request(["Clipline.exe", "--uninstall-cleanup"]),
            Some(false)
        );
        assert_eq!(
            cleanup_request(["Clipline.exe", "--uninstall-cleanup", "--delete-recordings",]),
            Some(true)
        );
        assert_eq!(cleanup_request(["Clipline.exe", "--autostart"]), None);
    }

    #[test]
    fn plan_wipes_only_known_residue_and_preserves_recordings_by_default() {
        let root = TestDir::new("clipline-uninstall", "known-residue");
        let layout = layout(root.path());
        let media = root.path().join("Custom Media");
        write_settings(
            &layout,
            &format!(r#"{{"media_dir":{:?}}}"#, media.display().to_string()),
        );

        let plan = build_cleanup_plan(&layout, false);

        assert!(contains_path(&plan.remove_trees, &layout.config_dir));
        assert!(contains_path(&plan.remove_trees, &layout.temp_dir));
        for child in [
            "ffmpeg",
            "ffmpeg-staging",
            "cloud-cache",
            "support-staging",
            "EBWebView",
        ] {
            assert!(contains_path(
                &plan.remove_trees,
                &layout.local_cache_dir.join(child)
            ));
        }
        assert!(!contains_path(&plan.remove_trees, &layout.local_cache_dir));
        assert!(!plan.delete_recordings);
        assert!(contains_path(&plan.media_dirs, &media));
        assert!(!plan
            .remove_trees
            .iter()
            .any(|path| { path.ends_with(Path::new("Microsoft").join("EdgeWebView")) }));
    }

    #[test]
    fn opted_in_cleanup_deletes_managed_media_but_leaves_foreign_files_and_custom_root() {
        let root = TestDir::new("clipline-uninstall", "delete-recordings");
        let layout = layout(root.path());
        let media = root.path().join("Custom Media");
        let managed = media.join("owned.mp4");
        let foreign = media.join("foreign.mp4");
        fs::create_dir_all(&media).unwrap();
        fs::write(&managed, b"owned").unwrap();
        fs::write(managed.with_extension("clipline.json"), b"{}").unwrap();
        fs::write(&foreign, b"foreign").unwrap();
        write_settings(
            &layout,
            &format!(r#"{{"media_dir":{:?}}}"#, media.display().to_string()),
        );

        let plan = build_cleanup_plan(&layout, true);
        execute_cleanup_plan_with(&plan, [], |_| Ok(()), |_| Ok(()));

        assert!(!managed.exists());
        assert!(foreign.exists());
        assert!(media.exists(), "custom media directories are user-owned");
    }

    #[test]
    fn executor_removes_known_residue_without_removing_local_cache_root() {
        let root = TestDir::new("clipline-uninstall", "execute-residue");
        let layout = layout(root.path());
        write_settings(&layout, "{}");
        for path in [
            layout.temp_dir.join("logs/clipline.log"),
            layout.local_cache_dir.join("ffmpeg/ffmpeg.exe"),
            layout.local_cache_dir.join("EBWebView/Cache/data"),
            layout.local_identifier_dir.join("cache/data"),
            layout.roaming_identifier_dir.join("state/data"),
        ] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"residue").unwrap();
        }
        let unrelated = layout.local_cache_dir.join("leave-me.txt");
        fs::write(&unrelated, b"unrelated").unwrap();

        let plan = build_cleanup_plan(&layout, false);
        execute_cleanup_plan_with(&plan, [], |_| Ok(()), |_| Ok(()));

        assert!(!layout.config_dir.exists());
        assert!(!layout.temp_dir.exists());
        assert!(!layout.local_identifier_dir.exists());
        assert!(!layout.roaming_identifier_dir.exists());
        assert!(layout.local_cache_dir.exists());
        assert!(unrelated.exists());
    }

    #[test]
    fn opted_in_cleanup_removes_only_an_empty_default_media_root() {
        let root = TestDir::new("clipline-uninstall", "default-media-root");
        let layout = layout(root.path());
        let clip = layout.default_media_dir.join("owned.mp4");
        fs::create_dir_all(&layout.default_media_dir).unwrap();
        fs::write(&clip, b"owned").unwrap();
        fs::write(clip.with_extension("clipline.json"), b"{}").unwrap();
        write_settings(&layout, "{}");

        let plan = build_cleanup_plan(&layout, true);
        execute_cleanup_plan_with(&plan, [], |_| Ok(()), |_| Ok(()));

        assert!(!layout.default_media_dir.exists());

        fs::create_dir_all(&layout.default_media_dir).unwrap();
        let foreign = layout.default_media_dir.join("foreign.mp4");
        fs::write(&foreign, b"foreign").unwrap();
        let plan = build_cleanup_plan(&layout, true);
        execute_cleanup_plan_with(&plan, [], |_| Ok(()), |_| Ok(()));

        assert!(foreign.exists());
        assert!(layout.default_media_dir.exists());
    }

    #[test]
    fn backup_settings_supply_media_replay_and_credential_cleanup_targets() {
        let root = TestDir::new("clipline-uninstall", "backup-settings");
        let layout = layout(root.path());
        let media = root.path().join("Media");
        let replay = root.path().join("Replay");
        let replay_run = replay.join("clipline-replay-cache-123-456-0");
        fs::create_dir_all(&replay_run).unwrap();
        fs::write(
            replay_run.join(clipline_storage::REPLAY_CACHE_OWNER_FILE),
            br#"{"process_instance_id":"456:789","created_at_unix":123}"#,
        )
        .unwrap();
        fs::create_dir_all(&layout.config_dir).unwrap();
        fs::write(layout.config_dir.join("settings.json"), b"not json").unwrap();
        fs::write(
            layout.config_dir.join("settings.json.bak"),
            format!(
                r#"{{
                    "media_dir": {:?},
                    "replay_storage": {{"mode":"disk","disk_dir":{:?}}},
                    "cloud": {{
                        "credential_target":"Clipline Cloud:current",
                        "credential_cleanup_targets":["Clipline Cloud:old"]
                    }},
                    "osu": {{
                        "credential_target":"Clipline osu!:current",
                        "credential_cleanup_targets":["Clipline osu!:old"]
                    }}
                }}"#,
                media.display().to_string(),
                replay.display().to_string(),
            ),
        )
        .unwrap();

        let plan = build_cleanup_plan(&layout, false);

        assert!(contains_path(&plan.replay_trees, &replay_run));
        assert_eq!(
            plan.credential_targets,
            [
                "Clipline Cloud:current",
                "Clipline Cloud:old",
                "Clipline osu!:current",
                "Clipline osu!:old",
            ]
        );
    }

    #[test]
    fn replay_runs_inside_media_are_removed_only_with_recording_consent() {
        let root = TestDir::new("clipline-uninstall", "replay-media-overlap");
        let layout = layout(root.path());
        let media = root.path().join("Media");
        let replay_run = media.join("clipline-replay-cache-123-456-0");
        fs::create_dir_all(&replay_run).unwrap();
        fs::write(
            replay_run.join(clipline_storage::REPLAY_CACHE_OWNER_FILE),
            br#"{"process_instance_id":"456:789","created_at_unix":123}"#,
        )
        .unwrap();
        write_settings(
            &layout,
            &format!(
                r#"{{
                    "media_dir": {:?},
                    "replay_storage": {{"mode":"disk","disk_dir":{:?}}}
                }}"#,
                media.display().to_string(),
                media.display().to_string(),
            ),
        );

        let keep = build_cleanup_plan(&layout, false);
        let delete = build_cleanup_plan(&layout, true);

        assert!(!contains_path(&keep.replay_trees, &replay_run));
        assert!(contains_path(&delete.replay_trees, &replay_run));
    }

    #[test]
    fn replay_cleanup_requires_the_generated_run_name_shape() {
        let root = TestDir::new("clipline-uninstall", "strict-replay-run-name");
        let layout = layout(root.path());
        let replay = root.path().join("Replay");
        let valid = replay.join("clipline-replay-cache-123-456-0");
        let unowned = replay.join("clipline-replay-cache-123-457-0");
        let malformed = replay.join("clipline-replay-cache-123-458-0");
        let mismatched = replay.join("clipline-replay-cache-123-459-0");
        let unrelated = replay.join("clipline-replay-cache-backup");
        fs::create_dir_all(&valid).unwrap();
        fs::write(
            valid.join(clipline_storage::REPLAY_CACHE_OWNER_FILE),
            br#"{"process_instance_id":"456:789","created_at_unix":123}"#,
        )
        .unwrap();
        fs::create_dir_all(&unowned).unwrap();
        fs::create_dir_all(&malformed).unwrap();
        fs::write(
            malformed.join(clipline_storage::REPLAY_CACHE_OWNER_FILE),
            b"{}",
        )
        .unwrap();
        fs::create_dir_all(&mismatched).unwrap();
        fs::write(
            mismatched.join(clipline_storage::REPLAY_CACHE_OWNER_FILE),
            br#"{"process_instance_id":"999:789","created_at_unix":123}"#,
        )
        .unwrap();
        fs::create_dir_all(&unrelated).unwrap();
        write_settings(
            &layout,
            &format!(
                r#"{{"replay_storage":{{"mode":"disk","disk_dir":{:?}}}}}"#,
                replay.display().to_string(),
            ),
        );

        let plan = build_cleanup_plan(&layout, false);

        assert!(contains_path(&plan.replay_trees, &valid));
        assert!(!contains_path(&plan.replay_trees, &unowned));
        assert!(!contains_path(&plan.replay_trees, &malformed));
        assert!(!contains_path(&plan.replay_trees, &mismatched));
        assert!(!contains_path(&plan.replay_trees, &unrelated));
    }

    #[test]
    fn cleanup_preserves_a_custom_media_tree_nested_under_app_residue() {
        let root = TestDir::new("clipline-uninstall", "nested-custom-media");
        let layout = layout(root.path());
        let media = layout.config_dir.join("Media");
        let clip = media.join("owned.mp4");
        let foreign = media.join("foreign.txt");
        fs::create_dir_all(&media).unwrap();
        fs::write(&clip, b"owned").unwrap();
        fs::write(clip.with_extension("clipline.json"), b"{}").unwrap();
        fs::write(&foreign, b"foreign").unwrap();
        write_settings(
            &layout,
            &format!(r#"{{"media_dir":{:?}}}"#, media.display().to_string()),
        );

        let plan = build_cleanup_plan(&layout, false);
        execute_cleanup_plan_with(&plan, [], |_| Ok(()), |_| Ok(()));

        assert!(clip.exists());
        assert!(foreign.exists());
        assert!(media.exists());
        assert!(!layout.config_dir.join("settings.json").exists());
    }

    #[test]
    fn opted_in_cleanup_includes_configured_and_default_media_roots() {
        let root = TestDir::new("clipline-uninstall", "fallback-media");
        let layout = layout(root.path());
        let custom = root.path().join("Custom Media");
        let custom_clip = custom.join("custom.mp4");
        let fallback_clip = layout.default_media_dir.join("fallback.mp4");
        for clip in [&custom_clip, &fallback_clip] {
            fs::create_dir_all(clip.parent().unwrap()).unwrap();
            fs::write(clip, b"owned").unwrap();
            fs::write(clip.with_extension("clipline.json"), b"{}").unwrap();
        }
        write_settings(
            &layout,
            &format!(r#"{{"media_dir":{:?}}}"#, custom.display().to_string()),
        );

        let plan = build_cleanup_plan(&layout, true);
        execute_cleanup_plan_with(&plan, [], |_| Ok(()), |_| Ok(()));

        assert!(!custom_clip.exists());
        assert!(custom.exists());
        assert!(!fallback_clip.exists());
        assert!(!layout.default_media_dir.exists());
    }

    #[test]
    fn dangerous_media_and_replay_roots_are_rejected() {
        let root = TestDir::new("clipline-uninstall", "dangerous-roots");
        let layout = layout(root.path());
        let replay_run = layout
            .local_cache_dir
            .join("clipline-replay-cache-123-456-0");
        fs::create_dir_all(&replay_run).unwrap();
        write_settings(
            &layout,
            &format!(
                r#"{{
                    "media_dir": {:?},
                    "replay_storage": {{"mode":"disk","disk_dir":{:?}}}
                }}"#,
                layout.profile_dir.display().to_string(),
                layout.local_cache_dir.display().to_string(),
            ),
        );

        let plan = build_cleanup_plan(&layout, true);

        assert!(!contains_path(&plan.media_dirs, &layout.profile_dir));
        assert!(contains_path(&plan.media_dirs, &layout.default_media_dir));
        assert!(!contains_path(&plan.replay_trees, &replay_run));
        assert!(!contains_path(&plan.remove_trees, &layout.local_cache_dir));
    }

    #[test]
    fn executor_sweeps_enumerated_credentials_and_both_autostart_names() {
        let root = TestDir::new("clipline-uninstall", "system-cleanup-callbacks");
        let layout = layout(root.path());
        write_settings(
            &layout,
            r#"{
                "cloud":{
                    "credential_target":"Clipline Cloud:settings",
                    "credential_cleanup_targets":["Unrelated credential"]
                }
            }"#,
        );
        let plan = build_cleanup_plan(&layout, false);
        let credentials = RefCell::new(Vec::new());
        let autostart = RefCell::new(Vec::new());

        execute_cleanup_plan_with(
            &plan,
            [
                "Clipline Cloud:swept",
                "Clipline osu!:swept",
                "Another unrelated credential",
            ],
            |target| {
                credentials.borrow_mut().push(target.to_owned());
                Ok(())
            },
            |name| {
                autostart.borrow_mut().push(name.to_owned());
                Ok(())
            },
        );

        assert_eq!(
            credentials.into_inner(),
            [
                "Clipline Cloud:settings",
                "Clipline Cloud:swept",
                "Clipline osu!:swept",
            ]
        );
        assert_eq!(autostart.into_inner(), ["clipline-app", "Clipline"]);
    }
}
