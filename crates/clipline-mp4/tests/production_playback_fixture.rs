use std::path::PathBuf;

use clipline_mp4::walker::{children, find, movie_duration_s, walk};
use clipline_mp4::{
    media_track_counts, media_video_codecs, IndexedMovie, MediaTrackCounts, MediaVideoCodec,
    PlaybackTrackConfig,
};

#[path = "support/production_fixture.rs"]
mod production_fixture;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/playback/hybrid-writer-h264-two-opus-5s.mp4")
}

#[test]
fn checked_in_production_fixture_is_reproducible_and_playback_shaped() {
    let expected =
        production_fixture::generate().expect("remux decoder oracle through HybridMp4Writer");
    let committed = std::fs::read(fixture_path()).expect(
        "missing production fixture; run `cargo run -p clipline-mp4 --example generate_production_playback_fixture`",
    );

    assert_eq!(committed, expected, "production fixture is stale");
    assert_ne!(
        committed,
        production_fixture::SOURCE,
        "fixture must not retain the foreign mux"
    );
    assert_eq!(
        media_track_counts(&committed).expect("read fixture tracks"),
        MediaTrackCounts { video: 1, audio: 2 }
    );
    assert_eq!(
        media_video_codecs(&committed).expect("read fixture video codec"),
        vec![MediaVideoCodec::H264]
    );

    let top = walk(&committed);
    assert_eq!(
        top.iter().map(|item| item.fourcc).collect::<Vec<_>>(),
        vec![*b"ftyp", *b"mdat", *b"moov"]
    );
    let moov = find(&top, b"moov").expect("finalized moov");
    assert!(find(&children(&committed, moov), b"mvex").is_none());
    let duration = movie_duration_s(&committed).expect("movie duration");
    assert!(
        (5.0..=5.1).contains(&duration),
        "unexpected duration: {duration}"
    );

    let indexed = IndexedMovie::from_reader(std::io::Cursor::new(committed))
        .expect("production fixture must open through the native playback index");
    assert_eq!(indexed.index().tracks.len(), 3);
    assert!(matches!(
        &indexed.index().tracks[0].config,
        PlaybackTrackConfig::H264 {
            nal_length_size: 4,
            ..
        }
    ));
    assert!(matches!(
        &indexed.index().tracks[1].config,
        PlaybackTrackConfig::Opus { .. }
    ));
    assert!(matches!(
        &indexed.index().tracks[2].config,
        PlaybackTrackConfig::Opus { .. }
    ));
}
