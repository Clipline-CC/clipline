//! Encoder capability probing and encoder construction.
use super::*;

/// Process-static MFT capabilities (hardware probe is stable for the process).
pub(super) fn mft_capabilities_cached() -> &'static [EncoderCapability] {
    use std::sync::OnceLock;
    static MFT_CAPS: OnceLock<Vec<EncoderCapability>> = OnceLock::new();
    MFT_CAPS.get_or_init(|| mft_probe::enumerate().unwrap_or_default())
}

#[derive(Debug, Clone)]
pub(super) struct FfmpegCapabilitySlot {
    pub(super) identity: String,
    pub(super) caps: Vec<EncoderCapability>,
}

pub(super) fn ffmpeg_caps_slot() -> &'static std::sync::Mutex<Option<FfmpegCapabilitySlot>> {
    use std::sync::{Mutex, OnceLock};
    static FFMPEG_CAPS: OnceLock<Mutex<Option<FfmpegCapabilitySlot>>> = OnceLock::new();
    FFMPEG_CAPS.get_or_init(|| Mutex::new(None))
}

pub(super) fn ffmpeg_capability_slot(
    existing: Option<&FfmpegCapabilitySlot>,
    identity: &str,
    probe: impl FnOnce() -> Vec<EncoderCapability>,
) -> FfmpegCapabilitySlot {
    if let Some(existing) = existing {
        if existing.identity == identity {
            return existing.clone();
        }
    }
    FfmpegCapabilitySlot {
        identity: identity.to_string(),
        caps: probe(),
    }
}

/// Identity for the replaceable FFmpeg half: managed provenance, external path, or missing.
pub fn ffmpeg_capability_identity(
    managed: Option<&crate::ffmpeg_runtime::ManagedRuntimeInfo>,
    locate_path: Option<&std::path::Path>,
) -> String {
    if let Some(info) = managed {
        return format!("managed:{}:{}", info.dir.display(), info.manifest_sha256);
    }
    if let Some(path) = locate_path {
        return format!("external:{}", path.display());
    }
    "missing".to_string()
}

pub(super) fn current_ffmpeg_capability_identity() -> String {
    let local = std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from);
    let managed_dir = local
        .as_ref()
        .map(|path| crate::ffmpeg_install::managed_root(path));
    let locate = ffmpeg::locate();
    let status =
        crate::ffmpeg_install::runtime_status_for_dirs(managed_dir.as_deref(), locate.as_deref())
            .unwrap_or_else(|_| crate::ffmpeg_runtime::FfmpegRuntimeStatus {
                kind: crate::ffmpeg_runtime::FfmpegDiscoveryKind::Missing,
                managed: None,
                locate_path: locate.clone(),
            });
    ffmpeg_capability_identity(status.managed.as_ref(), status.locate_path.as_deref())
}

pub(super) fn ffmpeg_capabilities_cached() -> Vec<EncoderCapability> {
    let identity = current_ffmpeg_capability_identity();
    let slot_lock = ffmpeg_caps_slot();
    let mut guard = match slot_lock.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let next = ffmpeg_capability_slot(guard.as_ref(), &identity, ffmpeg::probe);
    *guard = Some(next.clone());
    next.caps
}

/// Drop the FFmpeg capability slot and re-probe against the current runtime identity.
pub fn refresh_ffmpeg_encoder_capabilities() -> Vec<EncoderOption> {
    {
        let slot_lock = ffmpeg_caps_slot();
        let mut guard = match slot_lock.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard = None;
    }
    available_encoder_options()
}

/// Combined MFT (process-static) + FFmpeg (replaceable/versioned) capabilities.
pub(super) fn encoder_capabilities() -> Vec<EncoderCapability> {
    let mut caps = mft_capabilities_cached().to_vec();
    caps.extend(ffmpeg_capabilities_cached());
    caps
}

#[cfg(test)]
pub(super) fn output_dimensions(in_w: u32, in_h: u32, resolution: OutputResolution) -> (u32, u32) {
    output_dimensions_with_bounds(in_w, in_h, resolution, None)
}

pub(super) fn output_dimensions_with_bounds(
    in_w: u32,
    in_h: u32,
    resolution: OutputResolution,
    bounds: Option<OutputResolutionBounds>,
) -> (u32, u32) {
    let max_box = bounds
        .map(|bounds| (bounds.width, bounds.height))
        .or_else(|| resolution.bounds())
        .unwrap_or((2560, u32::MAX));
    let scale = (max_box.0 as f64 / in_w.max(1) as f64)
        .min(max_box.1 as f64 / in_h.max(1) as f64)
        .min(1.0);
    even_dimensions(
        (in_w as f64 * scale).round() as u32,
        (in_h as f64 * scale).round() as u32,
    )
}

/// Build the recorder's video encoder by walking the ranked candidate list
/// until one opens. Returns the boxed encoder and the candidate that won so
/// the caller can report it. Warns the user once if an explicit choice could
/// not be honored and Auto fallback was used instead.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_encoder(
    device: &ID3D11Device,
    opts: &ServiceOptions,
    in_w: u32,
    in_h: u32,
    enc_w: u32,
    enc_h: u32,
    events: &Sender<Event>,
) -> Result<(Box<dyn Encoder>, EncoderCandidate), String> {
    let preference = opts.video_encoder.preference();
    let capabilities = encoder_capabilities();
    let candidates = rank_encoders(&capabilities, &opts.decodable_codecs, preference);
    if candidates.is_empty() {
        return Err("init: no usable video encoder found on this system".into());
    }

    let explicit_target = match preference {
        EncoderPreference::Explicit { backend, codec } => Some((backend, codec)),
        EncoderPreference::Auto => None,
    };
    let ffmpeg_path = ffmpeg::locate();
    let mut last_err = String::new();
    for candidate in &candidates {
        match open_candidate(
            *candidate,
            device,
            opts,
            in_w,
            in_h,
            enc_w,
            enc_h,
            &ffmpeg_path,
        ) {
            Ok(encoder) => {
                // If the user forced a specific encoder/codec and we ended up
                // on a different one — whether the choice failed to open or
                // was never offered (so it isn't even in `candidates`) — tell
                // them we downgraded.
                if let Some((backend, codec)) = explicit_target {
                    if candidate.backend != backend || candidate.codec != codec {
                        let reason = if last_err.is_empty() {
                            "not available on this system".to_string()
                        } else {
                            last_err.clone()
                        };
                        warn_user(
                            events,
                            format!(
                                "{:?} encoder unavailable ({reason}); using {} instead",
                                opts.video_encoder,
                                encoder_label(*candidate)
                            ),
                        );
                    }
                }
                return Ok((encoder, *candidate));
            }
            Err(e) => last_err = e,
        }
    }
    Err(format!(
        "init: no video encoder could be opened: {last_err}"
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FfmpegConversionPath {
    Gpu,
    Cpu,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MftEncoderPath {
    Hardware,
    Software,
}

pub(super) fn mft_encoder_path(backend: EncoderBackend) -> MftEncoderPath {
    if backend == EncoderBackend::MfSoftware {
        MftEncoderPath::Software
    } else {
        MftEncoderPath::Hardware
    }
}

pub(super) fn ffmpeg_conversion_path(backend: EncoderBackend) -> FfmpegConversionPath {
    if backend == EncoderBackend::MfSoftware {
        FfmpegConversionPath::Cpu
    } else {
        FfmpegConversionPath::Gpu
    }
}

/// Construct one candidate encoder. MFT uses the zero-copy GPU H.264 path;
/// FFmpeg hardware backends convert BGRA→NV12 on the GPU, while `MfSoftware`
/// uses readback and CPU conversion so it works without a video processor.
#[allow(clippy::too_many_arguments)]
pub(super) fn open_candidate(
    candidate: EncoderCandidate,
    device: &ID3D11Device,
    opts: &ServiceOptions,
    in_w: u32,
    in_h: u32,
    enc_w: u32,
    enc_h: u32,
    ffmpeg_path: &Option<PathBuf>,
) -> Result<Box<dyn Encoder>, String> {
    match candidate.api {
        EncoderApi::Mft => {
            let cfg = MftConfig {
                width: enc_w,
                height: enc_h,
                fps: opts.fps,
                bitrate_bps: opts.bitrate_bps,
                encoder_backend: Some(candidate.backend),
            };
            match mft_encoder_path(candidate.backend) {
                MftEncoderPath::Software => SoftwareMftH264Encoder::new(device, in_w, in_h, cfg)
                    .map(|encoder| Box::new(encoder) as Box<dyn Encoder>)
                    .map_err(|error| error.to_string()),
                MftEncoderPath::Hardware => MftH264Encoder::new(device, in_w, in_h, cfg)
                    .map(|encoder| Box::new(encoder) as Box<dyn Encoder>)
                    .map_err(|error| error.to_string()),
            }
        }
        EncoderApi::Ffmpeg => {
            let ffmpeg = ffmpeg_path
                .as_deref()
                .ok_or_else(|| "ffmpeg not located".to_string())?;
            let encoder = match ffmpeg_conversion_path(candidate.backend) {
                FfmpegConversionPath::Gpu => FfmpegVideoEncoder::new_on(
                    device,
                    ffmpeg,
                    candidate.backend,
                    candidate.codec,
                    in_w,
                    in_h,
                    None,
                    enc_w,
                    enc_h,
                    opts.fps,
                    opts.bitrate_bps,
                ),
                FfmpegConversionPath::Cpu => FfmpegVideoEncoder::new_cpu_on(
                    device,
                    ffmpeg,
                    candidate.backend,
                    candidate.codec,
                    in_w,
                    in_h,
                    None,
                    enc_w,
                    enc_h,
                    opts.fps,
                    opts.bitrate_bps,
                ),
            };
            encoder
                .map(|e| Box::new(e) as Box<dyn Encoder>)
                .map_err(|e| e.to_string())
        }
    }
}
