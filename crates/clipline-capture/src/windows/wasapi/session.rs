use super::*;

/// Everything needed to (re-)create the capture client for one endpoint.
/// Stored on the capture so a lost device can be re-activated mid-recording.
#[derive(Debug, Clone)]
pub(crate) enum EndpointTarget {
    OutputLoopback {
        device_id: Option<String>,
    },
    ProcessOutput {
        pid: u32,
        identity: ProcessIdentity,
    },
    Microphone {
        device_id: Option<String>,
        channels: WasapiChannelMode,
    },
}

impl EndpointTarget {
    pub(crate) fn mode(&self) -> EndpointMode {
        match self {
            Self::OutputLoopback { .. } | Self::ProcessOutput { .. } => {
                EndpointMode::OutputLoopback
            }
            Self::Microphone { channels, .. } => EndpointMode::InputCapture(*channels),
        }
    }

    pub(crate) fn activate(&self, phase: ActivationPhase) -> Result<ActivatedDevice, CaptureError> {
        match self {
            Self::OutputLoopback { device_id } => activate_endpoint(
                eRender,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                device_id.as_deref(),
                selected_endpoint_fallback_allowed(device_id.as_deref(), phase),
            ),
            Self::Microphone { device_id, .. } => activate_endpoint(
                eCapture,
                0,
                device_id.as_deref(),
                selected_endpoint_fallback_allowed(device_id.as_deref(), phase),
            ),
            Self::ProcessOutput { pid, .. } => {
                init_com()?;
                let client = activate_process_loopback_client(*pid)?;
                let (streamflags, buffer_duration_100ns) = process_loopback_stream_config();
                initialize_client(
                    client,
                    streamflags,
                    buffer_duration_100ns,
                    Some(process_loopback_format()),
                )
            }
        }
    }

    pub(crate) fn record_initial_endpoint(&mut self, endpoint_id: Option<&str>) {
        let selected_id = match self {
            Self::OutputLoopback { device_id } | Self::Microphone { device_id, .. } => device_id,
            Self::ProcessOutput { .. } => return,
        };
        if selected_id
            .as_deref()
            .is_some_and(|id| !id.trim().is_empty())
        {
            *selected_id = endpoint_id.map(str::to_owned);
        }
    }

    pub(crate) fn process_identity_matches(&self) -> bool {
        match self {
            Self::ProcessOutput { pid, identity } => identity.matches(*pid),
            Self::OutputLoopback { .. } | Self::Microphone { .. } => true,
        }
    }
}

/// A freshly activated and started WASAPI endpoint.
pub(crate) struct ActivatedDevice {
    pub(crate) client: IAudioClient,
    pub(crate) capture: IAudioCaptureClient,
    pub(crate) mix: MixFormat,
    pub(crate) endpoint_id: Option<String>,
}

impl ActivatedDevice {
    pub(crate) fn stop(self) {
        // SAFETY: the client was successfully started by `initialize_client`;
        // this rejected activation is never installed on a capture owner.
        let _ = unsafe { self.client.Stop() };
    }
}

pub(crate) fn activate_endpoint(
    dataflow: EDataFlow,
    streamflags: u32,
    device_id: Option<&str>,
    allow_selected_device_fallback: bool,
) -> Result<ActivatedDevice, CaptureError> {
    init_com()?;
    // SAFETY: standard MMDevice activation chain; all results checked.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(init)?;
        let device = endpoint_device(
            &enumerator,
            dataflow,
            device_id,
            allow_selected_device_fallback,
        )
        .map_err(init)?;
        let endpoint_id = device_id_string(&device)?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(init)?;
        let mut activated =
            initialize_client(client, streamflags, POLLING_BUFFER_DURATION_100NS, None)?;
        activated.endpoint_id = Some(endpoint_id);
        Ok(activated)
    }
}

pub(crate) fn initialize_client(
    client: IAudioClient,
    streamflags: u32,
    buffer_duration_100ns: i64,
    fixed_mix_format: Option<WAVEFORMATEX>,
) -> Result<ActivatedDevice, CaptureError> {
    // SAFETY: IAudioClient initialization follows the WASAPI contract and
    // releases the mix-format allocation after Initialize consumes it.
    unsafe {
        let mut fixed_mix_format = fixed_mix_format;
        let mut format_storage = if let Some(format) = fixed_mix_format.as_mut() {
            WaveFormatStorage::borrowed(format)
        } else {
            let format = client.GetMixFormat().map_err(init)?;
            WaveFormatStorage::co_task_mem(format).ok_or_else(|| {
                CaptureError::Init("WASAPI GetMixFormat returned a null format".into())
            })?
        };
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
            .map_err(|e| CaptureError::Init(format!("WASAPI Initialize: {e}")))?;

        let capture: IAudioCaptureClient = client
            .GetService()
            .map_err(|e| CaptureError::Init(format!("WASAPI GetService: {e}")))?;
        client
            .Start()
            .map_err(|e| CaptureError::Init(format!("WASAPI Start: {e}")))?;

        Ok(ActivatedDevice {
            client,
            capture,
            mix,
            endpoint_id: None,
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

enum WaveFormatStorage<'a> {
    Borrowed(&'a mut WAVEFORMATEX),
    CoTaskMem(*mut WAVEFORMATEX),
}

impl<'a> WaveFormatStorage<'a> {
    fn borrowed(format: &'a mut WAVEFORMATEX) -> Self {
        Self::Borrowed(format)
    }

    fn co_task_mem(format: *mut WAVEFORMATEX) -> Option<Self> {
        (!format.is_null()).then_some(Self::CoTaskMem(format))
    }

    fn as_mut_ptr(&mut self) -> *mut WAVEFORMATEX {
        match self {
            Self::Borrowed(format) => *format as *mut WAVEFORMATEX,
            Self::CoTaskMem(format) => *format,
        }
    }

    #[cfg(test)]
    fn owns_allocation(&self) -> bool {
        matches!(self, Self::CoTaskMem(_))
    }
}

impl Drop for WaveFormatStorage<'_> {
    fn drop(&mut self) {
        if let Self::CoTaskMem(format) = self {
            // SAFETY: this variant is created only from `GetMixFormat`, which
            // transfers one COM-task allocation to the caller.
            unsafe { CoTaskMemFree(Some((*format).cast())) };
        }
    }
}

fn selected_endpoint_fallback_allowed(device_id: Option<&str>, phase: ActivationPhase) -> bool {
    device_id.is_some_and(|id| !id.trim().is_empty()) && phase == ActivationPhase::Initial
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG};
    use windows::Win32::Media::Audio::AUDCLNT_E_NOT_INITIALIZED;

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
    fn selected_endpoint_fallback_is_startup_only() {
        assert!(selected_endpoint_fallback_allowed(
            Some("selected-device"),
            ActivationPhase::Initial
        ));
        assert!(!selected_endpoint_fallback_allowed(
            Some("selected-device"),
            ActivationPhase::Recovery
        ));
        assert!(!selected_endpoint_fallback_allowed(
            None,
            ActivationPhase::Initial
        ));
    }

    #[test]
    fn startup_fallback_tracks_the_endpoint_that_actually_activated() {
        let mut target = EndpointTarget::OutputLoopback {
            device_id: Some("stale-selection".into()),
        };

        target.record_initial_endpoint(Some("actual-default"));

        assert!(matches!(
            target,
            EndpointTarget::OutputLoopback {
                device_id: Some(ref id)
            } if id == "actual-default"
        ));
    }

    #[test]
    fn fixed_wave_format_storage_is_borrowed() {
        let mut format = process_loopback_format();
        let storage = WaveFormatStorage::borrowed(&mut format);

        assert!(!storage.owns_allocation());
    }

    #[test]
    fn com_wave_format_storage_owns_its_allocation() {
        let allocation = unsafe { CoTaskMemAlloc(size_of::<WAVEFORMATEX>()) } as *mut WAVEFORMATEX;
        assert!(!allocation.is_null());
        unsafe { allocation.write(process_loopback_format()) };
        let storage = WaveFormatStorage::co_task_mem(allocation).expect("COM allocation");

        assert!(storage.owns_allocation());
        drop(storage);
    }
}
