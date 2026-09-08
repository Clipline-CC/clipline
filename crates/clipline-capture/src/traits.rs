use clipline_mp4::{AudioTrackConfig, VideoTrackConfig};

/// One captured video frame. Platform implementations keep pixels on the
/// GPU (ddoc §3: frames stay as GPU textures); the pipeline only needs
/// timing plus an opaque payload handle.
#[derive(Debug, Clone)]
pub struct Frame {
    /// Seconds since capture start (monotonic, from the capture clock).
    pub pts_s: f64,
    pub data: FrameData,
}

/// Frame payload. `Cpu` serves mocks/tests/software paths; `Gpu` keeps
/// pixels on the GPU as ddoc §3 requires (no CPU round-trips).
#[derive(Debug, Clone)]
pub enum FrameData {
    Cpu(Vec<u8>),
    #[cfg(windows)]
    Gpu(::windows::Win32::Graphics::Direct3D11::ID3D11Texture2D),
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("capture source changed: {0}")]
    SourceChanged(String),
    #[error("capture init failed: {0}")]
    Init(String),
    #[error("capture device lost: {0}")]
    DeviceLost(String),
    #[error("no frame arrived within {0:?}")]
    Timeout(std::time::Duration),
    #[error("{operation} timed out after {after:?}")]
    OperationTimeout {
        operation: String,
        after: std::time::Duration,
    },
}

impl CaptureError {
    pub fn is_timeout(&self) -> bool {
        matches!(self, Self::Timeout(_) | Self::OperationTimeout { .. })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error("encoder failed: {0}")]
    Backend(String),
}

/// Pull-model capture source. `Ok(None)` means the source ended.
pub trait CaptureEngine {
    fn next_frame(&mut self) -> Result<Option<Frame>, CaptureError>;
}

/// One encoded sample out of the encoder.
#[derive(Debug, Clone)]
pub struct EncodedPacket {
    pub data: Vec<u8>,
    pub pts_s: f64,
    pub duration_s: f64,
    pub is_keyframe: bool,
}

/// Video encoder. May buffer internally (B-frames later), hence Vec out.
pub trait Encoder {
    fn encode(&mut self, frame: &Frame) -> Result<Vec<EncodedPacket>, EncodeError>;
    /// Track parameters for muxing the produced stream.
    fn track_config(&self) -> VideoTrackConfig;
    /// Drain any internally buffered packets at end of stream. Called by
    /// the pipeline after the capture source ends.
    fn finish(&mut self) -> Result<Vec<EncodedPacket>, EncodeError> {
        Ok(Vec::new())
    }
}

/// Lets the recorder hold a runtime-selected encoder (MFT or FFmpeg) behind
/// one type after walking the ranked candidate list.
impl Encoder for Box<dyn Encoder> {
    fn encode(&mut self, frame: &Frame) -> Result<Vec<EncodedPacket>, EncodeError> {
        (**self).encode(frame)
    }
    fn track_config(&self) -> VideoTrackConfig {
        (**self).track_config()
    }
    fn finish(&mut self) -> Result<Vec<EncodedPacket>, EncodeError> {
        (**self).finish()
    }
}

/// One encoded audio packet (e.g. a 20 ms Opus frame).
#[derive(Debug, Clone)]
pub struct AudioPacket {
    pub data: Vec<u8>,
    /// Seconds since capture start, same timebase as video frames.
    pub pts_s: f64,
    pub duration_s: f64,
}

/// An encoded-audio producer (ddoc §10: WASAPI loopback / per-process /
/// mic, each composed with an Opus encoder behind this trait). Drain
/// model: return every packet that ends at or before `until_pts_s`.
pub trait AudioSource {
    fn poll_packets(&mut self, until_pts_s: f64) -> Result<Vec<AudioPacket>, CaptureError>;
    /// Final opportunity to drain device/encoder latency through the video
    /// boundary. Sources without delayed delivery use ordinary polling.
    fn finish_packets(&mut self, until_pts_s: f64) -> Result<Vec<AudioPacket>, CaptureError> {
        self.poll_packets(until_pts_s)
    }
    /// Track parameters for muxing this source's stream.
    fn track_config(&self) -> AudioTrackConfig;
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_mp4::VideoTrackConfig;

    struct StubEncoder {
        call_count: u32,
    }

    impl StubEncoder {
        fn new() -> Self {
            Self { call_count: 0 }
        }
    }

    impl Encoder for StubEncoder {
        fn encode(&mut self, frame: &Frame) -> Result<Vec<EncodedPacket>, EncodeError> {
            self.call_count += 1;
            Ok(vec![EncodedPacket {
                data: frame.data.cpu_bytes().to_vec(),
                pts_s: frame.pts_s,
                duration_s: 1.0 / 30.0,
                is_keyframe: self.call_count == 1,
            }])
        }

        fn track_config(&self) -> VideoTrackConfig {
            VideoTrackConfig::h264(64, 64, 90_000, vec![0x67], vec![0x68])
        }

        fn finish(&mut self) -> Result<Vec<EncodedPacket>, EncodeError> {
            Ok(vec![EncodedPacket {
                data: b"flush".to_vec(),
                pts_s: 99.0,
                duration_s: 1.0 / 30.0,
                is_keyframe: false,
            }])
        }
    }

    impl FrameData {
        fn cpu_bytes(&self) -> &[u8] {
            match self {
                FrameData::Cpu(b) => b,
                #[cfg(windows)]
                _ => panic!("no GPU frames in test"),
            }
        }
    }

    #[test]
    fn box_dyn_encoder_delegates_encode() {
        let mut enc: Box<dyn Encoder> = Box::new(StubEncoder::new());
        let frame = Frame {
            pts_s: 0.5,
            data: FrameData::Cpu(vec![1, 2, 3]),
        };
        let packets = enc.encode(&frame).unwrap();
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].data, vec![1, 2, 3]);
        assert_eq!(packets[0].pts_s, 0.5);
        assert!(packets[0].is_keyframe);
    }

    #[test]
    fn box_dyn_encoder_delegates_track_config() {
        let enc: Box<dyn Encoder> = Box::new(StubEncoder::new());
        let cfg = enc.track_config();
        assert_eq!(cfg.width, 64);
        assert_eq!(cfg.height, 64);
        assert_eq!(cfg.timescale, 90_000);
    }

    #[test]
    fn box_dyn_encoder_delegates_finish() {
        let mut enc: Box<dyn Encoder> = Box::new(StubEncoder::new());
        let packets = enc.finish().unwrap();
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].data, b"flush");
    }

    #[test]
    fn default_finish_returns_empty() {
        struct MinimalEncoder;
        impl Encoder for MinimalEncoder {
            fn encode(&mut self, _: &Frame) -> Result<Vec<EncodedPacket>, EncodeError> {
                Ok(Vec::new())
            }
            fn track_config(&self) -> VideoTrackConfig {
                VideoTrackConfig::h264(32, 32, 90_000, Vec::new(), Vec::new())
            }
        }
        let mut enc = MinimalEncoder;
        assert!(enc.finish().unwrap().is_empty());
    }

    #[test]
    fn capture_error_display_includes_context() {
        let err = CaptureError::Init("no device".into());
        assert!(format!("{err}").contains("no device"));
        let err = CaptureError::DeviceLost("gone".into());
        assert!(format!("{err}").contains("gone"));
        let err = CaptureError::Timeout(std::time::Duration::from_millis(500));
        assert!(format!("{err}").contains("500ms"));
        assert!(err.is_timeout());
        let err = CaptureError::OperationTimeout {
            operation: "process loopback activation".into(),
            after: std::time::Duration::from_millis(1500),
        };
        assert!(err.is_timeout());
        assert!(format!("{err}").contains("process loopback activation"));
    }

    #[test]
    fn encode_error_display_includes_context() {
        let err = EncodeError::Backend("oom".into());
        assert!(format!("{err}").contains("oom"));
    }

    #[test]
    fn frame_data_cpu_variant_holds_bytes() {
        let data = FrameData::Cpu(vec![0xDE, 0xAD]);
        match data {
            FrameData::Cpu(b) => assert_eq!(b, vec![0xDE, 0xAD]),
            #[cfg(windows)]
            _ => panic!("expected Cpu"),
        }
    }

    #[test]
    fn audio_packet_fields_are_accessible() {
        let pkt = AudioPacket {
            data: vec![0x01],
            pts_s: 1.5,
            duration_s: 0.02,
        };
        assert_eq!(pkt.data, vec![0x01]);
        assert!((pkt.pts_s - 1.5).abs() < f64::EPSILON);
        assert!((pkt.duration_s - 0.02).abs() < f64::EPSILON);
    }
}
