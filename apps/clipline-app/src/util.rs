//! Shared helpers used by multiple app modules.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use clipline_events::{ClipAudioTrack, ClipMarkers};

/// Read the `.markers.json` sidecar next to a clip file.
pub(crate) fn read_markers_raw(path: &Path) -> Option<ClipMarkers> {
    std::fs::read_to_string(path.with_extension("markers.json"))
        .ok()
        .and_then(|json| serde_json::from_str(&json).ok())
}

pub(crate) fn markers_with_inferred_audio_tracks(
    path: &Path,
    markers: Option<ClipMarkers>,
) -> Option<ClipMarkers> {
    if markers
        .as_ref()
        .is_some_and(|markers| !markers.audio_tracks.is_empty())
    {
        return markers;
    }

    let audio_tracks = infer_audio_tracks_from_file(path).unwrap_or_default();
    if audio_tracks.is_empty() {
        return markers;
    }

    Some(match markers {
        Some(mut markers) => {
            markers.audio_tracks = audio_tracks;
            markers
        }
        None => ClipMarkers {
            bookmarks: Vec::new(),
            recording_start_s: 0.0,
            duration_s: 0.0,
            player_summary: None,
            audio_tracks,
            plays: Vec::new(),
            markers: Vec::new(),
        },
    })
}

fn infer_audio_tracks_from_file(path: &Path) -> Result<Vec<ClipAudioTrack>, String> {
    let count = clipline_mp4::media_track_counts_file(path)
        .map_err(|e| e.to_string())?
        .audio;
    Ok((0..count)
        .map(|index| ClipAudioTrack {
            id: format!("audio:{index}"),
            track_index: index as u32,
            label: format!("Audio Track {}", index + 1),
            kind: Some("inferred".into()),
        })
        .collect())
}

/// Current wall-clock time as seconds since the Unix epoch.
pub(crate) fn unix_now() -> u64 {
    unix_seconds_at(SystemTime::now())
}

pub(crate) fn unix_now_i64() -> i64 {
    unix_seconds_i64_at(SystemTime::now())
}

fn unix_seconds_at(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn unix_seconds_i64_at(time: SystemTime) -> i64 {
    i64::try_from(unix_seconds_at(time)).unwrap_or(i64::MAX)
}

/// Resolve user-facing audio track IDs to their MP4 track indices, validating
/// for duplicates and unknown IDs.
pub(crate) fn selected_audio_track_indices(
    markers: &ClipMarkers,
    selected_audio_track_ids: &[String],
) -> Result<Vec<u32>, String> {
    let selected_ids: BTreeSet<&str> = selected_audio_track_ids
        .iter()
        .map(String::as_str)
        .collect();
    if selected_ids.len() != selected_audio_track_ids.len() {
        return Err("audio track selection contains duplicates".into());
    }
    let available: BTreeSet<&str> = markers
        .audio_tracks
        .iter()
        .map(|track| track.id.as_str())
        .collect();
    if let Some(unknown) = selected_ids
        .iter()
        .find(|selected| !available.contains(**selected))
    {
        return Err(format!("unknown audio track {unknown:?}"));
    }
    Ok(markers
        .audio_tracks
        .iter()
        .filter(|track| selected_ids.contains(track.id.as_str()))
        .map(|track| track.track_index)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_mp4::{
        AudioTrackConfig, FragSample, HybridMp4Writer, TrackConfig, VideoTrackConfig,
    };
    use std::io::Cursor;

    #[test]
    fn signed_unix_seconds_clamps_pre_epoch_and_maps_normal_time() {
        assert_eq!(
            unix_seconds_i64_at(UNIX_EPOCH - std::time::Duration::from_secs(1)),
            0
        );
        assert_eq!(
            unix_seconds_i64_at(UNIX_EPOCH + std::time::Duration::from_secs(42)),
            42
        );
    }

    fn two_audio_fixture() -> Vec<u8> {
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
        let audio = |prefix: &str| {
            (0..50)
                .map(|i| FragSample {
                    data: format!("{prefix}{i:05}").into_bytes(),
                    duration: 960,
                    is_sync: true,
                })
                .collect::<Vec<_>>()
        };
        writer
            .write_fragment_multi(&[&video, &audio("A"), &audio("B")])
            .unwrap();
        writer.finalize().unwrap().into_inner()
    }

    #[test]
    fn infer_audio_tracks_uses_file_track_counts_and_preserves_legacy_order() {
        let source = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/util.rs"),
        )
        .unwrap();
        let start = source
            .find("fn infer_audio_tracks_from_file")
            .expect("inference helper exists");
        let end = source[start..]
            .find("\n/// Current wall-clock")
            .map(|offset| start + offset)
            .expect("inference helper end marker exists");
        let body = &source[start..end];
        assert!(
            body.contains("media_track_counts_file"),
            "legacy inference must use bounded file metadata counts"
        );
        assert!(
            !body.contains("std::fs::read(path)"),
            "legacy inference must not read the full source file"
        );

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "clipline-util-infer-audio-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("legacy.mp4");
        std::fs::write(&clip, two_audio_fixture()).unwrap();

        let inferred = markers_with_inferred_audio_tracks(&clip, None)
            .expect("legacy multitrack clip should gain inferred audio metadata");

        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(inferred.audio_tracks.len(), 2);
        assert_eq!(inferred.audio_tracks[0].id, "audio:0");
        assert_eq!(inferred.audio_tracks[0].track_index, 0);
        assert_eq!(inferred.audio_tracks[1].id, "audio:1");
        assert_eq!(inferred.audio_tracks[1].track_index, 1);
    }
}
