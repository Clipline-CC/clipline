//! FFmpeg capability matrix, managed-runtime verification, and discovery status
//! (slim-core Milestone B).
//!
//! `clipline_capture::ffmpeg::locate` remains discovery of a runnable binary —
//! it does **not** mean ManagedVerified. Download/ensure state machine is B3.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use clipline_capture::{Codec, EncoderApi, EncoderBackend, EncoderCapability};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Why a Core surface still needs an FFmpeg child process today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FfmpegRequirementReason {
    SvtAv1,
    FfmpegBackendEncoder,
    Poster,
    AudioSidecarExtract,
    /// `library.rs` share/Copy Clip export always routes through FFmpeg today.
    ShareableClipboardExport,
}

/// How the shell classifies an FFmpeg binary for UI/ensure.
///
/// `ManagedVerified` is reserved for a LOCALAPPDATA tree that passed the B2
/// manifest verifier. A successful `locate()` of PATH/override/bundled bytes is
/// `ExternalUnmanaged`, never a silent no-op for Install/Repair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FfmpegDiscoveryKind {
    ManagedVerified,
    ExternalUnmanaged,
    Missing,
}

/// Default H.264 recording can proceed when any non-empty MFT capability exists.
pub fn recording_without_ffmpeg_possible(capabilities: &[EncoderCapability]) -> bool {
    capabilities
        .iter()
        .any(|cap| cap.api == EncoderApi::Mft && !cap.codecs.is_empty())
}

/// Reasons that currently require FFmpeg for the given capability set / product
/// surfaces. Always includes poster, audio sidecar extract, and shareable
/// clipboard export — those paths are FFmpeg-backed regardless of MFT.
pub fn ffmpeg_required_for(capabilities: &[EncoderCapability]) -> Vec<FfmpegRequirementReason> {
    let mut reasons = vec![
        FfmpegRequirementReason::Poster,
        FfmpegRequirementReason::AudioSidecarExtract,
        FfmpegRequirementReason::ShareableClipboardExport,
    ];

    let has_ffmpeg_backend = capabilities.iter().any(|cap| {
        cap.api == EncoderApi::Ffmpeg
            && cap.backend != EncoderBackend::SvtAv1
            && !cap.codecs.is_empty()
    });
    if has_ffmpeg_backend {
        reasons.push(FfmpegRequirementReason::FfmpegBackendEncoder);
    }

    let has_svt = capabilities.iter().any(|cap| {
        cap.api == EncoderApi::Ffmpeg
            && cap.backend == EncoderBackend::SvtAv1
            && cap.codecs.contains(&Codec::Av1)
    });
    if has_svt {
        reasons.push(FfmpegRequirementReason::SvtAv1);
    }

    reasons
}

/// Install/Repair `ensure` is a no-op only for a managed-verified runtime.
pub fn ensure_ffmpeg_runtime_is_noop(kind: FfmpegDiscoveryKind) -> bool {
    matches!(kind, FfmpegDiscoveryKind::ManagedVerified)
}

/// Classify discovery for UI/ensure. `managed_verified` comes from the B2
/// verifier; `locate_found` is whether `ffmpeg::locate()` returned a path.
pub fn classify_ffmpeg_discovery(
    managed_verified: bool,
    locate_found: bool,
) -> FfmpegDiscoveryKind {
    if managed_verified {
        FfmpegDiscoveryKind::ManagedVerified
    } else if locate_found {
        FfmpegDiscoveryKind::ExternalUnmanaged
    } else {
        FfmpegDiscoveryKind::Missing
    }
}

/// One allowlisted file in `ffmpeg-runtime.json`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct FfmpegAllowedFile {
    pub archive_path: String,
    pub staged_name: String,
    pub size: u64,
    pub sha256: String,
}

/// Committed/runtime manifest for the managed LGPL FFmpeg tree.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct FfmpegRuntimeManifest {
    pub schema_version: u32,
    pub provider: String,
    pub release_tag: String,
    pub published_at: String,
    pub archive_name: String,
    pub archive_url: String,
    pub archive_sha256: String,
    pub archive_size: u64,
    pub archive_root: String,
    pub version_line: String,
    pub source_offer_url: String,
    pub ffmpeg_source_url: String,
    pub allowed_files: Vec<FfmpegAllowedFile>,
}

/// Provenance file entry written beside the managed runtime.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ProvenanceFile {
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

/// `PROVENANCE.json` identity for a published managed runtime.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct FfmpegProvenance {
    pub schema_version: u32,
    pub provider: String,
    pub release_tag: String,
    pub published_at: String,
    pub archive_name: String,
    pub archive_url: String,
    pub archive_sha256: String,
    pub manifest_sha256: String,
    pub ffmpeg_version: String,
    pub source_offer_url: String,
    pub ffmpeg_source_url: String,
    pub files: Vec<ProvenanceFile>,
}

/// Successful managed-runtime verification result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedRuntimeInfo {
    pub dir: PathBuf,
    pub ffmpeg_exe: PathBuf,
    pub release_tag: String,
    pub archive_sha256: String,
    pub manifest_sha256: String,
}

/// Why a managed tree failed verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedRuntimeVerifyError {
    NotDirectory { path: PathBuf },
    MissingFile { name: String },
    UnexpectedFile { name: String },
    SizeMismatch { name: String, expected: u64, actual: u64 },
    HashMismatch { name: String, expected: String, actual: String },
    MissingProvenance,
    InvalidProvenance { message: String },
    ProvenanceMismatch { field: String, expected: String, actual: String },
    InvalidManifest { message: String },
    Io { message: String },
}

impl std::fmt::Display for ManagedRuntimeVerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotDirectory { path } => {
                write!(f, "managed FFmpeg path is not a directory: {}", path.display())
            }
            Self::MissingFile { name } => write!(f, "managed FFmpeg missing required file: {name}"),
            Self::UnexpectedFile { name } => {
                write!(f, "managed FFmpeg contains unexpected file: {name}")
            }
            Self::SizeMismatch {
                name,
                expected,
                actual,
            } => write!(
                f,
                "managed FFmpeg {name} size mismatch: expected {expected}, got {actual}"
            ),
            Self::HashMismatch {
                name,
                expected,
                actual,
            } => write!(
                f,
                "managed FFmpeg {name} SHA-256 mismatch: expected {expected}, got {actual}"
            ),
            Self::MissingProvenance => write!(f, "managed FFmpeg missing PROVENANCE.json"),
            Self::InvalidProvenance { message } => {
                write!(f, "managed FFmpeg PROVENANCE.json invalid: {message}")
            }
            Self::ProvenanceMismatch {
                field,
                expected,
                actual,
            } => write!(
                f,
                "managed FFmpeg provenance {field} mismatch: expected {expected}, got {actual}"
            ),
            Self::InvalidManifest { message } => {
                write!(f, "FFmpeg runtime manifest invalid: {message}")
            }
            Self::Io { message } => write!(f, "managed FFmpeg IO error: {message}"),
        }
    }
}

impl From<io::Error> for ManagedRuntimeVerifyError {
    fn from(error: io::Error) -> Self {
        Self::Io {
            message: error.to_string(),
        }
    }
}

/// UI/ensure-facing classification of the current FFmpeg runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FfmpegRuntimeStatus {
    pub kind: FfmpegDiscoveryKind,
    pub managed: Option<ManagedRuntimeInfo>,
    pub locate_path: Option<PathBuf>,
}

fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn hex_sha256_file(path: &Path) -> Result<String, ManagedRuntimeVerifyError> {
    let bytes = fs::read(path)?;
    Ok(hex_sha256(&bytes))
}

/// Parse the committed `ffmpeg-runtime.json` allowlist/manifest.
pub fn parse_ffmpeg_runtime_manifest(
    json: &str,
) -> Result<FfmpegRuntimeManifest, ManagedRuntimeVerifyError> {
    let manifest: FfmpegRuntimeManifest = serde_json::from_str(json).map_err(|error| {
        ManagedRuntimeVerifyError::InvalidManifest {
            message: error.to_string(),
        }
    })?;
    if manifest.schema_version != 1 {
        return Err(ManagedRuntimeVerifyError::InvalidManifest {
            message: format!(
                "schema_version must be 1, got {}",
                manifest.schema_version
            ),
        });
    }
    if manifest.allowed_files.is_empty() {
        return Err(ManagedRuntimeVerifyError::InvalidManifest {
            message: "allowed_files must not be empty".into(),
        });
    }
    if manifest.archive_size == 0 {
        return Err(ManagedRuntimeVerifyError::InvalidManifest {
            message: "archive_size must be > 0".into(),
        });
    }
    if manifest.archive_root.trim().is_empty() {
        return Err(ManagedRuntimeVerifyError::InvalidManifest {
            message: "archive_root must not be empty".into(),
        });
    }
    Ok(manifest)
}

/// Total bytes of allowlisted staged files (for free-space planning).
pub fn allowlist_total_bytes(manifest: &FfmpegRuntimeManifest) -> u64 {
    manifest.allowed_files.iter().map(|file| file.size).sum()
}

/// Bytes that must be free before download/extract: archive + staged tree + margin.
pub fn free_space_required_bytes(manifest: &FfmpegRuntimeManifest, margin_bytes: u64) -> u64 {
    manifest
        .archive_size
        .saturating_add(allowlist_total_bytes(manifest))
        .saturating_add(margin_bytes)
}

pub fn load_ffmpeg_runtime_manifest(
    path: &Path,
) -> Result<(FfmpegRuntimeManifest, String), ManagedRuntimeVerifyError> {
    let bytes = fs::read(path)?;
    let json = String::from_utf8(bytes.clone()).map_err(|error| {
        ManagedRuntimeVerifyError::InvalidManifest {
            message: error.to_string(),
        }
    })?;
    Ok((parse_ffmpeg_runtime_manifest(&json)?, hex_sha256(&bytes)))
}

fn assert_exact(
    field: &str,
    expected: &str,
    actual: &str,
) -> Result<(), ManagedRuntimeVerifyError> {
    if expected == actual {
        Ok(())
    } else {
        Err(ManagedRuntimeVerifyError::ProvenanceMismatch {
            field: field.into(),
            expected: expected.into(),
            actual: actual.into(),
        })
    }
}

fn verify_regular_file(
    path: &Path,
    name: &str,
    expected_size: u64,
    expected_sha256: &str,
) -> Result<String, ManagedRuntimeVerifyError> {
    let meta = fs::metadata(path).map_err(|_| ManagedRuntimeVerifyError::MissingFile {
        name: name.into(),
    })?;
    if !meta.is_file() {
        return Err(ManagedRuntimeVerifyError::MissingFile {
            name: name.into(),
        });
    }
    let actual_size = meta.len();
    if actual_size != expected_size {
        return Err(ManagedRuntimeVerifyError::SizeMismatch {
            name: name.into(),
            expected: expected_size,
            actual: actual_size,
        });
    }
    let actual_hash = hex_sha256_file(path)?;
    let expected = expected_sha256.to_ascii_lowercase();
    if actual_hash != expected {
        return Err(ManagedRuntimeVerifyError::HashMismatch {
            name: name.into(),
            expected,
            actual: actual_hash,
        });
    }
    Ok(actual_hash)
}

/// Verify a managed LOCALAPPDATA (or staging) FFmpeg tree against the committed
/// manifest. Unexpected files other than `README.md`/`PROVENANCE.json` fail;
/// tampered allowlisted bytes always fail.
pub fn verify_managed_ffmpeg_runtime(
    dir: &Path,
    manifest: &FfmpegRuntimeManifest,
    manifest_sha256: &str,
) -> Result<ManagedRuntimeInfo, ManagedRuntimeVerifyError> {
    let meta = fs::metadata(dir).map_err(|_| ManagedRuntimeVerifyError::NotDirectory {
        path: dir.to_path_buf(),
    })?;
    if !meta.is_dir() {
        return Err(ManagedRuntimeVerifyError::NotDirectory {
            path: dir.to_path_buf(),
        });
    }

    let mut expected_names = std::collections::BTreeSet::new();
    expected_names.insert("PROVENANCE.json".to_string());
    for file in &manifest.allowed_files {
        expected_names.insert(file.staged_name.clone());
    }

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.eq_ignore_ascii_case("README.md") {
            continue;
        }
        if !expected_names.contains(&name) {
            return Err(ManagedRuntimeVerifyError::UnexpectedFile { name });
        }
    }

    for file in &manifest.allowed_files {
        let path = dir.join(&file.staged_name);
        if !path.is_file() {
            return Err(ManagedRuntimeVerifyError::MissingFile {
                name: file.staged_name.clone(),
            });
        }
        verify_regular_file(
            &path,
            &file.staged_name,
            file.size,
            &file.sha256,
        )?;
    }

    let provenance_path = dir.join("PROVENANCE.json");
    if !provenance_path.is_file() {
        return Err(ManagedRuntimeVerifyError::MissingProvenance);
    }
    let provenance_json = fs::read_to_string(&provenance_path)?;
    let provenance: FfmpegProvenance =
        serde_json::from_str(&provenance_json).map_err(|error| {
            ManagedRuntimeVerifyError::InvalidProvenance {
                message: error.to_string(),
            }
        })?;
    if provenance.schema_version != 1 {
        return Err(ManagedRuntimeVerifyError::InvalidProvenance {
            message: format!(
                "schema_version must be 1, got {}",
                provenance.schema_version
            ),
        });
    }

    assert_exact("provider", &manifest.provider, &provenance.provider)?;
    assert_exact("release_tag", &manifest.release_tag, &provenance.release_tag)?;
    assert_exact(
        "published_at",
        &manifest.published_at,
        &provenance.published_at,
    )?;
    assert_exact(
        "archive_name",
        &manifest.archive_name,
        &provenance.archive_name,
    )?;
    assert_exact("archive_url", &manifest.archive_url, &provenance.archive_url)?;
    assert_exact(
        "archive_sha256",
        &manifest.archive_sha256.to_ascii_lowercase(),
        &provenance.archive_sha256.to_ascii_lowercase(),
    )?;
    assert_exact(
        "source_offer_url",
        &manifest.source_offer_url,
        &provenance.source_offer_url,
    )?;
    assert_exact(
        "ffmpeg_source_url",
        &manifest.ffmpeg_source_url,
        &provenance.ffmpeg_source_url,
    )?;
    assert_exact(
        "manifest_sha256",
        &manifest_sha256.to_ascii_lowercase(),
        &provenance.manifest_sha256.to_ascii_lowercase(),
    )?;
    assert_exact(
        "ffmpeg_version",
        &manifest.version_line,
        &provenance.ffmpeg_version,
    )?;

    if provenance.files.len() != manifest.allowed_files.len() {
        return Err(ManagedRuntimeVerifyError::InvalidProvenance {
            message: format!(
                "files count mismatch: expected {}, got {}",
                manifest.allowed_files.len(),
                provenance.files.len()
            ),
        });
    }
    for (index, expected) in manifest.allowed_files.iter().enumerate() {
        let actual = &provenance.files[index];
        assert_exact(
            &format!("files[{index}].name"),
            &expected.staged_name,
            &actual.name,
        )?;
        if actual.size != expected.size {
            return Err(ManagedRuntimeVerifyError::SizeMismatch {
                name: format!("provenance files[{index}]"),
                expected: expected.size,
                actual: actual.size,
            });
        }
        assert_exact(
            &format!("files[{index}].sha256"),
            &expected.sha256.to_ascii_lowercase(),
            &actual.sha256.to_ascii_lowercase(),
        )?;
    }

    let ffmpeg_exe = dir.join("ffmpeg.exe");
    if !ffmpeg_exe.is_file() {
        return Err(ManagedRuntimeVerifyError::MissingFile {
            name: "ffmpeg.exe".into(),
        });
    }

    Ok(ManagedRuntimeInfo {
        dir: dir.to_path_buf(),
        ffmpeg_exe,
        release_tag: manifest.release_tag.clone(),
        archive_sha256: manifest.archive_sha256.to_ascii_lowercase(),
        manifest_sha256: manifest_sha256.to_ascii_lowercase(),
    })
}

/// True when a damaged/stale managed tree should be repaired (re-downloaded)
/// rather than treated as Ready. External locate hits never satisfy this.
pub fn managed_runtime_needs_repair(
    verify_result: &Result<ManagedRuntimeInfo, ManagedRuntimeVerifyError>,
) -> bool {
    verify_result.is_err()
}

/// Default on-demand install directory: `%LOCALAPPDATA%\\Clipline\\ffmpeg`.
pub fn managed_ffmpeg_dir_from_local_app_data(local_app_data: Option<&Path>) -> Option<PathBuf> {
    Some(local_app_data?.join("Clipline").join("ffmpeg"))
}

/// Classify managed vs external for UI/ensure. `locate_path` comes from
/// `ffmpeg::locate()` (or a test double); managed verification is separate.
pub fn ffmpeg_runtime_status(
    managed_dir: Option<&Path>,
    manifest: &FfmpegRuntimeManifest,
    manifest_sha256: &str,
    locate_path: Option<&Path>,
) -> FfmpegRuntimeStatus {
    let managed = managed_dir.and_then(|dir| {
        verify_managed_ffmpeg_runtime(dir, manifest, manifest_sha256).ok()
    });
    if let Some(info) = managed {
        return FfmpegRuntimeStatus {
            kind: FfmpegDiscoveryKind::ManagedVerified,
            managed: Some(info),
            locate_path: locate_path.map(Path::to_path_buf),
        };
    }
    if locate_path.is_some() {
        return FfmpegRuntimeStatus {
            kind: FfmpegDiscoveryKind::ExternalUnmanaged,
            managed: None,
            locate_path: locate_path.map(Path::to_path_buf),
        };
    }
    FfmpegRuntimeStatus {
        kind: FfmpegDiscoveryKind::Missing,
        managed: None,
        locate_path: None,
    }
}

/// Short UI copy for an install affordance tied to a requirement reason.
pub fn install_affordance_copy(reason: FfmpegRequirementReason) -> &'static str {
    match reason {
        FfmpegRequirementReason::Poster => {
            "Install the FFmpeg runtime to generate clip posters."
        }
        FfmpegRequirementReason::AudioSidecarExtract => {
            "Install the FFmpeg runtime to extract audio tracks."
        }
        FfmpegRequirementReason::ShareableClipboardExport => {
            "Install the FFmpeg runtime to copy a shareable clip."
        }
        FfmpegRequirementReason::SvtAv1 => {
            "Install the FFmpeg runtime to use SVT-AV1 encoding."
        }
        FfmpegRequirementReason::FfmpegBackendEncoder => {
            "Install the FFmpeg runtime to use FFmpeg encoder backends."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mft_h264() -> EncoderCapability {
        EncoderCapability {
            api: EncoderApi::Mft,
            backend: EncoderBackend::Amf,
            codecs: vec![Codec::H264],
        }
    }

    fn ffmpeg_nvenc() -> EncoderCapability {
        EncoderCapability {
            api: EncoderApi::Ffmpeg,
            backend: EncoderBackend::Nvenc,
            codecs: vec![Codec::H264],
        }
    }

    fn svt_av1() -> EncoderCapability {
        EncoderCapability {
            api: EncoderApi::Ffmpeg,
            backend: EncoderBackend::SvtAv1,
            codecs: vec![Codec::Av1],
        }
    }

    #[test]
    fn mft_capability_allows_recording_without_ffmpeg() {
        assert!(recording_without_ffmpeg_possible(&[mft_h264()]));
        assert!(!recording_without_ffmpeg_possible(&[ffmpeg_nvenc(), svt_av1()]));
        assert!(!recording_without_ffmpeg_possible(&[]));
        assert!(!recording_without_ffmpeg_possible(&[EncoderCapability {
            api: EncoderApi::Mft,
            backend: EncoderBackend::Amf,
            codecs: vec![],
        }]));
    }

    #[test]
    fn required_reasons_always_include_poster_sidecar_and_share_export() {
        let reasons = ffmpeg_required_for(&[mft_h264()]);
        assert!(reasons.contains(&FfmpegRequirementReason::Poster));
        assert!(reasons.contains(&FfmpegRequirementReason::AudioSidecarExtract));
        assert!(reasons.contains(&FfmpegRequirementReason::ShareableClipboardExport));
        assert!(!reasons.contains(&FfmpegRequirementReason::SvtAv1));
        assert!(!reasons.contains(&FfmpegRequirementReason::FfmpegBackendEncoder));
    }

    #[test]
    fn ffmpeg_capabilities_add_backend_and_svt_reasons() {
        let reasons = ffmpeg_required_for(&[mft_h264(), ffmpeg_nvenc(), svt_av1()]);
        assert!(reasons.contains(&FfmpegRequirementReason::FfmpegBackendEncoder));
        assert!(reasons.contains(&FfmpegRequirementReason::SvtAv1));
        assert!(reasons.contains(&FfmpegRequirementReason::ShareableClipboardExport));
    }

    #[test]
    fn ensure_noop_only_for_managed_verified() {
        assert!(ensure_ffmpeg_runtime_is_noop(
            FfmpegDiscoveryKind::ManagedVerified
        ));
        assert!(!ensure_ffmpeg_runtime_is_noop(
            FfmpegDiscoveryKind::ExternalUnmanaged
        ));
        assert!(!ensure_ffmpeg_runtime_is_noop(FfmpegDiscoveryKind::Missing));
    }

    #[test]
    fn classify_discovery_prefers_managed_over_locate() {
        assert_eq!(
            classify_ffmpeg_discovery(true, true),
            FfmpegDiscoveryKind::ManagedVerified
        );
        assert_eq!(
            classify_ffmpeg_discovery(false, true),
            FfmpegDiscoveryKind::ExternalUnmanaged
        );
        assert_eq!(
            classify_ffmpeg_discovery(false, false),
            FfmpegDiscoveryKind::Missing
        );
    }

    #[test]
    fn shareable_clipboard_affordance_mentions_install() {
        let copy = install_affordance_copy(FfmpegRequirementReason::ShareableClipboardExport);
        assert!(copy.to_ascii_lowercase().contains("install"));
        assert!(copy.to_ascii_lowercase().contains("ffmpeg"));
        assert!(copy.to_ascii_lowercase().contains("shareable"));
    }

    fn tiny_manifest(files: Vec<FfmpegAllowedFile>) -> FfmpegRuntimeManifest {
        FfmpegRuntimeManifest {
            schema_version: 1,
            provider: "test-provider".into(),
            release_tag: "test-tag".into(),
            published_at: "2026-01-01T00:00:00Z".into(),
            archive_name: "ffmpeg-test.zip".into(),
            archive_url: "https://example.test/ffmpeg-test.zip".into(),
            archive_sha256: "aa".repeat(32),
            archive_size: 128,
            archive_root: "ffmpeg-test-root".into(),
            version_line: "ffmpeg version test".into(),
            source_offer_url: "https://example.test/source".into(),
            ffmpeg_source_url: "https://example.test/ffmpeg".into(),
            allowed_files: files,
        }
    }

    fn write_bytes(dir: &std::path::Path, name: &str, bytes: &[u8]) -> String {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        hex_sha256(bytes)
    }

    fn write_provenance(
        dir: &std::path::Path,
        manifest: &FfmpegRuntimeManifest,
        manifest_sha256: &str,
        files: &[(String, u64, String)],
    ) {
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
        std::fs::write(
            dir.join("PROVENANCE.json"),
            serde_json::to_string_pretty(&provenance).unwrap() + "\n",
        )
        .unwrap();
    }

    fn happy_tree() -> (clipline_test_utils::TestDir, FfmpegRuntimeManifest, String) {
        let dir = clipline_test_utils::TestDir::new("clipline-ffmpeg", "managed-ok");
        let exe_bytes = b"ffmpeg-bytes";
        let dll_bytes = b"avcodec-bytes";
        let exe_hash = write_bytes(dir.path(), "ffmpeg.exe", exe_bytes);
        let dll_hash = write_bytes(dir.path(), "avcodec-62.dll", dll_bytes);
        let manifest = tiny_manifest(vec![
            FfmpegAllowedFile {
                archive_path: "bin/ffmpeg.exe".into(),
                staged_name: "ffmpeg.exe".into(),
                size: exe_bytes.len() as u64,
                sha256: exe_hash.clone(),
            },
            FfmpegAllowedFile {
                archive_path: "bin/avcodec-62.dll".into(),
                staged_name: "avcodec-62.dll".into(),
                size: dll_bytes.len() as u64,
                sha256: dll_hash.clone(),
            },
        ]);
        let manifest_sha256 = hex_sha256(b"committed-manifest-bytes");
        write_provenance(
            dir.path(),
            &manifest,
            &manifest_sha256,
            &[
                ("ffmpeg.exe".into(), exe_bytes.len() as u64, exe_hash),
                ("avcodec-62.dll".into(), dll_bytes.len() as u64, dll_hash),
            ],
        );
        (dir, manifest, manifest_sha256)
    }

    #[test]
    fn verify_accepts_happy_managed_tree() {
        let (dir, manifest, manifest_sha256) = happy_tree();
        let info = verify_managed_ffmpeg_runtime(dir.path(), &manifest, &manifest_sha256)
            .expect("happy tree should verify");
        assert_eq!(info.release_tag, "test-tag");
        assert_eq!(info.ffmpeg_exe, dir.path().join("ffmpeg.exe"));
        assert!(!managed_runtime_needs_repair(&Ok(info)));
    }

    #[test]
    fn verify_rejects_tampered_allowlisted_dll() {
        let (dir, manifest, manifest_sha256) = happy_tree();
        std::fs::write(dir.path().join("avcodec-62.dll"), b"tampered-dll-bytes").unwrap();
        let err = verify_managed_ffmpeg_runtime(dir.path(), &manifest, &manifest_sha256)
            .expect_err("tampered DLL must fail");
        assert!(matches!(
            err,
            ManagedRuntimeVerifyError::HashMismatch { .. }
                | ManagedRuntimeVerifyError::SizeMismatch { .. }
        ));
        assert!(managed_runtime_needs_repair(&Err(err)));
    }

    #[test]
    fn verify_rejects_missing_or_stale_provenance() {
        let (dir, manifest, manifest_sha256) = happy_tree();
        std::fs::remove_file(dir.path().join("PROVENANCE.json")).unwrap();
        assert!(matches!(
            verify_managed_ffmpeg_runtime(dir.path(), &manifest, &manifest_sha256),
            Err(ManagedRuntimeVerifyError::MissingProvenance)
        ));

        let (dir, manifest, _) = happy_tree();
        write_provenance(
            dir.path(),
            &manifest,
            "bb".repeat(32).as_str(),
            &[
                (
                    "ffmpeg.exe".into(),
                    12,
                    hex_sha256(b"ffmpeg-bytes"),
                ),
                (
                    "avcodec-62.dll".into(),
                    13,
                    hex_sha256(b"avcodec-bytes"),
                ),
            ],
        );
        let err = verify_managed_ffmpeg_runtime(dir.path(), &manifest, &"aa".repeat(32))
            .expect_err("stale provenance must fail");
        assert!(matches!(
            err,
            ManagedRuntimeVerifyError::ProvenanceMismatch { field, .. } if field == "manifest_sha256"
        ));
    }

    #[test]
    fn runtime_status_reports_override_or_path_as_external() {
        let (dir, manifest, manifest_sha256) = happy_tree();
        let managed = ffmpeg_runtime_status(
            Some(dir.path()),
            &manifest,
            &manifest_sha256,
            Some(Path::new("C:/tools/ffmpeg.exe")),
        );
        assert_eq!(managed.kind, FfmpegDiscoveryKind::ManagedVerified);
        assert!(ensure_ffmpeg_runtime_is_noop(managed.kind));

        // Damaged managed tree + locate hit => external, not managed.
        std::fs::write(dir.path().join("avcodec-62.dll"), b"nope").unwrap();
        let external = ffmpeg_runtime_status(
            Some(dir.path()),
            &manifest,
            &manifest_sha256,
            Some(Path::new("C:/tools/ffmpeg.exe")),
        );
        assert_eq!(external.kind, FfmpegDiscoveryKind::ExternalUnmanaged);
        assert!(!ensure_ffmpeg_runtime_is_noop(external.kind));
        assert!(managed_runtime_needs_repair(
            &verify_managed_ffmpeg_runtime(dir.path(), &manifest, &manifest_sha256)
        ));

        let missing = ffmpeg_runtime_status(None, &manifest, &manifest_sha256, None);
        assert_eq!(missing.kind, FfmpegDiscoveryKind::Missing);
    }

    #[test]
    fn managed_dir_uses_local_app_data_clipline_ffmpeg() {
        let root = Path::new("C:/Users/test/AppData/Local");
        assert_eq!(
            managed_ffmpeg_dir_from_local_app_data(Some(root)),
            Some(root.join("Clipline").join("ffmpeg"))
        );
        assert_eq!(managed_ffmpeg_dir_from_local_app_data(None), None);
    }
}
