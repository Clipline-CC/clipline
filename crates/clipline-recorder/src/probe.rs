//! Concrete bounded catalogs produced by the Settings probe executor.

#[cfg(windows)]
use clipline_settings::BoundedProbePayload;

pub const MAX_PROBE_ENCODERS: usize = 32;
pub const MAX_PROBE_GAME_PLUGINS: usize = 16;
pub const MAX_PROBE_GAME_ROWS: usize = 256;
pub const MAX_PROBE_TEXT_BYTES: usize = 64 * 1024;
pub const MAX_PROBE_CATALOG_BYTES: usize = 4 * 1024 * 1024;

/// One selectable encoder for the Settings dropdown.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EncoderOption {
    /// VideoEncoder settings id (e.g. "amf_hevc").
    pub id: String,
    /// Human label (e.g. "AMD AMF · HEVC").
    pub name: String,
    /// Codec key the frontend matches against native playback capability.
    pub codec: String,
}

#[cfg(windows)]
#[derive(Debug, Clone)]
pub enum SettingsProbeCatalog {
    Displays(Vec<clipline_capture::windows::display::DisplayInfo>),
    AudioEndpoints(clipline_capture::windows::wasapi::AudioDeviceList),
    Encoders(Vec<EncoderOption>),
    GameWindows(Vec<clipline_games::detection::GameWindowInfo>),
    InstalledGames(Vec<clipline_games::discovery::DetectedGameCandidate>),
    GamePlugins(Vec<clipline_games::plugin::GamePluginInfo>),
    Storage(clipline_storage::StorageStatus),
    /// Reserved for Task 6's real configured-decoder capability probe.
    PlaybackCapabilitiesPending,
}

#[cfg(windows)]
impl SettingsProbeCatalog {
    pub const fn kind(&self) -> clipline_settings::ProbeKind {
        match self {
            Self::Displays(_) => clipline_settings::ProbeKind::Displays,
            Self::AudioEndpoints(_) => clipline_settings::ProbeKind::AudioEndpoints,
            Self::Encoders(_) => clipline_settings::ProbeKind::Encoders,
            Self::GameWindows(_) => clipline_settings::ProbeKind::GameWindows,
            Self::InstalledGames(_) => clipline_settings::ProbeKind::InstalledGames,
            Self::GamePlugins(_) => clipline_settings::ProbeKind::GamePlugins,
            Self::Storage(_) => clipline_settings::ProbeKind::Storage,
            Self::PlaybackCapabilitiesPending => clipline_settings::ProbeKind::PlaybackCapabilities,
        }
    }
}

#[cfg(windows)]
impl BoundedProbePayload for SettingsProbeCatalog {
    fn validate_bounds(&self) -> Result<(), String> {
        match self {
            Self::Displays(displays) => {
                clipline_capture::windows::display::validate_display_catalog(displays)
                    .map_err(|error| error.to_string())
            }
            Self::AudioEndpoints(devices) => {
                clipline_capture::windows::wasapi::validate_audio_device_catalog(devices)
                    .map_err(|error| error.to_string())
            }
            Self::Encoders(encoders) => validate_encoders(encoders),
            Self::GameWindows(windows) => {
                validate_count("game windows", windows.len(), MAX_PROBE_GAME_ROWS)?;
                validate_strings(windows.iter().flat_map(|window| {
                    [
                        Some(window.title.as_str()),
                        Some(window.exe_name.as_str()),
                        window.exe_path.as_deref(),
                    ]
                }))
            }
            Self::InstalledGames(games) => {
                clipline_games::discovery::validate_discovery_candidates(games)
            }
            Self::GamePlugins(plugins) => clipline_games::plugin::validate_plugin_catalog(plugins),
            Self::Storage(_) | Self::PlaybackCapabilitiesPending => Ok(()),
        }
    }
}

pub fn validate_encoders(encoders: &[EncoderOption]) -> Result<(), String> {
    validate_count("encoders", encoders.len(), MAX_PROBE_ENCODERS)?;
    validate_strings(encoders.iter().flat_map(|encoder| {
        [
            Some(encoder.id.as_str()),
            Some(encoder.name.as_str()),
            Some(encoder.codec.as_str()),
        ]
    }))
}

fn validate_count(label: &str, actual: usize, maximum: usize) -> Result<(), String> {
    if actual > maximum {
        Err(format!("{label} count {actual} exceeds {maximum}"))
    } else {
        Ok(())
    }
}

fn validate_strings<'a>(values: impl Iterator<Item = Option<&'a str>>) -> Result<(), String> {
    let mut aggregate = 0usize;
    for value in values.flatten() {
        if value.len() > MAX_PROBE_TEXT_BYTES {
            return Err(format!(
                "probe text is {} bytes; maximum is {MAX_PROBE_TEXT_BYTES}",
                value.len()
            ));
        }
        aggregate = aggregate
            .checked_add(value.len())
            .ok_or_else(|| "probe catalog byte count overflowed".to_string())?;
    }
    if aggregate > MAX_PROBE_CATALOG_BYTES {
        return Err(format!(
            "probe catalog is {aggregate} bytes; maximum is {MAX_PROBE_CATALOG_BYTES}"
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn codec_id(codec: crate::service::Codec) -> &'static str {
    match codec {
        crate::service::Codec::Av1 => "av1",
        crate::service::Codec::Hevc => "hevc",
        crate::service::Codec::H264 => "h264",
    }
}

/// Encoders this machine can actually use, ordered by recorder preference.
#[cfg(windows)]
pub fn available_encoder_options() -> Vec<EncoderOption> {
    available_encoder_options_bounded().unwrap_or_default()
}

#[cfg(windows)]
pub fn available_encoder_options_bounded() -> Result<Vec<EncoderOption>, String> {
    use clipline_capture::probe::EncoderCandidate;
    use clipline_settings::VideoEncoder;

    use crate::service::VideoEncoderRuntimeExt as _;

    let mut seen = std::collections::BTreeSet::new();
    let mut options = Vec::new();
    options
        .try_reserve_exact(MAX_PROBE_ENCODERS)
        .map_err(|_| "reserve bounded encoder option catalog".to_string())?;
    for capability in crate::service::encoder_capabilities() {
        for &codec in &capability.codecs {
            let Some(encoder) = VideoEncoder::from_parts(capability.backend, codec) else {
                continue;
            };
            if !seen.insert(encoder.id()) {
                continue;
            }
            if options.len() == MAX_PROBE_ENCODERS {
                return Err(format!("encoder option count exceeds {MAX_PROBE_ENCODERS}"));
            }
            let candidate = EncoderCandidate {
                api: capability.api,
                backend: capability.backend,
                codec,
            };
            options.push(EncoderOption {
                id: encoder.id().to_string(),
                name: crate::service::encoder_label(candidate),
                codec: codec_id(codec).to_string(),
            });
        }
    }
    validate_encoders(&options)?;
    Ok(options)
}
