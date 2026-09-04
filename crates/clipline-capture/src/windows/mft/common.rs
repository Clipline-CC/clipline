use super::*;

/// eAVEncH264VProfile_High (codecapi.h) — windows-rs feature placement of
/// the enum varies; the wire value is stable.
pub(crate) const H264_PROFILE_HIGH: u32 = 100;
pub(crate) const MFT_EVENT_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const MFT_EVENT_POLL_INTERVAL: Duration = Duration::from_millis(2);

pub(crate) fn take_and_clear_manually_drop_option<T>(slot: &mut ManuallyDrop<Option<T>>) -> Option<T> {
    // SAFETY: the value is immediately replaced with `None`, so the owner can
    // still drop its field without releasing the moved value a second time.
    let value = unsafe { ManuallyDrop::take(slot) };
    *slot = ManuallyDrop::new(None);
    value
}

pub(crate) struct OwnedMftOutputBuffer {
    raw: MFT_OUTPUT_DATA_BUFFER,
}

pub(crate) struct ReusableMftOutputSample {
    pub(crate) sample: IMFSample,
    pub(crate) capacity: u32,
    pub(crate) alignment: u32,
}

pub(crate) struct OwnedMftActivation(pub(crate) IMFActivate);

impl Drop for OwnedMftActivation {
    fn drop(&mut self) {
        // SAFETY: the owner called ActivateObject and therefore owns the
        // matching activation shutdown responsibility. This is also harmless
        // for transforms that do not require an explicit shutdown.
        let _ = unsafe { self.0.ShutdownObject() };
    }
}

impl OwnedMftOutputBuffer {
    pub(crate) fn new(output_id: u32) -> Self {
        Self::with_sample(output_id, None)
    }

    pub(crate) fn with_sample(output_id: u32, sample: Option<IMFSample>) -> Self {
        Self {
            raw: MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: output_id,
                pSample: ManuallyDrop::new(sample),
                ..Default::default()
            },
        }
    }

    pub(crate) fn raw_mut(&mut self) -> &mut MFT_OUTPUT_DATA_BUFFER {
        &mut self.raw
    }

    pub(crate) fn take_sample(&mut self) -> Option<IMFSample> {
        take_and_clear_manually_drop_option(&mut self.raw.pSample)
    }
}

impl Drop for OwnedMftOutputBuffer {
    fn drop(&mut self) {
        // SAFETY: ProcessOutput transfers these fields to its caller on every
        // result. A taken sample is replaced with None before this guard drops.
        unsafe {
            ManuallyDrop::drop(&mut self.raw.pSample);
            ManuallyDrop::drop(&mut self.raw.pEvents);
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MftConfig {
    /// Encode size; must already be even (`annexb::even_dimensions`).
    pub width: u32,
    pub height: u32,
    /// Nominal fps for media types + first-frame duration fallback.
    pub fps: u32,
    pub bitrate_bps: u32,
    /// None means automatic hardware H.264 selection.
    pub encoder_backend: Option<EncoderBackend>,
}

pub(crate) fn backend(e: windows::core::Error) -> EncodeError {
    EncodeError::Backend(e.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MftEventKind {
    NeedInput,
    HaveOutput,
    DrainComplete,
    Error,
    Other(u32),
}

pub(crate) fn classify_mft_event_type(ty: u32) -> MftEventKind {
    if ty == METransformNeedInput.0 as u32 {
        MftEventKind::NeedInput
    } else if ty == METransformHaveOutput.0 as u32 {
        MftEventKind::HaveOutput
    } else if ty == METransformDrainComplete.0 as u32 {
        MftEventKind::DrainComplete
    } else if ty == MEError.0 as u32 {
        MftEventKind::Error
    } else {
        MftEventKind::Other(ty)
    }
}

pub(crate) fn mft_event_error(event: &windows::Win32::Media::MediaFoundation::IMFMediaEvent) -> EncodeError {
    match unsafe { event.GetStatus() } {
        Ok(status) if status.is_err() => EncodeError::Backend(format!(
            "MFT encoder event error: {}",
            windows::core::Error::from(status)
        )),
        Ok(_) => EncodeError::Backend("MFT encoder reported MEError".into()),
        Err(e) => backend(e),
    }
}

pub(crate) fn mft_unexpected_event_error(ty: u32) -> EncodeError {
    EncodeError::Backend(format!("MFT encoder unexpected event type {ty}"))
}

pub(crate) fn mft_event_timeout_error(waiting_for: &str) -> EncodeError {
    EncodeError::Backend(format!("MFT encoder timed out waiting for {waiting_for}"))
}

pub(crate) fn h264_activate(
    activates: &[windows::Win32::Media::MediaFoundation::IMFActivate],
    requested: Option<EncoderBackend>,
) -> Option<&windows::Win32::Media::MediaFoundation::IMFActivate> {
    match requested {
        // Forced backend: match on vendor ID. No fallback here — the app
        // service layer decides whether to retry as Automatic.
        Some(requested) => activates
            .iter()
            .find(|activate| mft_probe::backend_of(activate) == Some(requested)),
        // Automatic: trust MFTEnumEx merit order (SORTANDFILTER). A fixed
        // vendor priority risked preferring an adapter the capture D3D device
        // can't bind, and the Automatic arm has no retry path in the service.
        None => activates.first(),
    }
}

/// `MFCreateAlignedMemoryBuffer` takes an alignment mask (`boundary - 1`).
/// Stream-info values seen in the wild are documented masks, but accepting a
/// power-of-two boundary as well keeps third-party synchronous MFTs safe.
pub(crate) fn mf_alignment_mask(required: u32) -> Result<u32, EncodeError> {
    let boundary = match required.checked_add(1) {
        Some(value) if value.is_power_of_two() => value,
        _ => required.checked_next_power_of_two().ok_or_else(|| {
            EncodeError::Backend("Media Foundation buffer alignment overflow".into())
        })?,
    }
    .max(16);
    Ok(boundary - 1)
}

/// VT_UI4 VARIANT for ICodecAPI (no Drop needed for plain integers).
pub(crate) fn variant_u32(value: u32) -> windows::Win32::System::Variant::VARIANT {
    use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_UI4};
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_UI4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { ulVal: value },
            }),
        },
    }
}

/// eAVEncCommonRateControlMode_CBR (codecapi.h).
pub(crate) const RATE_CONTROL_MODE_CBR: u32 = 0;

pub(crate) fn set_rec709_limited_attrs(
    media_type: &windows::Win32::Media::MediaFoundation::IMFMediaType,
) -> windows::core::Result<()> {
    unsafe {
        media_type.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        media_type.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        media_type.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
        media_type.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
    }
    Ok(())
}

pub(crate) fn sequence_header_sps_pps(
    media_type: &windows::Win32::Media::MediaFoundation::IMFMediaType,
) -> Option<(Vec<u8>, Vec<u8>)> {
    // SAFETY: blob getters with a correctly sized out buffer.
    unsafe {
        let len = media_type.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER).ok()?;
        let mut blob = vec![0u8; len as usize];
        media_type
            .GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut blob, None)
            .ok()?;
        extract_sps_pps(&blob)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    struct DropSpy(Rc<Cell<usize>>);

    impl Drop for DropSpy {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[test]
    fn taking_a_manually_dropped_option_clears_its_owner_slot() {
        let drops = Rc::new(Cell::new(0));
        let mut slot = ManuallyDrop::new(Some(DropSpy(drops.clone())));

        let value = take_and_clear_manually_drop_option(&mut slot).expect("owned value");
        assert!((*slot).is_none());
        assert_eq!(drops.get(), 0);

        drop(value);
        assert_eq!(drops.get(), 1);
        unsafe { ManuallyDrop::drop(&mut slot) };
        assert_eq!(
            drops.get(),
            1,
            "cleared owner must not double-drop the value"
        );

        let mut untouched = ManuallyDrop::new(Some(DropSpy(drops.clone())));
        unsafe { ManuallyDrop::drop(&mut untouched) };
        assert_eq!(drops.get(), 2, "untaken owner must release its value once");
    }

    #[test]
    fn classifies_mft_error_event_as_error() {
        assert_eq!(
            classify_mft_event_type(MEError.0 as u32),
            MftEventKind::Error
        );
        assert_eq!(
            classify_mft_event_type(METransformNeedInput.0 as u32),
            MftEventKind::NeedInput
        );
        assert_eq!(
            classify_mft_event_type(METransformHaveOutput.0 as u32),
            MftEventKind::HaveOutput
        );
        assert_eq!(
            classify_mft_event_type(METransformDrainComplete.0 as u32),
            MftEventKind::DrainComplete
        );
        assert_eq!(
            classify_mft_event_type(0xFFFF_FFFE),
            MftEventKind::Other(0xFFFF_FFFE)
        );
    }
}
