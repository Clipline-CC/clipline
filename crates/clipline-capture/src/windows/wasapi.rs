//! System-audio capture: WASAPI loopback on the default render endpoint
//! (ddoc §10), QPC-stamped against the shared capture clock, assembled
//! into 20 ms frames and Opus-encoded behind `AudioSource`.

use std::mem::size_of;
use std::time::{Duration, Instant};

use windows::core::{HRESULT, PCWSTR, PWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, EDataFlow, IAudioCaptureClient, IAudioClient, IMMDevice,
    IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR,
    AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_E_RESOURCES_INVALIDATED,
    AUDCLNT_E_SERVICE_NOT_RUNNING, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
    DEVICE_STATE_ACTIVE, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVE_FORMAT_PCM,
};
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToString};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
};

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

pub use types::{AudioDeviceInfo, AudioDeviceList, AudioLevel, WasapiChannelMode, WasapiMonitorChunk};
pub use capture::{WasapiLoopback};
pub use devices::{enumerate_audio_devices, windows_build_number};
pub(crate) use types::{OPUS_SAMPLE_RATE, POLLING_BUFFER_DURATION_100NS, AUDIO_DELIVERY_HEADROOM_S, TERMINAL_AUDIO_DRAIN_S, DEVICE_REACTIVATION_RETRY_INTERVAL, EndpointMode, ActivationPhase, init};
pub(crate) use session::{EndpointTarget, ActivatedDevice, wasapi_error_recoverable};
pub(crate) use format::{wasapi_timestamp_valid, wasapi_data_discontinuous, SampleFormat, MixFormat, AudioLevelAccumulator, audio_poll_silence_horizon, parse_mix_format, decode_sample_bytes};
pub(crate) use devices::{endpoint_device, device_id_string, init_com};
