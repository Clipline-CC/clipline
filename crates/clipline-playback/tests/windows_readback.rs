#![cfg(windows)]

use std::fs;
use std::thread;
use std::time::{Duration, Instant};

use clipline_playback::windows::{
    convert_nv12_to_rgb8, session_channel, D3D11VideoSurface, Nv12FrameView, Nv12ReadbackError,
    SessionExit, SessionUpdatePayload, WindowsNv12Readback,
};
use clipline_playback::{
    DecodedVideoFrame, FramePublisher, PipelineToken, PlaybackCommand, PlaybackEvent,
    PublicationReceipt,
};

#[test]
fn limited_range_rec709_vectors_convert_to_rgb() {
    let black = [16, 16, 16, 16, 128, 128];
    let mut rgb = [255; 12];
    convert_nv12_to_rgb8(Nv12FrameView::new(&black, 2, 2, 2, 2).unwrap(), &mut rgb).unwrap();
    assert_eq!(rgb, [0; 12]);

    let white = [235, 235, 235, 235, 128, 128];
    convert_nv12_to_rgb8(Nv12FrameView::new(&white, 2, 2, 2, 2).unwrap(), &mut rgb).unwrap();
    assert_eq!(rgb, [255; 12]);

    let red = [63, 63, 63, 63, 102, 240];
    convert_nv12_to_rgb8(Nv12FrameView::new(&red, 2, 2, 2, 2).unwrap(), &mut rgb).unwrap();
    for pixel in rgb.chunks_exact(3) {
        assert!(pixel[0] >= 250, "red channel was {pixel:?}");
        assert!(pixel[1] <= 3, "green channel was {pixel:?}");
        assert!(pixel[2] <= 3, "blue channel was {pixel:?}");
    }
}

#[test]
fn padded_rows_crop_to_visible_dimensions_without_touching_padding() {
    let frame = [
        16, 235, 99, 99, // Y row 0
        235, 16, 99, 99, // Y row 1
        128, 128, 77, 77, // UV row
    ];
    let mut rgb = [0; 12];
    convert_nv12_to_rgb8(Nv12FrameView::new(&frame, 2, 2, 4, 4).unwrap(), &mut rgb).unwrap();
    assert_eq!(rgb, [0, 0, 0, 255, 255, 255, 255, 255, 255, 0, 0, 0]);
}

#[test]
fn malformed_dimensions_strides_and_buffers_fail_closed() {
    assert!(matches!(
        Nv12FrameView::new(&[0; 8], 3, 2, 4, 4),
        Err(Nv12ReadbackError::InvalidDimensions { .. })
    ));
    assert!(matches!(
        Nv12FrameView::new(&[0; 8], 4, 2, 2, 4),
        Err(Nv12ReadbackError::InvalidStride { .. })
    ));
    assert!(matches!(
        Nv12FrameView::new(&[0; 5], 2, 2, 2, 2),
        Err(Nv12ReadbackError::InputTooShort { .. })
    ));

    let frame = [16, 16, 16, 16, 128, 128];
    assert_eq!(
        convert_nv12_to_rgb8(
            Nv12FrameView::new(&frame, 2, 2, 2, 2).unwrap(),
            &mut [0; 11],
        ),
        Err(Nv12ReadbackError::OutputSize {
            actual: 11,
            required: 12,
        })
    );
}

#[derive(Debug, Default)]
struct ReadbackPublisher {
    readback: WindowsNv12Readback,
    rgb: Vec<u8>,
    allocations: u64,
    checksum: u64,
}

impl FramePublisher<D3D11VideoSurface> for ReadbackPublisher {
    fn publish(
        &mut self,
        frame: DecodedVideoFrame<D3D11VideoSurface>,
    ) -> Result<PublicationReceipt, clipline_playback::BackendError> {
        let format = self.readback.configure(frame.surface())?;
        if self.rgb.len() != format.rgb_bytes {
            self.rgb = vec![0; format.rgb_bytes];
            self.allocations = self.allocations.saturating_add(1);
        }
        self.readback.read_rgb8(frame.surface(), &mut self.rgb)?;
        self.checksum = self.rgb.iter().fold(self.checksum, |sum, byte| {
            sum.wrapping_add(u64::from(*byte))
        });
        drop(frame);
        Ok(PublicationReceipt::Presented)
    }

    fn clear(&mut self, _token: PipelineToken) -> Result<(), clipline_playback::BackendError> {
        Ok(())
    }
}

#[test]
fn live_decoder_reads_into_one_reused_rgb_buffer_and_releases_media() {
    if std::env::var_os("CI").is_some() {
        eprintln!("SKIP: Windows diagnostic readback device test is disabled under CI");
        return;
    }

    let source = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/playback/hybrid-writer-h264-two-opus-5s.mp4");
    let directory = std::env::temp_dir().join(format!(
        "clipline-readback-test-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    fs::create_dir_all(&directory).unwrap();
    let active = directory.join("active.mp4");
    let released = directory.join("released.mp4");
    fs::copy(source, &active).unwrap();

    let (client, runtime) = session_channel();
    let playback = thread::Builder::new()
        .name("clipline-readback-test".into())
        .spawn(move || runtime.run(ReadbackPublisher::default()))
        .unwrap();
    client
        .try_send(PlaybackCommand::Open {
            path: active.clone(),
        })
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut opened = false;
    let mut unavailable = None;
    while Instant::now() < deadline && !opened && unavailable.is_none() {
        while let Some(update) = client.try_recv_update() {
            match update.payload {
                SessionUpdatePayload::Event(PlaybackEvent::Opened { .. }) => opened = true,
                SessionUpdatePayload::Event(PlaybackEvent::Error { message, .. }) => {
                    unavailable = Some(message);
                }
                _ => {}
            }
        }
        thread::sleep(Duration::from_millis(2));
    }
    if let Some(message) = unavailable {
        client.try_send(PlaybackCommand::Close).unwrap();
        let report = playback.join().unwrap().unwrap();
        assert_eq!(report.exit, SessionExit::Closed);
        fs::remove_file(&active).unwrap();
        fs::remove_dir(&directory).unwrap();
        eprintln!("SKIP: diagnostic readback devices are unavailable: {message}");
        return;
    }
    assert!(opened, "diagnostic readback session did not open");

    client.try_send(PlaybackCommand::Play).unwrap();
    thread::sleep(Duration::from_millis(250));
    client.try_send(PlaybackCommand::Close).unwrap();
    let report = playback.join().unwrap().unwrap();
    assert_eq!(report.exit, SessionExit::Closed);
    let telemetry = report.publisher.readback.telemetry();
    assert!(telemetry.frames_read > 0);
    assert_eq!(telemetry.configurations, 1);
    assert_eq!(report.publisher.allocations, 1);
    assert!(report.publisher.checksum > 0);
    let session = report.telemetry.expect("opened session returns telemetry");
    assert_eq!(
        session.decoder_ownership.mft_samples_received,
        session.decoder_ownership.mft_samples_released
    );
    fs::rename(&active, &released).expect("readback close must release the media file");
    fs::remove_file(&released).unwrap();
    fs::remove_dir(&directory).unwrap();
}
