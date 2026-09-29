use super::*;

#[tauri::command]
pub async fn storage_status(
    settings: tauri::State<'_, StorageSettings>,
) -> Result<StorageInfo, String> {
    let dir = settings.clips_dir()?;
    let quota_bytes = settings.quota_bytes();
    tauri::async_runtime::spawn_blocking(move || storage_status_for_dir(dir, quota_bytes))
        .await
        .map_err(|e| format!("storage status task: {e}"))?
}

pub(crate) fn storage_status_for_dir(dir: PathBuf, quota_bytes: Option<u64>) -> Result<StorageInfo, String> {
    let status = read_storage_status(&dir, quota_bytes).map_err(|e| e.to_string())?;
    Ok(StorageInfo {
        clip_count: status.clip_count,
        total_bytes: status.total_bytes,
        quota_bytes: status.quota_bytes,
        over_quota: status.is_over_quota(),
    })
}

#[tauri::command]
pub fn reveal_clip(path: String, settings: tauri::State<StorageSettings>) -> Result<(), String> {
    let target = validate_clip_path(&settings, &path)?;
    select_path_in_explorer(&target)
}

pub(crate) fn select_path_in_explorer(target: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let canonical = target.display().to_string();
    // Explorer's `/select` rejects verbatim (`\\?\`) paths from `canonicalize`.
    let displayable = if let Some(rest) = canonical.strip_prefix(r"\\?\UNC\") {
        std::borrow::Cow::Owned(format!(r"\\{rest}"))
    } else {
        std::borrow::Cow::Borrowed(
            canonical
                .strip_prefix(r"\\?\")
                .unwrap_or(canonical.as_str()),
        )
    };
    std::process::Command::new("explorer.exe")
        .raw_arg(format!("/select,\"{displayable}\""))
        .spawn()
        .map_err(|e| format!("open explorer: {e}"))?;
    Ok(())
}

#[tauri::command]
pub fn open_media_folder(settings: tauri::State<StorageSettings>) -> Result<(), String> {
    let dir = settings.clips_dir()?;
    open_folder_path(&dir)
}

pub(crate) fn open_folder_path(dir: &Path) -> Result<(), String> {
    std::process::Command::new("explorer.exe")
        .arg(dir)
        .spawn()
        .map_err(|e| format!("open explorer: {e}"))?;
    Ok(())
}
