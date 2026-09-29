use clipline_mp4::{AudioTrackConfig, VideoTrackConfig};
use crate::mock::{MockCapture, MockEncoder};
use crate::traits::{AudioPacket, AudioSource, CaptureEngine, CaptureError, EncodeError, EncodedPacket, Encoder, Frame, FrameData};
use std::io;
use std::io::{Seek, Write};
use std::sync::mpsc::{Receiver, Sender};

    pub(crate) struct NeverKeyframeEncoder {
        pub(crate) fps: u32,
    }

    pub(crate) struct PtsRemapEncoder {
        pub(crate) inner: MockEncoder,
        pub(crate) ticks: &'static [f64],
    }

    pub(crate) struct TimestampCapture {
        pub(crate) pts: std::collections::VecDeque<f64>,
    }

    pub(crate) struct VariableDurationEncoder {
        pub(crate) inner: MockEncoder,
        pub(crate) fps: u32,
        pub(crate) previous_pts_s: Option<f64>,
    }

    pub(crate) const CLOSELY_SPACED_TICKS: &[f64] = &[0.0, 900.0, 907.0, 2_700.0, 3_600.0];
    pub(crate) const REPEATED_SUB_TICK_TICKS: &[f64] = &[0.0, 900.0, 900.009, 900.018, 3_600.0];

    impl TimestampCapture {
        pub(crate) fn new(pts: impl IntoIterator<Item = f64>) -> Self {
            Self {
                pts: pts.into_iter().collect(),
            }
        }
    }

    impl CaptureEngine for TimestampCapture {
        fn next_frame(&mut self) -> Result<Option<Frame>, CaptureError> {
            Ok(self.pts.pop_front().map(|pts_s| Frame {
                pts_s,
                data: FrameData::Cpu(vec![0; 16]),
            }))
        }
    }

    impl VariableDurationEncoder {
        pub(crate) fn new(gop_len: u64, fps: u32) -> Self {
            Self {
                inner: MockEncoder::new(gop_len, fps),
                fps,
                previous_pts_s: None,
            }
        }
    }

    impl Encoder for VariableDurationEncoder {
        fn encode(&mut self, frame: &Frame) -> Result<Vec<EncodedPacket>, EncodeError> {
            let mut packets = self.inner.encode(frame)?;
            packets[0].duration_s = self
                .previous_pts_s
                .map_or(1.0 / f64::from(self.fps), |previous| frame.pts_s - previous);
            self.previous_pts_s = Some(frame.pts_s);
            Ok(packets)
        }

        fn track_config(&self) -> VideoTrackConfig {
            self.inner.track_config()
        }
    }

    impl Encoder for PtsRemapEncoder {
        fn encode(&mut self, frame: &Frame) -> Result<Vec<EncodedPacket>, EncodeError> {
            let mut packets = self.inner.encode(frame)?;
            for packet in &mut packets {
                let index = (packet.pts_s * 30.0).round() as u64;
                let ticks = self.ticks.get(index as usize).copied().unwrap_or_else(|| {
                    let last_index = self.ticks.len() - 1;
                    self.ticks[last_index] + (index as usize - last_index) as f64 * 3_000.0
                });
                packet.pts_s = ticks / 90_000.0;
            }
            Ok(packets)
        }

        fn track_config(&self) -> VideoTrackConfig {
            self.inner.track_config()
        }
    }

    impl NeverKeyframeEncoder {
        pub(crate) fn new(fps: u32) -> Self {
            Self { fps }
        }
    }

    impl Encoder for NeverKeyframeEncoder {
        fn encode(&mut self, frame: &Frame) -> Result<Vec<EncodedPacket>, EncodeError> {
            Ok(vec![EncodedPacket {
                data: vec![0xEE; 128],
                pts_s: frame.pts_s,
                duration_s: 1.0 / self.fps as f64,
                is_keyframe: false,
            }])
        }

        fn track_config(&self) -> VideoTrackConfig {
            VideoTrackConfig::h264(
                128,
                128,
                90_000,
                vec![0x67, 0x64, 0x00, 0x0A, 0xAC],
                vec![0x68, 0xEE, 0x38, 0x80],
            )
        }
    }

    pub(crate) struct GappedAudioSource {
        pub(crate) packets: std::collections::VecDeque<AudioPacket>,
    }

    impl GappedAudioSource {
        pub(crate) fn new() -> Self {
            let mut packets = std::collections::VecDeque::new();
            for start in [1.2_f64, 3.2] {
                for index in 0..38 {
                    packets.push_back(AudioPacket {
                        data: vec![index as u8; 24],
                        pts_s: start + f64::from(index) * 0.02,
                        duration_s: 0.02,
                    });
                }
            }
            Self { packets }
        }
    }

    impl AudioSource for GappedAudioSource {
        fn poll_packets(&mut self, until_pts_s: f64) -> Result<Vec<AudioPacket>, CaptureError> {
            let mut output = Vec::new();
            while self
                .packets
                .front()
                .is_some_and(|packet| packet.pts_s + packet.duration_s <= until_pts_s + 1e-9)
            {
                output.push(self.packets.pop_front().expect("front packet exists"));
            }
            Ok(output)
        }

        fn track_config(&self) -> clipline_mp4::AudioTrackConfig {
            clipline_mp4::AudioTrackConfig {
                channels: 2,
                sample_rate: 48_000,
                pre_skip: 312,
            }
        }
    }

    pub(crate) fn edit_list_entries(bytes: &[u8]) -> Vec<(u32, i32)> {
        let fourcc = bytes
            .windows(4)
            .position(|window| window == b"elst")
            .expect("audio gap edit list");
        let payload = fourcc + 4;
        assert_eq!(bytes[payload], 0);
        let count = u32::from_be_bytes(bytes[payload + 4..payload + 8].try_into().unwrap());
        let mut entries = Vec::new();
        let mut pos = payload + 8;
        for _ in 0..count {
            entries.push((
                u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap()),
                i32::from_be_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()),
            ));
            pos += 12;
        }
        entries
    }

    pub(crate) fn first_opus_pre_skip(buf: &[u8]) -> u16 {
        let fourcc = buf
            .windows(4)
            .position(|window| window == b"dOps")
            .expect("dOps box");
        let p = fourcc + 4;
        u16::from_be_bytes(buf[p + 2..p + 4].try_into().expect("pre-skip bytes"))
    }

    pub(crate) struct GatedWriter {
        pub(crate) inner: std::io::Cursor<Vec<u8>>,
        pub(crate) entered: Option<Sender<()>>,
        pub(crate) release: Receiver<()>,
    }

    impl Write for GatedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if let Some(entered) = self.entered.take() {
                entered
                    .send(())
                    .map_err(|_| io::Error::other("gate observer stopped"))?;
                self.release
                    .recv()
                    .map_err(|_| io::Error::other("gate released by disconnect"))?;
            }
            self.inner.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.inner.flush()
        }
    }

    impl Seek for GatedWriter {
        fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    pub(crate) struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("disk full"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Seek for FailingWriter {
        fn seek(&mut self, _pos: io::SeekFrom) -> io::Result<u64> {
            Ok(0)
        }
    }

    pub(crate) const DELAYED_SPS: &[u8] = &[0x67, 0x64, 0x00, 0x0A, 0xAC];

    pub(crate) struct DelayedTrackConfig {
        pub(crate) inner: MockEncoder,
        pub(crate) encoded_any: bool,
    }

    impl Encoder for DelayedTrackConfig {
        fn encode(
            &mut self,
            frame: &crate::traits::Frame,
        ) -> Result<Vec<crate::traits::EncodedPacket>, crate::traits::EncodeError> {
            let packets = self.inner.encode(frame)?;
            self.encoded_any = true;
            Ok(packets)
        }

        fn track_config(&self) -> clipline_mp4::VideoTrackConfig {
            if self.encoded_any {
                return self.inner.track_config();
            }
            clipline_mp4::VideoTrackConfig::h264(128, 128, 90_000, Vec::new(), Vec::new())
        }
    }

    /// Wraps MockEncoder but holds back the latest packet until finish() —
    /// models real encoders' internal buffering.
    pub(crate) struct OneFrameLatency {
        pub(crate) inner: MockEncoder,
        pub(crate) held: Option<crate::traits::EncodedPacket>,
    }

    impl Encoder for OneFrameLatency {
        fn encode(
            &mut self,
            frame: &crate::traits::Frame,
        ) -> Result<Vec<crate::traits::EncodedPacket>, crate::traits::EncodeError> {
            let mut out = self.inner.encode(frame)?;
            let newly = out.pop();
            let released = self.held.take();
            self.held = newly;
            Ok(released.into_iter().collect())
        }

        fn track_config(&self) -> clipline_mp4::VideoTrackConfig {
            self.inner.track_config()
        }

        fn finish(
            &mut self,
        ) -> Result<Vec<crate::traits::EncodedPacket>, crate::traits::EncodeError> {
            Ok(self.held.take().into_iter().collect())
        }
    }

    /// Encoder echoing nominal durations while pts jitters (VRR-style):
    /// the sealed timeline must follow the STAMPS (ddoc §6), not the echo.
    pub(crate) struct JitteryEncoder {
        pub(crate) inner: MockEncoder,
    }

    impl Encoder for JitteryEncoder {
        fn encode(
            &mut self,
            frame: &crate::traits::Frame,
        ) -> Result<Vec<crate::traits::EncodedPacket>, crate::traits::EncodeError> {
            let mut pkts = self.inner.encode(frame)?;
            for p in &mut pkts {
                // Stamps: frames alternate 10 ms / 30 ms apart, while the
                // encoder still claims a flat 1/30 s duration.
                let idx = (p.pts_s * 30.0).round();
                p.pts_s = (idx / 2.0).floor() * 0.04 + if idx % 2.0 == 1.0 { 0.01 } else { 0.0 };
            }
            Ok(pkts)
        }
        fn track_config(&self) -> clipline_mp4::VideoTrackConfig {
            self.inner.track_config()
        }
    }

    pub(crate) struct FinishOnlyAudioSource {
        pub(crate) finished: bool,
    }

    impl AudioSource for FinishOnlyAudioSource {
        fn poll_packets(&mut self, _until_pts_s: f64) -> Result<Vec<AudioPacket>, CaptureError> {
            Ok(Vec::new())
        }

        fn finish_packets(&mut self, until_pts_s: f64) -> Result<Vec<AudioPacket>, CaptureError> {
            if self.finished || until_pts_s + 1e-9 < 0.98 {
                return Ok(Vec::new());
            }
            self.finished = true;
            Ok(vec![AudioPacket {
                data: vec![0xAB; 24],
                pts_s: 0.96,
                duration_s: 0.02,
            }])
        }

        fn track_config(&self) -> AudioTrackConfig {
            AudioTrackConfig {
                channels: 2,
                sample_rate: 48_000,
                pre_skip: 312,
            }
        }
    }

    /// Shifts a capture source's pts later — models the real lead-in
    /// between clock creation and the first WGC frame.
    pub(crate) struct OffsetCapture {
        pub(crate) inner: MockCapture,
        pub(crate) offset_s: f64,
    }

    impl crate::traits::CaptureEngine for OffsetCapture {
        fn next_frame(
            &mut self,
        ) -> Result<Option<crate::traits::Frame>, crate::traits::CaptureError> {
            Ok(self.inner.next_frame()?.map(|mut f| {
                f.pts_s += self.offset_s;
                f
            }))
        }
    }
