//! FFmpeg capability matrix and discovery kinds (slim-core Milestone B).
//!
//! Pure helpers only. Managed-runtime verification (hashes/provenance) lands in
//! Task B2; download/ensure state machine in B3. `clipline_capture::ffmpeg::locate`
//! remains discovery of a runnable binary — it does **not** mean ManagedVerified.

use clipline_capture::{Codec, EncoderApi, EncoderBackend, EncoderCapability};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}
