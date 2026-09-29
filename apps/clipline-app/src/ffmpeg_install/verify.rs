//! Verify-and-publish: allowlist extraction, provenance, atomic managed-tree swap.
//! Downloaded bytes are never executed before allowlist hash verification.
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;

use sha2::{Digest, Sha256};
use zip::ZipArchive;

use super::download::{managed_root, staging_root, staging_tree_path};
use crate::ffmpeg_runtime::{
    verify_managed_ffmpeg_runtime, FfmpegAllowedFile, FfmpegRuntimeManifest, ManagedRuntimeInfo,
};

fn hex_sha256_reader(mut reader: impl io::Read) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

pub(super) fn hex_sha256_file(path: &Path) -> io::Result<String> {
    hex_sha256_reader(File::open(path)?)
}

fn validate_allowlist_entry(file: &FfmpegAllowedFile) -> Result<(), String> {
    if file.staged_name.trim().is_empty()
        || file.staged_name
            != Path::new(&file.staged_name)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
        || file.archive_path.contains("..")
        || Path::new(&file.archive_path).is_absolute()
    {
        return Err(format!(
            "unsafe FFmpeg allowlist entry: {} -> {}",
            file.archive_path, file.staged_name
        ));
    }
    Ok(())
}

pub(super) fn copy_with_cancel(
    reader: &mut impl Read,
    writer: &mut impl Write,
    cancelled: &dyn Fn() -> bool,
) -> Result<(u64, String), String> {
    let mut copied = 0_u64;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        if cancelled() {
            return Err("ffmpeg install cancelled".into());
        }
        let read = reader
            .read(&mut buffer)
            .map_err(|e| format!("read archive entry: {e}"))?;
        if read == 0 {
            let hash = hasher
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            return Ok((copied, hash));
        }
        writer
            .write_all(&buffer[..read])
            .map_err(|e| format!("write staged file: {e}"))?;
        hasher.update(&buffer[..read]);
        copied = copied.saturating_add(read as u64);
    }
}

/// Extract only allowlisted entries from a verified archive into `tree_dir`.
pub fn extract_allowlisted_ffmpeg_archive(
    archive_path: &Path,
    tree_dir: &Path,
    manifest: &FfmpegRuntimeManifest,
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<(String, u64, String)>, String> {
    if cancelled() {
        return Err("ffmpeg install cancelled".into());
    }
    if tree_dir.exists() {
        fs::remove_dir_all(tree_dir).map_err(|e| format!("clear staging tree: {e}"))?;
    }
    fs::create_dir_all(tree_dir).map_err(|e| format!("create staging tree: {e}"))?;

    let file = File::open(archive_path).map_err(|e| format!("open archive: {e}"))?;
    let mut zip = ZipArchive::new(file).map_err(|e| format!("open zip: {e}"))?;
    let mut verified = Vec::with_capacity(manifest.allowed_files.len());

    for allowed in &manifest.allowed_files {
        if cancelled() {
            return Err("ffmpeg install cancelled".into());
        }
        validate_allowlist_entry(allowed)?;
        let entry_name = format!(
            "{}/{}",
            manifest.archive_root.trim_end_matches('/'),
            allowed.archive_path.replace('\\', "/")
        );
        let mut entry = zip
            .by_name(&entry_name)
            .map_err(|_| format!("missing archive entry {entry_name}"))?;
        if entry.size() != allowed.size {
            return Err(format!(
                "archive entry {entry_name} size mismatch: expected {}, got {}",
                allowed.size,
                entry.size()
            ));
        }
        let output = tree_dir.join(&allowed.staged_name);
        let (copied, hash) = {
            let mut out = File::create(&output)
                .map_err(|e| format!("create staged {}: {e}", allowed.staged_name))?;
            copy_with_cancel(&mut entry, &mut out, cancelled)
                .map_err(|e| format!("extract {}: {e}", allowed.staged_name))?
        };
        if cancelled() {
            return Err("ffmpeg install cancelled".into());
        }
        if copied != allowed.size {
            return Err(format!(
                "staged {} size mismatch: expected {}, got {copied}",
                allowed.staged_name, allowed.size
            ));
        }
        if hash != allowed.sha256.to_ascii_lowercase() {
            return Err(format!(
                "staged {} SHA-256 mismatch: expected {}, got {hash}",
                allowed.staged_name, allowed.sha256
            ));
        }
        verified.push((allowed.staged_name.clone(), allowed.size, hash));
    }
    Ok(verified)
}

pub fn write_managed_provenance(
    tree_dir: &Path,
    manifest: &FfmpegRuntimeManifest,
    manifest_sha256: &str,
    files: &[(String, u64, String)],
) -> Result<(), String> {
    let provenance = serde_json::json!({
        "schema_version": 1,
        "provider": manifest.provider,
        "release_tag": manifest.release_tag,
        "published_at": manifest.published_at,
        "archive_name": manifest.archive_name,
        "archive_url": manifest.archive_url,
        "archive_sha256": manifest.archive_sha256,
        "manifest_sha256": manifest_sha256,
        "ffmpeg_version": manifest.version_line,
        "source_offer_url": manifest.source_offer_url,
        "ffmpeg_source_url": manifest.ffmpeg_source_url,
        "files": files.iter().map(|(name, size, sha)| serde_json::json!({
            "name": name,
            "size": size,
            "sha256": sha,
        })).collect::<Vec<_>>(),
    });
    fs::write(
        tree_dir.join("PROVENANCE.json"),
        serde_json::to_string_pretty(&provenance).map_err(|e| e.to_string())? + "\n",
    )
    .map_err(|e| format!("write PROVENANCE.json: {e}"))
}

/// Atomically replace `dest` with `tree_dir` (backup+rename).
pub fn publish_managed_runtime_atomic(tree_dir: &Path, dest: &Path) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create managed parent: {e}"))?;
    }
    let backup = dest.with_file_name(format!(
        ".ffmpeg-previous-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    if dest.exists() {
        fs::rename(dest, &backup).map_err(|e| format!("backup existing managed runtime: {e}"))?;
    }
    match fs::rename(tree_dir, dest) {
        Ok(()) => {
            let _ = fs::remove_dir_all(&backup);
            Ok(())
        }
        Err(error) => {
            if backup.exists() && !dest.exists() {
                let _ = fs::rename(&backup, dest);
            }
            Err(format!("publish managed runtime: {error}"))
        }
    }
}

pub fn verify_archive_sha256(archive_path: &Path, expected: &str) -> Result<String, String> {
    let actual = hex_sha256_file(archive_path).map_err(|e| format!("hash archive: {e}"))?;
    let expected = expected.to_ascii_lowercase();
    if actual != expected {
        return Err(format!(
            "FFmpeg archive SHA-256 mismatch: expected {expected}, got {actual}"
        ));
    }
    Ok(actual)
}

pub fn assert_archive_size(archive_path: &Path, expected_size: u64) -> Result<(), String> {
    let actual = fs::metadata(archive_path)
        .map_err(|e| format!("archive metadata: {e}"))?
        .len();
    if actual != expected_size {
        return Err(format!(
            "FFmpeg archive size mismatch: expected {expected_size}, got {actual}"
        ));
    }
    Ok(())
}

/// Install from a local archive that already passed size/hash checks.
pub fn install_managed_runtime_from_archive(
    archive_path: &Path,
    local_app_data: &Path,
    manifest: &FfmpegRuntimeManifest,
    manifest_sha256: &str,
    cancelled: &dyn Fn() -> bool,
    before_publish: impl FnOnce() -> Result<(), String>,
) -> Result<ManagedRuntimeInfo, String> {
    if cancelled() {
        return Err("ffmpeg install cancelled".into());
    }
    assert_archive_size(archive_path, manifest.archive_size)?;
    verify_archive_sha256(archive_path, &manifest.archive_sha256)?;

    let staging = staging_root(local_app_data);
    let tree = staging_tree_path(&staging);
    let files = extract_allowlisted_ffmpeg_archive(archive_path, &tree, manifest, cancelled)?;
    write_managed_provenance(&tree, manifest, manifest_sha256, &files)?;
    let mut info = verify_managed_ffmpeg_runtime(&tree, manifest, manifest_sha256)
        .map_err(|e| e.to_string())?;
    if cancelled() {
        return Err("ffmpeg install cancelled".into());
    }
    let dest = managed_root(local_app_data);
    before_publish()?;
    publish_managed_runtime_atomic(&tree, &dest)?;
    info.dir = dest.clone();
    info.ffmpeg_exe = dest.join("ffmpeg.exe");
    Ok(info)
}
