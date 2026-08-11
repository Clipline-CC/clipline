//! Real H.264 encoder probing (ddoc §4) via MFTEnumEx: which hardware
//! vendors offer an H.264 encoder MFT, plus the Microsoft software H.264
//! MFT as the last-resort tier. This is the proven zero-copy path; it only
//! reports H.264 because the native hardware and software MFT wrappers only
//! implement H.264. Hardware AV1/HEVC on the same silicon is surfaced by the
//! FFmpeg probe instead.

use windows::core::GUID;
use windows::Win32::Media::MediaFoundation::{
    CMSH264EncoderMFT, IMFActivate, MFMediaType_Video, MFStartup, MFTEnumEx,
    MFT_ENUM_HARDWARE_VENDOR_ID_Attribute, MFT_TRANSFORM_CLSID_Attribute, MFVideoFormat_H264,
    MFSTARTUP_FULL, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT, MFT_REGISTER_TYPE_INFO, MF_VERSION,
};
use windows::Win32::System::Com::CoTaskMemFree;

use crate::probe::{
    retain_encodable_hardware, Codec, EncoderApi, EncoderBackend, EncoderCapability,
};

/// PCI vendor id (as MFT_ENUM_HARDWARE_VENDOR_ID reports it) → backend.
pub fn backend_for_vendor(vendor: &str) -> Option<EncoderBackend> {
    match vendor.trim_end_matches('\0').trim() {
        "VEN_10DE" => Some(EncoderBackend::Nvenc),
        "VEN_1002" => Some(EncoderBackend::Amf),
        "VEN_8086" => Some(EncoderBackend::QuickSync),
        _ => None,
    }
}

/// Make sure Media Foundation is up. Refcounted by the OS; never paired
/// with MFShutdown — the process uses MF for its whole lifetime.
pub fn ensure_mf_started() -> windows::core::Result<()> {
    // SAFETY: MFStartup is safe to call repeatedly.
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }
}

/// Enumerate activates for one codec/flag combination. The returned
/// activates are released on drop; the CoTaskMem array is freed here.
pub(crate) fn enum_activates(
    subtype: GUID,
    flags: windows::Win32::Media::MediaFoundation::MFT_ENUM_FLAG,
) -> windows::core::Result<Vec<IMFActivate>> {
    let out_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype,
    };
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: out-params receive a CoTaskMem array of COM pointers; we take
    // ownership of each element and free the array afterwards.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            None,
            Some(&out_info),
            &mut activates,
            &mut count,
        )?;
        // No matches → the out array may stay null (e.g. AV1 on pre-RDNA3).
        if count == 0 || activates.is_null() {
            return Ok(Vec::new());
        }
        let slice = std::slice::from_raw_parts_mut(activates, count as usize);
        let owned: Vec<IMFActivate> = slice.iter_mut().filter_map(|a| a.take()).collect();
        CoTaskMemFree(Some(activates as *const _));
        Ok(owned)
    }
}

fn vendor_of(activate: &IMFActivate) -> Option<String> {
    let mut buf = [0u16; 64];
    let mut len = 0u32;
    // SAFETY: fixed-size out buffer; GetString writes at most buf.len() chars.
    unsafe {
        activate
            .GetString(
                &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute,
                &mut buf,
                Some(&mut len),
            )
            .ok()?;
    }
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

pub(crate) fn backend_of(activate: &IMFActivate) -> Option<EncoderBackend> {
    vendor_of(activate).and_then(|vendor| backend_for_vendor(&vendor))
}

pub(crate) fn is_microsoft_software_h264(activate: &IMFActivate) -> bool {
    unsafe { activate.GetGUID(&MFT_TRANSFORM_CLSID_Attribute) }
        .is_ok_and(|clsid| clsid == CMSH264EncoderMFT)
}

/// MF-backed implementation of the ddoc §4 probe — H.264 only, since that is
/// all the native MFT wrappers implement.
///
/// Hardware backends are confirmed with a real probe encode before being
/// reported: a registered vendor MFT can open cleanly, accept a frame, and
/// still fail once the pump does real work (`mft::hardware_backend_can_encode`).
/// Use [`enumerate_registered`] for the raw, unvalidated view.
pub fn enumerate() -> windows::core::Result<Vec<EncoderCapability>> {
    enumerate_with_validator(crate::windows::mft::hardware_backend_can_encode)
}

/// [`enumerate`] with the hardware check injected, so tests can decide the
/// verdict instead of depending on whatever silicon they run on.
pub fn enumerate_with_validator(
    can_encode: impl FnMut(EncoderBackend) -> bool,
) -> windows::core::Result<Vec<EncoderCapability>> {
    Ok(retain_encodable_hardware(
        enumerate_registered()?,
        can_encode,
    ))
}

/// Every H.264 encoder MFT this machine *registers*, with no proof any of them
/// can encode. Callers that act on the result want [`enumerate`]; this exists
/// for diagnostics and for the validation layer above it.
pub fn enumerate_registered() -> windows::core::Result<Vec<EncoderCapability>> {
    ensure_mf_started()?;
    let mut backends: Vec<EncoderBackend> = Vec::new();
    for activate in enum_activates(
        MFVideoFormat_H264,
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
    )? {
        if let Some(backend) = backend_of(&activate) {
            if !backends.contains(&backend) {
                backends.push(backend);
            }
        }
    }
    let mut caps: Vec<EncoderCapability> = backends
        .into_iter()
        .map(|backend| EncoderCapability {
            api: EncoderApi::Mft,
            backend,
            codecs: vec![Codec::H264],
        })
        .collect();
    // Software H.264 (sync MFT — Microsoft's encoder) as the last resort.
    if enum_activates(
        MFVideoFormat_H264,
        MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
    )?
    .iter()
    .any(is_microsoft_software_h264)
    {
        caps.push(EncoderCapability {
            api: EncoderApi::Mft,
            backend: EncoderBackend::MfSoftware,
            codecs: vec![Codec::H264],
        });
    }
    Ok(caps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::EncoderBackend;

    /// Enumeration itself must work anywhere Media Foundation exists;
    /// result contents depend on hardware. Self-skips if MF is absent
    /// (Server without the Media Foundation feature).
    #[test]
    fn enumerate_returns_without_error() {
        let caps = match enumerate() {
            Ok(caps) => caps,
            Err(e) => {
                eprintln!("SKIP: Media Foundation unavailable: {e}");
                return;
            }
        };
        for c in &caps {
            assert!(!c.codecs.is_empty(), "empty-codec entries are filtered");
        }
        eprintln!("encoders found: {caps:?}");
    }

    /// The invariant this module exists to uphold: anything we advertise, the
    /// recorder will commit to. Intel's Alder Lake-N H.264 MFT registers and
    /// opens but fails its first frame, so registration alone must never reach
    /// this list. Trivially true on machines with no hardware encoder.
    #[test]
    fn advertised_hardware_encoders_can_actually_encode() {
        let caps = match enumerate() {
            Ok(caps) => caps,
            Err(e) => {
                eprintln!("SKIP: Media Foundation unavailable: {e}");
                return;
            }
        };
        for cap in caps.iter().filter(|cap| cap.backend.is_hardware()) {
            assert!(
                crate::windows::mft::hardware_backend_can_encode(cap.backend),
                "{:?} is advertised but cannot encode a frame",
                cap.backend
            );
        }
    }

    /// Validation is the only difference between the two entry points, so a
    /// rejecting validator must leave the software tier and nothing else.
    #[test]
    fn validation_drops_hardware_but_keeps_software() {
        let registered = match enumerate_registered() {
            Ok(caps) => caps,
            Err(e) => {
                eprintln!("SKIP: Media Foundation unavailable: {e}");
                return;
            }
        };
        let validated =
            enumerate_with_validator(|_| false).expect("enumeration already succeeded above");
        assert!(
            validated.iter().all(|cap| !cap.backend.is_hardware()),
            "a rejecting validator left hardware advertised: {validated:?}"
        );
        let software_registered = registered.iter().filter(|c| !c.backend.is_hardware()).count();
        assert_eq!(
            validated.len(),
            software_registered,
            "software tiers must survive validation untouched"
        );
    }

    #[test]
    fn vendor_ids_map_to_backends() {
        assert_eq!(backend_for_vendor("VEN_10DE"), Some(EncoderBackend::Nvenc));
        assert_eq!(backend_for_vendor("VEN_1002"), Some(EncoderBackend::Amf));
        assert_eq!(
            backend_for_vendor("VEN_8086"),
            Some(EncoderBackend::QuickSync)
        );
        assert_eq!(backend_for_vendor("VEN_FFFF"), None);
    }
}
