//! System-audio capture: WASAPI loopback on the default render endpoint
//! (ddoc §10), QPC-stamped against the shared capture clock, assembled
//! into 20 ms frames and Opus-encoded behind `AudioSource`.

use std::mem::{size_of, ManuallyDrop};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use windows::core::{implement, Interface, Ref, Result as WindowsResult, HRESULT, PCWSTR, PWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, FILETIME, RPC_E_CHANGED_MODE};
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, ActivateAudioInterfaceAsync, AudioSessionStateExpired, EDataFlow,
    IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
    IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient, IAudioClient,
    IAudioSessionControl2, IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_E_DEVICE_INVALIDATED,
    AUDCLNT_E_RESOURCES_INVALIDATED, AUDCLNT_E_SERVICE_NOT_RUNNING, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    DEVICE_STATE_ACTIVE, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
    VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVE_FORMAT_PCM,
};
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::StructuredStorage::{
    PropVariantClear, PropVariantToString, PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0,
    PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemAlloc, CoTaskMemFree, IAgileObject,
    IAgileObject_Impl, BLOB, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::Variant::VT_BLOB;

use clipline_mp4::AudioTrackConfig;

use crate::clock::RelativeClock;
use crate::diagnostics::{emit_diagnostic, CaptureDiagnostic, DiagnosticRateLimiter};
use crate::opus::{OpusFrameEncoder, FRAME_DURATION_S};
use crate::pcm::{
    apply_gain, extract_mono_centered, extract_stereo, DevicePacketPlacement, DevicePacketTimeline,
    DeviceReactivation, DiscontinuityFade, LoopbackAssembler, PcmFrame, StereoResampler,
};
use crate::traits::{AudioPacket, AudioSource, CaptureError};

mod types;
mod session;
mod format;
mod capture;
mod devices;
mod processes;

pub use types::{AudioDeviceInfo, AudioDeviceList, AudioProcessInfo, AudioLevel, WasapiMonitorChunk, WasapiChannelMode};
pub use capture::{WasapiLoopback};
pub use devices::{enumerate_audio_devices, process_loopback_available, windows_build_number};
pub use processes::{enumerate_output_processes};
pub(crate) use types::{OPUS_SAMPLE_RATE, POLLING_BUFFER_DURATION_100NS, PROCESS_LOOPBACK_ACTIVATION_TIMEOUT, AUDIO_DELIVERY_HEADROOM_S, TERMINAL_AUDIO_DRAIN_S, DEVICE_REACTIVATION_RETRY_INTERVAL, EndpointMode, ActivationPhase, ProcessIdentity, ProcessSnapshotEntry, init};
pub(crate) use session::{EndpointTarget, ActivatedDevice, wasapi_error_recoverable};
pub(crate) use format::{wasapi_timestamp_valid, wasapi_data_discontinuous, SampleFormat, MixFormat, AudioLevelAccumulator, audio_poll_silence_horizon, parse_mix_format, decode_sample_bytes, process_loopback_format};
pub(crate) use devices::{endpoint_device, device_id_string, pwstr_to_optional_string_and_free, utf16z_from_buf, init_com};
pub(crate) use processes::{process_loopback_stream_config, activate_process_loopback_client, process_identity};
