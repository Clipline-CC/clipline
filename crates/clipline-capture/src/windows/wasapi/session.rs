use super::*;

/// Everything needed to (re-)create the capture client for one endpoint.
/// Stored on the capture so a lost device can be re-activated mid-recording.
#[derive(Debug, Clone)]
pub(crate) enum EndpointTarget {
    OutputLoopback {
        device_id: Option<String>,
    },
    Microphone {
        device_id: Option<String>,
        channels: WasapiChannelMode,
    },
}

impl EndpointTarget {
    pub(crate) fn mode(&self) -> EndpointMode {
        match self {
            Self::OutputLoopback { .. } => EndpointMode::OutputLoopback,
            Self::Microphone { channels, .. } => EndpointMode::InputCapture(*channels),
        }
    }

    pub(crate) fn activate(&self) -> Result<ActivatedDevice, CaptureError> {
        match self {
            Self::OutputLoopback { device_id } => activate_endpoint(
                eRender,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                device_id.as_deref(),
            ),
            Self::Microphone { device_id, .. } => {
                activate_endpoint(eCapture, 0, device_id.as_deref())
            }
        }
    }
}

/// A freshly activated and started WASAPI endpoint.
pub(crate) struct ActivatedDevice {
    pub(crate) client: IAudioClient,
    pub(crate) capture: IAudioCaptureClient,
    pub(crate) mix: MixFormat,
}

impl ActivatedDevice {
    pub(crate) fn stop(self) {
        // SAFETY: the client was started successfully by `initialize_client`.
        let _ = unsafe { self.client.Stop() };
    }
}

pub(crate) fn activate_endpoint(
    dataflow: EDataFlow,
    streamflags: u32,
    device_id: Option<&str>,
) -> Result<ActivatedDevice, CaptureError> {
    init_com()?;
    // SAFETY: standard MMDevice activation chain; all results checked.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|error| activation_error("enumerator activation", error))?;
        let device = endpoint_device(&enumerator, dataflow, device_id).map_err(|error| {
            CaptureError::DeviceLost(format!("WASAPI endpoint unavailable: {error}"))
        })?;
        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|error| activation_error("endpoint activation", error))?;
        initialize_client(client, streamflags, POLLING_BUFFER_DURATION_100NS)
    }
}

pub(crate) fn initialize_client(
    client: IAudioClient,
    streamflags: u32,
    buffer_duration_100ns: i64,
) -> Result<ActivatedDevice, CaptureError> {
    // SAFETY: IAudioClient initialization follows the WASAPI contract and
    // releases the mix-format allocation after Initialize consumes it.
    unsafe {
        let format = client
            .GetMixFormat()
            .map_err(|error| activation_error("GetMixFormat", error))?;
        let mut format_storage = CoTaskMemWaveFormat::new(format).ok_or_else(|| {
            CaptureError::Init("WASAPI GetMixFormat returned a null format".into())
        })?;
        let format_ptr = format_storage.as_mut_ptr();
        let format = &*format_ptr;
        // Copy packed fields to locals (references into packed structs are UB).
        let tag = format.wFormatTag;
        let ch = format.nChannels;
        let rate = format.nSamplesPerSec;
        let bits = format.wBitsPerSample;
        let Some(mix) = parse_mix_format(format) else {
            return Err(CaptureError::Init(format!(
                "unsupported mix format: tag {tag} ch {ch} rate {rate} bits {bits} \
                 (need float32 or signed PCM)"
            )));
        };
        // 1 s device buffer: poll_packets runs per video frame, this
        // gives ~60 polls of headroom.
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                streamflags,
                buffer_duration_100ns,
                0,
                format_ptr,
                None,
            )
            .map_err(|error| activation_error("Initialize", error))?;

        let capture: IAudioCaptureClient = client
            .GetService()
            .map_err(|error| activation_error("GetService", error))?;
        client
            .Start()
            .map_err(|error| activation_error("Start", error))?;

        Ok(ActivatedDevice {
            client,
            capture,
            mix,
        })
    }
}

/// WASAPI client states recoverable by re-activating the endpoint: the
/// device (or audio service) went away and a fresh client reattaches when
/// it returns. Everything else keeps the existing fatal semantics.
pub(crate) fn wasapi_error_recoverable(code: HRESULT) -> bool {
    code == AUDCLNT_E_DEVICE_INVALIDATED
        || code == AUDCLNT_E_SERVICE_NOT_RUNNING
        || code == AUDCLNT_E_RESOURCES_INVALIDATED
}

fn activation_error(operation: &str, error: windows::core::Error) -> CaptureError {
    let message = format!("WASAPI {operation}: {error}");
    if wasapi_error_recoverable(error.code()) {
        CaptureError::DeviceLost(message)
    } else {
        CaptureError::Init(message)
    }
}

struct CoTaskMemWaveFormat(*mut WAVEFORMATEX);

impl CoTaskMemWaveFormat {
    fn new(format: *mut WAVEFORMATEX) -> Option<Self> {
        (!format.is_null()).then_some(Self(format))
    }

    fn as_mut_ptr(&mut self) -> *mut WAVEFORMATEX {
        self.0
    }
}

impl Drop for CoTaskMemWaveFormat {
    fn drop(&mut self) {
        // SAFETY: this wrapper is created only from `GetMixFormat`, which
        // transfers one COM-task allocation to the caller.
        unsafe { CoTaskMemFree(Some(self.0.cast())) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG};
    use windows::Win32::Media::Audio::AUDCLNT_E_NOT_INITIALIZED;
    use windows::Win32::System::Com::CoTaskMemAlloc;

    #[test]
    fn recoverable_audclnt_errors_are_classified_by_hresult() {
        for code in [
            AUDCLNT_E_DEVICE_INVALIDATED,
            AUDCLNT_E_SERVICE_NOT_RUNNING,
            AUDCLNT_E_RESOURCES_INVALIDATED,
        ] {
            assert!(
                wasapi_error_recoverable(code),
                "{code:?} must trigger reactivation"
            );
        }
        for fatal in [HRESULT(0), E_FAIL, E_INVALIDARG, AUDCLNT_E_NOT_INITIALIZED] {
            assert!(
                !wasapi_error_recoverable(fatal),
                "{fatal:?} must stay fatal"
            );
        }
    }

    #[test]
    fn recoverable_startup_errors_create_device_loss_for_dormant_capture() {
        let error = activation_error(
            "Start",
            windows::core::Error::from_hresult(AUDCLNT_E_SERVICE_NOT_RUNNING),
        );
        assert!(matches!(error, CaptureError::DeviceLost(message) if message.contains("Start")));

        let error = activation_error(
            "Start",
            windows::core::Error::from_hresult(E_FAIL),
        );
        assert!(matches!(error, CaptureError::Init(message) if message.contains("Start")));
    }

    #[test]
    fn com_wave_format_storage_owns_its_allocation() {
        let allocation = unsafe { CoTaskMemAlloc(size_of::<WAVEFORMATEX>()) } as *mut WAVEFORMATEX;
        assert!(!allocation.is_null());
        unsafe { allocation.write(WAVEFORMATEX::default()) };
        let storage = CoTaskMemWaveFormat::new(allocation).expect("COM allocation");
        drop(storage);
    }
}
