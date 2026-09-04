use super::*;

use clipline_events::{ClipAudioTrack, EventKind, GameEvent, GameId};
use clipline_mp4::{AudioTrackConfig, FragSample, HybridMp4Writer, TrackConfig, VideoTrackConfig};
use shiguredo_opus::{Encoder, EncoderConfig};
use std::io::Cursor;

pub(crate) fn marker(t_s: f64) -> ClipMarker {
    marker_with(t_s, EventKind::ChampionKill, true)
}

pub(crate) fn marker_with(t_s: f64, kind: EventKind, involves_local_player: bool) -> ClipMarker {
    ClipMarker {
        t_s,
        event: GameEvent {
            game_id: GameId::LeagueOfLegends,
            kind,
            actor: "Dain".into(),
            victim: None,
            assisters: Vec::new(),
            subtype: None,
            game_time_s: 0.0,
            recording_offset_s: Some(10.0 + t_s),
            importance: 7,
            involves_local_player,
        },
    }
}

pub(crate) fn osu_play(t_start_s: f64, t_end_s: Option<f64>, external_id: &str) -> ClipPlay {
    ClipPlay {
        game_id: GameId::Osu,
        source: "osu_api".into(),
        external_id: external_id.into(),
        url: None,
        beatmap_id: Some(123),
        beatmapset_id: Some(456),
        cover_url: None,
        title: "Everything will freeze".into(),
        artist: "UNDEAD CORPORATION".into(),
        difficulty: "Time Freeze".into(),
        mapper: Some("Ekoro".into()),
        star_rating: None,
        mods: vec!["HD".into()],
        rank: Some("A".into()),
        passed: true,
        accuracy: Some(0.9876),
        max_combo: Some(1234),
        total_score: Some(987654),
        pp: Some(123.4),
        started_at: Some("2026-06-30T23:54:00+00:00".into()),
        ended_at: "2026-06-30T23:56:00+00:00".into(),
        derived_start: false,
        t_start_s,
        t_end_s,
    }
}

pub(crate) fn write_audio_track_markers(source: &Path, tracks: Vec<(&str, u32, &str)>) {
    let markers = ClipMarkers {
        bookmarks: Vec::new(),
        recording_start_s: 0.0,
        duration_s: 1.0,
        player_summary: None,
        audio_tracks: tracks
            .into_iter()
            .map(|(id, track_index, label)| ClipAudioTrack {
                id: id.into(),
                track_index,
                label: label.into(),
                kind: Some("test".into()),
            })
            .collect(),
        plays: Vec::new(),
        markers: Vec::new(),
    };
    std::fs::write(
        source.with_extension("markers.json"),
        serde_json::to_string(&markers).unwrap(),
    )
    .unwrap();
}

pub(crate) fn pending_osu_enrichment(clip: &Path) -> crate::osu_enrichment::OsuPendingEnrichment {
    crate::osu_enrichment::OsuPendingEnrichment {
        schema_version: 1,
        clip_path: clip.display().to_string(),
        recording_start_unix: 10,
        recording_end_unix: 20,
        clip_duration_s: 10.0,
        status: crate::osu_enrichment::OsuEnrichmentStatus::Pending,
        attempts: 0,
        pagination_ceiling_reached: false,
        title_events: Vec::new(),
        message: None,
    }
}

pub(crate) fn touch_mp4(path: &Path) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, b"\0\0\0\0").unwrap();
}

pub(crate) fn two_real_opus_audio_mp4() -> Vec<u8> {
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
    writer
        .write_fragment_multi(&[&video, &opus_audio_packets(0.20), &opus_audio_packets(0.25)])
        .unwrap();
    writer.finalize().unwrap().into_inner()
}

pub(crate) fn audio_only_opus_mp4_for_stream(audio_stream_index: u32) -> Vec<u8> {
    let amplitude = 0.20 + 0.05 * audio_stream_index as f32;
    let tracks = vec![TrackConfig::Audio(AudioTrackConfig {
        channels: 2,
        sample_rate: 48_000,
        pre_skip: 312,
    })];
    let mut writer = HybridMp4Writer::new_multi(Cursor::new(Vec::new()), tracks).unwrap();
    let packets = opus_audio_packets(amplitude);
    writer.write_fragment_multi(&[&packets]).unwrap();
    writer.finalize().unwrap().into_inner()
}

pub(crate) fn opus_audio_packets(amplitude: f32) -> Vec<FragSample> {
    let mut encoder = Encoder::new(EncoderConfig::new(48_000, 2)).unwrap();
    (0..50)
        .map(|frame_idx| {
            let mut pcm = Vec::with_capacity(960 * 2);
            for sample_idx in 0..960 {
                let t = (frame_idx * 960 + sample_idx) as f32 / 48_000.0;
                let sample = (t * 440.0 * std::f32::consts::TAU).sin() * amplitude;
                pcm.extend([sample, sample]);
            }
            let encoded = encoder.encode_f32(&pcm).unwrap();
            FragSample {
                data: encoded,
                duration: 960,
                is_sync: true,
            }
        })
        .collect()
}
