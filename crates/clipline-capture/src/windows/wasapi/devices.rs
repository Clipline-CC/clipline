use super::*;

pub fn enumerate_audio_devices() -> Result<AudioDeviceList, CaptureError> {
    init_com()?;
    // SAFETY: standard MMDevice enumeration; all COM results are checked.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(init)?;
        Ok(AudioDeviceList {
            outputs: enumerate_endpoints(&enumerator, eRender)?,
            inputs: enumerate_endpoints(&enumerator, eCapture)?,
        })
    }
}

pub fn process_loopback_available() -> bool {
    // Per-process application loopback (ActivateAudioInterfaceAsync with
    // AUDIOCLIENT_PROCESS_LOOPBACK) is *documented* as Windows 10 build 20348+,
    // but in practice works on fully updated Windows 10 2004+ (build 19041):
    // OBS's Application Audio Capture relies on exactly this API there, and we
    // deliberately target it too (see ddoc.md). Below 2004 the activation fails
    // or its completion callback never fires — but `activate_process_loopback_client`
    // caps the wait at 1.5s and `add_output_audio_sources` falls back to
    // full-system mixed output, so attempting it on an unsupported build costs at
    // most one bounded stall. This gate only skips that pointless attempt on
    // pre-2004 builds; do not raise it to 20348 without revisiting that tradeoff.
    const MIN_PROCESS_LOOPBACK_BUILD: u32 = 19_041;
    windows_build_number().is_some_and(|build| build >= MIN_PROCESS_LOOPBACK_BUILD)
}

/// The OS build number via `RtlGetVersion` (the manifest-independent source of
/// truth). `None` if the query somehow fails.
pub fn windows_build_number() -> Option<u32> {
    use windows::Wdk::System::SystemServices::RtlGetVersion;
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    // SAFETY: RtlGetVersion fills the OSVERSIONINFOW we own; its size is set and
    // the call returns STATUS_SUCCESS on all supported systems.
    let status = unsafe { RtlGetVersion(&mut info) };
    status.is_ok().then_some(info.dwBuildNumber)
}

pub(crate) fn endpoint_device(
    enumerator: &IMMDeviceEnumerator,
    dataflow: EDataFlow,
    device_id: Option<&str>,
    allow_selected_device_fallback: bool,
) -> windows::core::Result<IMMDevice> {
    // SAFETY: the optional PCWSTR is null-terminated for the duration of GetDevice.
    unsafe {
        if let Some(id) = device_id.filter(|id| !id.trim().is_empty()) {
            let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
            let selected = enumerator.GetDevice(PCWSTR(wide.as_ptr()));
            if allow_selected_device_fallback {
                selected.or_else(|_| enumerator.GetDefaultAudioEndpoint(dataflow, eConsole))
            } else {
                selected
            }
        } else {
            enumerator.GetDefaultAudioEndpoint(dataflow, eConsole)
        }
    }
}

fn enumerate_endpoints(
    enumerator: &IMMDeviceEnumerator,
    dataflow: EDataFlow,
) -> Result<Vec<AudioDeviceInfo>, CaptureError> {
    // SAFETY: collection count and indexed access are checked by the COM methods.
    unsafe {
        let default_id = enumerator
            .GetDefaultAudioEndpoint(dataflow, eConsole)
            .ok()
            .and_then(|device| device_id_string(&device).ok());
        let collection = enumerator
            .EnumAudioEndpoints(dataflow, DEVICE_STATE_ACTIVE)
            .map_err(init)?;
        let mut devices = Vec::new();
        for i in 0..collection.GetCount().map_err(init)? {
            let device = collection.Item(i).map_err(init)?;
            let id = device_id_string(&device)?;
            let name = friendly_name(&device).unwrap_or_else(|| id.clone());
            devices.push(AudioDeviceInfo {
                is_default: default_id.as_deref() == Some(id.as_str()),
                id,
                name,
            });
        }
        Ok(devices)
    }
}

pub(crate) fn device_id_string(device: &IMMDevice) -> Result<String, CaptureError> {
    // SAFETY: IMMDevice::GetId returns a CoTaskMem-allocated null-terminated string.
    unsafe {
        let raw = device.GetId().map_err(init)?;
        pwstr_to_string_and_free(raw)
            .map_err(|e| CaptureError::Init(format!("device id utf16: {e}")))
    }
}

fn friendly_name(device: &IMMDevice) -> Option<String> {
    // SAFETY: property store and PROPVARIANT lifecycle follow the Windows API contract.
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ).ok()?;
        let mut prop = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
        let mut buf = [0u16; 256];
        let result = PropVariantToString(&prop, &mut buf)
            .ok()
            .map(|_| utf16z_from_buf(&buf))
            .filter(|s| !s.trim().is_empty());
        let _ = PropVariantClear(&mut prop);
        result
    }
}

pub(crate) fn pwstr_to_string_and_free(raw: PWSTR) -> Result<String, std::string::FromUtf16Error> {
    // SAFETY: callers pass PWSTRs returned by Windows APIs and release them with CoTaskMemFree.
    let value = unsafe { raw.to_string() };
    unsafe { CoTaskMemFree(Some(raw.0 as *const _)) };
    value
}

pub(crate) fn pwstr_to_optional_string_and_free(
    raw: PWSTR,
) -> Result<Option<String>, std::string::FromUtf16Error> {
    if raw.0.is_null() {
        return Ok(None);
    }
    pwstr_to_string_and_free(raw).map(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

pub(crate) fn utf16z_from_buf(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Best-effort COM init (MTA); an STA thread is fine too.
pub(crate) fn init_com() -> Result<(), CaptureError> {
    // SAFETY: CoInitializeEx is safe to call repeatedly per thread.
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr.is_ok() || hr == RPC_E_CHANGED_MODE {
        Ok(())
    } else {
        Err(CaptureError::Init(format!("CoInitializeEx: {hr}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumerates_audio_endpoints_when_available() {
        if std::env::var_os("CI").is_some() {
            eprintln!("SKIP: audio endpoint test");
            return;
        }
        let devices = match enumerate_audio_devices() {
            Ok(devices) => devices,
            Err(e) => {
                eprintln!("SKIP: audio endpoint enumeration unavailable: {e}");
                return;
            }
        };
        for device in devices.outputs.iter().chain(devices.inputs.iter()) {
            assert!(!device.id.is_empty());
            assert!(!device.name.is_empty());
        }
    }
}
