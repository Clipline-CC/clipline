//! Shared test helpers for the cloud modules.
use super::*;

use clipline_cloud_api::{ClipDetailResponse, ClipSummaryResponse};
use clipline_events::ClipAudioTrack;
use clipline_mp4::{
    AudioTrackConfig, FragSample, HybridMp4Writer, TrackConfig, VideoTrackConfig,
};
use chrono::Utc;
use httpmock::prelude::*;
use std::io::Cursor;

pub(crate) fn audio_markers() -> ClipMarkers {
    ClipMarkers {
        bookmarks: Vec::new(),
        recording_start_s: 0.0,
        duration_s: 1.0,
        player_summary: None,
        audio_tracks: vec![
            ClipAudioTrack {
                id: "output".into(),
                track_index: 0,
                label: "Output Audio".into(),
                kind: Some("output".into()),
            },
            ClipAudioTrack {
                id: "microphone".into(),
                track_index: 1,
                label: "Microphone".into(),
                kind: Some("microphone".into()),
            },
        ],
        plays: Vec::new(),
        markers: Vec::new(),
    }
}

pub(crate) fn two_audio_mp4() -> Vec<u8> {
    let tracks = vec![
        TrackConfig::Video(VideoTrackConfig::h264(
            128,
            72,
            90_000,
            vec![0x67, 0x64, 0x00, 0x0A, 0xAC],
            vec![0x68, 0xEE, 0x38, 0x80],
        )),
        TrackConfig::Audio(AudioTrackConfig {
            channels: 2,
            sample_rate: 48_000,
            pre_skip: 312,
        }),
        TrackConfig::Audio(AudioTrackConfig {
            channels: 2,
            sample_rate: 48_000,
            pre_skip: 312,
        }),
    ];
    let mut writer = HybridMp4Writer::new_multi(Cursor::new(Vec::new()), tracks).unwrap();
    let video: Vec<_> = (0..10)
        .map(|i| FragSample {
            data: format!("V{i:05}").into_bytes(),
            duration: 9_000,
            is_sync: i == 0,
        })
        .collect();
    let output = audio_samples("A");
    let mic = audio_samples("B");
    writer
        .write_fragment_multi(&[&video, &output, &mic])
        .unwrap();
    writer.finalize().unwrap().into_inner()
}

pub(crate) fn audio_samples(prefix: &str) -> Vec<FragSample> {
    (0..50)
        .map(|i| FragSample {
            data: format!("{prefix}{i:05}").into_bytes(),
            duration: 960,
            is_sync: true,
        })
        .collect()
}

pub(crate) fn upload_record(
    local_clip_id: &str,
    path: &str,
    upload_status: &str,
    updated_at_unix: u64,
) -> CloudUploadRecord {
    CloudUploadRecord {
        local_clip_id: local_clip_id.into(),
        path: path.into(),
        remote_clip_id: None,
        remote_url: None,
        visibility: "private".into(),
        upload_status: upload_status.into(),
        error: None,
        updated_at_unix,
    }
}

pub(crate) fn clip_detail(
    id: &str,
    visibility: &str,
    status: &str,
    public_url: Option<&str>,
) -> ClipDetailResponse {
    let now = Utc::now();
    ClipDetailResponse {
        id: id.into(),
        client_clip_id: Some("local".into()),
        title: "Clip".into(),
        description: None,
        game_name: None,
        game_id: None,
        game_executable: None,
        source_type: Some("replay".into()),
        recorded_at: None,
        uploaded_at: Some(now),
        duration_ms: None,
        file_size_bytes: None,
        width: None,
        height: None,
        fps: None,
        container: Some("mp4".into()),
        video_codec: None,
        audio_codec: None,
        checksum_sha256: None,
        visibility: visibility.into(),
        status: status.into(),
        public_share_id: None,
        public_url: public_url.map(str::to_string),
        view_count: 0,
        markers: Vec::new(),
        created_at: now,
        updated_at: now,
    }
}

pub(crate) fn test_cloud_client(server: &MockServer) -> CloudClient {
    CloudClient::with_device_token(server.base_url().parse().unwrap(), "token")
}

pub(crate) fn clip_summary(
    id: &str,
    client_clip_id: Option<&str>,
    title: &str,
    visibility: &str,
    status: &str,
    public_url: Option<&str>,
) -> ClipSummaryResponse {
    let now = Utc::now();
    ClipSummaryResponse {
        id: id.into(),
        client_clip_id: client_clip_id.map(str::to_string),
        title: title.into(),
        description: None,
        game_name: Some("League of Legends".into()),
        game_id: Some("league_of_legends".into()),
        source_type: Some("replay".into()),
        recorded_at: Some(now),
        uploaded_at: Some(now),
        duration_ms: Some(30_000),
        file_size_bytes: Some(12_345),
        width: Some(1920),
        height: Some(1080),
        fps: Some(60.0),
        visibility: visibility.into(),
        status: status.into(),
        public_url: public_url.map(str::to_string),
        view_count: 0,
        created_at: now,
        updated_at: now,
    }
}

pub(crate) fn test_dir(name: &str) -> std::path::PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "clipline-cloud-{name}-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
