use thiserror::Error;

use std::rc::Rc;

use windows_core::Error as WinError;

use crate::{
    BackendError, H264DecoderConfig, H264PlaybackSupport, PlaybackCapabilities, VideoAcceleration,
};

use super::com::{ComApartment, MediaFoundationRuntime};
use super::d3d11::PlaybackD3D11Device;
use super::mft_decode::{classify_device_failure, configure_h264_capability_tier};

// Exact AVC configuration written by Clipline's production hybrid-writer
// fixture. Keeping the bounded probe profile in-tree avoids filesystem work
// while still proving that the MFT accepts a real Clipline-authored format.
const PROBE_SPS: &[u8] = &[
    0x67, 0x64, 0x00, 0x1e, 0xac, 0x2b, 0x40, 0x50, 0x17, 0xfc, 0xb0, 0x80, 0x00, 0x00, 0x03, 0x00,
    0x80, 0x00, 0x00, 0x1e, 0x06, 0xd0, 0x44, 0x23, 0x50,
];
const PROBE_PPS: &[u8] = &[0x68, 0xce, 0x3c, 0x30];

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PlaybackCapabilityProbeError {
    #[error("native playback capability backend failed: {0}")]
    Backend(#[source] BackendError),
    #[error("native playback capability probe became stale: {0}")]
    Checkpoint(String),
    #[error("invalid built-in H.264 capability profile: {0}")]
    InvalidProfile(String),
}

impl From<BackendError> for PlaybackCapabilityProbeError {
    fn from(value: BackendError) -> Self {
        Self::Backend(value)
    }
}

pub fn probe_playback_capabilities() -> Result<PlaybackCapabilities, PlaybackCapabilityProbeError> {
    probe_playback_capabilities_with_checkpoint(|| Ok(()))
}

pub fn probe_playback_capabilities_with_checkpoint(
    mut checkpoint_after_activation: impl FnMut() -> Result<(), String>,
) -> Result<PlaybackCapabilities, PlaybackCapabilityProbeError> {
    let config = probe_config()?;
    let _apartment = ComApartment::multithreaded()
        .map_err(|error| backend_error(error, "initialize playback capability COM apartment"))?;
    let _media_foundation = MediaFoundationRuntime::acquire()
        .map_err(|error| backend_error(error, "start playback capability Media Foundation"))?;
    let device = Rc::new(
        PlaybackD3D11Device::hardware()
            .map_err(|error| backend_error(error, "create playback capability D3D11 device"))?,
    );
    checkpoint_after_activation().map_err(PlaybackCapabilityProbeError::Checkpoint)?;
    let adapter_luid = device.adapter_luid();
    let h264 = resolve_h264_support(
        || {
            device.reset_manager().map_err(|error| {
                backend_from_windows(error, "reset hardware capability manager")
            })?;
            configure_h264_capability_tier(&device, VideoAcceleration::Hardware, &config)
        },
        || {
            device.reset_manager().map_err(|error| {
                backend_from_windows(error, "reset software capability manager")
            })?;
            configure_h264_capability_tier(&device, VideoAcceleration::Software, &config)
        },
    )?;
    Ok(PlaybackCapabilities::new(adapter_luid, h264))
}

fn backend_error(error: WinError, context: &str) -> PlaybackCapabilityProbeError {
    PlaybackCapabilityProbeError::Backend(backend_from_windows(error, context))
}

fn backend_from_windows(error: WinError, context: &str) -> BackendError {
    let mut backend = classify_device_failure(error.code().0);
    backend.message = format!("{context}: {error}");
    backend
}

fn resolve_h264_support(
    hardware: impl FnOnce() -> Result<bool, BackendError>,
    software: impl FnOnce() -> Result<bool, BackendError>,
) -> Result<H264PlaybackSupport, BackendError> {
    if hardware()? {
        return Ok(H264PlaybackSupport::ConfiguredHardware);
    }
    if software()? {
        Ok(H264PlaybackSupport::ConfiguredSoftware)
    } else {
        Ok(H264PlaybackSupport::Unavailable)
    }
}

fn probe_config() -> Result<H264DecoderConfig, PlaybackCapabilityProbeError> {
    H264DecoderConfig::new(
        640,
        360,
        4,
        vec![PROBE_SPS.to_vec()],
        vec![PROBE_PPS.to_vec()],
    )
    .map_err(|error| PlaybackCapabilityProbeError::InvalidProfile(error.to_string()))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use crate::{BackendComponent, BackendErrorKind, RecoveryDisposition};

    use super::*;

    #[test]
    fn hardware_success_skips_the_software_tier() {
        let software_called = Cell::new(false);
        let resolved = resolve_h264_support(
            || Ok(true),
            || {
                software_called.set(true);
                Ok(true)
            },
        )
        .unwrap();
        assert_eq!(resolved, H264PlaybackSupport::ConfiguredHardware);
        assert!(!software_called.get());
    }

    #[test]
    fn hardware_rejection_uses_software_and_two_rejections_are_unavailable() {
        assert_eq!(
            resolve_h264_support(|| Ok(false), || Ok(true)).unwrap(),
            H264PlaybackSupport::ConfiguredSoftware
        );
        assert_eq!(
            resolve_h264_support(|| Ok(false), || Ok(false)).unwrap(),
            H264PlaybackSupport::Unavailable
        );
    }

    #[test]
    fn device_loss_or_probe_failure_never_becomes_a_passing_capability() {
        let lost = BackendError {
            component: BackendComponent::VideoDecoder,
            kind: BackendErrorKind::DeviceLost,
            recovery: RecoveryDisposition::RecreateComponent,
            native_code: Some(-1),
            message: "device lost".into(),
        };
        assert_eq!(
            resolve_h264_support(|| Err(lost.clone()), || Ok(true)),
            Err(lost)
        );
    }
}
