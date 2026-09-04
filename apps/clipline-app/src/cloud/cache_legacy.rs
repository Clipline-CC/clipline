//! Legacy cloud-cache layout migration (config_base to local_cache_base).
use super::*;

pub(crate) fn legacy_cloud_clip_cache_root_dir() -> PathBuf {
    crate::settings::persistence::config_base().join("cloud-cache")
}

pub(crate) fn migrate_legacy_cloud_cache(legacy: &Path, local: &Path) -> Result<(), String> {
    if legacy == local || !legacy.exists() {
        return Ok(());
    }
    let metadata = std::fs::symlink_metadata(legacy)
        .map_err(|error| format!("inspect legacy cloud cache: {error}"))?;
    if !metadata.is_dir() || metadata_is_link(&metadata) {
        return Ok(());
    }
    std::fs::create_dir_all(local).map_err(|error| format!("create local cloud cache: {error}"))?;
    let entries =
        std::fs::read_dir(legacy).map_err(|error| format!("read legacy cloud cache: {error}"))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let namespace = name.to_string_lossy();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_dir()
            || metadata_is_link(&metadata)
            || namespace.len() != 16
            || !namespace.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            continue;
        }
        let destination = local.join(&name);
        if destination.exists() {
            continue;
        }
        std::fs::rename(&path, &destination)
            .map_err(|error| format!("migrate cloud cache namespace {namespace}: {error}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_test_utils::TestDir;

    #[test]
    fn legacy_cloud_cache_migration_moves_only_regular_namespace_directories() {
        let dir = TestDir::new("clipline-cloud", "cloud-cache-migration");
        let legacy = dir.path().join("roaming-cloud-cache");
        let local = dir.path().join("local-cloud-cache");
        let namespace = legacy.join("abcdef0123456789");
        std::fs::create_dir_all(&namespace).unwrap();
        std::fs::write(namespace.join("clip.mp4"), b"clip").unwrap();
        std::fs::write(legacy.join("unrelated.txt"), b"leave me").unwrap();
        let external = dir.path().join("external");
        std::fs::create_dir_all(&external).unwrap();
        std::fs::write(external.join("outside.mp4"), b"outside").unwrap();
        let linked_namespace = legacy.join("1111111111111111");
        let linked = std::os::windows::fs::symlink_dir(&external, &linked_namespace).is_ok();

        migrate_legacy_cloud_cache(&legacy, &local).unwrap();

        assert!(local.join("abcdef0123456789").join("clip.mp4").exists());
        assert!(legacy.join("unrelated.txt").exists());
        if linked {
            assert!(external.join("outside.mp4").exists());
            assert!(!local.join("1111111111111111").exists());
        }
    }

}