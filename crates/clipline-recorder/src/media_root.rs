use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static PROBE_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn default_clips_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Videos")
        .join("Clipline")
}

pub fn clips_dir(media_dir: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(media_dir)
        .map_err(|error| format!("create media folder {}: {error}", media_dir.display()))?;
    Ok(media_dir.to_path_buf())
}

pub fn clips_dir_resolved_with_probe(
    media_dir: &Path,
    fallback: impl FnOnce() -> PathBuf,
    mut probe: impl FnMut(&Path) -> std::io::Result<()>,
) -> Result<(PathBuf, bool), String> {
    let configured_error = match prepare_writable_directory_with(media_dir, &mut probe) {
        Ok(()) => return Ok((media_dir.to_path_buf(), false)),
        Err(error) => error,
    };
    let dir = fallback();
    if let Err(fallback_error) = prepare_writable_directory_with(&dir, &mut probe) {
        return Err(format!(
            "media folder {} is not writable ({configured_error}); fallback {} is not writable ({fallback_error})",
            media_dir.display(),
            dir.display()
        ));
    }
    Ok((dir, true))
}

pub fn prepare_writable_directory_with(
    dir: &Path,
    mut probe: impl FnMut(&Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    probe(dir)
}

pub fn probe_writable_directory(dir: &Path) -> std::io::Result<()> {
    for _ in 0..16 {
        let unique = PROBE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!(
            ".clipline-write-probe-{}-{unique}.tmp",
            std::process::id()
        ));
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let result = file.write_all(&[0]).and_then(|()| file.sync_data());
        drop(file);
        let cleanup = std::fs::remove_file(&path);
        result?;
        cleanup?;
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not reserve a unique media-folder probe file",
    ))
}

pub fn is_within_temp(dir: &Path, temp_dir: &Path) -> bool {
    let normalize = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    normalize(dir).starts_with(normalize(temp_dir))
}
