//! Hardware H.264 encoder via an async Media Foundation transform
//! (handoff milestone 2). Event-driven NeedInput/HaveOutput pump wrapped
//! behind the synchronous `Encoder` pull contract; D3D-aware input (NV12
//! textures straight from the video processor); Annex B output converted
//! to AVCC for clipline-mp4.

use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize, ICodecAPI, IMFActivate,
    IMFDXGIDeviceManager, IMFMediaEventGenerator, IMFSample, IMFTransform, MEError,
    METransformDrainComplete, METransformHaveOutput, METransformNeedInput,
    MFCreateAlignedMemoryBuffer, MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer,
    MFCreateMediaType, MFCreateSample, MFMediaType_Video, MFNominalRange_16_235,
    MFSampleExtension_CleanPoint, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, MFVideoPrimaries_BT709, MFVideoTransFunc_709,
    MFVideoTransferMatrix_BT709, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_ENUM_FLAG_SYNCMFT, MFT_INPUT_STREAM_INFO, MFT_MESSAGE_COMMAND_DRAIN,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_INFO,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MF_EVENT_FLAG_NO_WAIT, MF_E_NOTACCEPTING,
    MF_E_NO_EVENTS_AVAILABLE, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
    MF_LOW_LATENCY, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_SUBTYPE,
    MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX,
    MF_TRANSFORM_ASYNC_UNLOCK,
};

use clipline_mp4::VideoTrackConfig;

use crate::annexb::{annexb_to_avcc, extract_sps_pps};
use crate::cpu_video::{CpuCropRect, CpuVideoConverter};
use crate::probe::EncoderBackend;
use crate::traits::{EncodeError, EncodedPacket, Encoder, Frame, FrameData};
use crate::windows::mft_probe;
use crate::windows::nv12::{CropRect, VideoConverter};

mod common;
mod hardware;
mod software;

pub use common::{MftConfig};
pub use hardware::{MftH264Encoder};
pub use software::{SoftwareMftH264Encoder};
pub(crate) use common::{H264_PROFILE_HIGH, MFT_EVENT_TIMEOUT, MFT_EVENT_POLL_INTERVAL, OwnedMftOutputBuffer, ReusableMftOutputSample, OwnedMftActivation, backend, MftEventKind, classify_mft_event_type, mft_event_error, mft_unexpected_event_error, mft_event_timeout_error, h264_activate, RATE_CONTROL_MODE_CBR, set_rec709_limited_attrs, sequence_header_sps_pps, mf_alignment_mask, variant_u32};
pub(crate) use hardware::{hardware_backend_can_encode};
