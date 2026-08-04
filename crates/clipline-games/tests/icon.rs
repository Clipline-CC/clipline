mod identity {
    pub use clipline_games::identity::*;
}

#[path = "../src/icon.rs"]
mod icon;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use clipline_games::discovery::{DetectedGameCandidate, DetectedGameSource};
use clipline_games::identity::{
    GameItemIdentity, InstalledGameIdentityCatalog, PluginGameIdentity, OSU_ID,
};
use clipline_settings::{
    ProbeKind, ProbeRequestGeneration, ProbeSessionOwner, ProbeToken, SettingsAttachmentGeneration,
    SettingsForegroundGeneration, SettingsSessionGeneration,
};
use icon::{
    decode_game_icon_source, GameIconError, GameIconId, GameIconLoadState, GameIconSource,
    MAX_GAME_ICON_ASSET_PATH_BYTES, MAX_GAME_ICON_BASE64_BYTES, MAX_GAME_ICON_DATA_URL_BYTES,
    MAX_GAME_ICON_DIMENSION, MAX_GAME_ICON_ENCODED_PNG_BYTES, MAX_GAME_ICON_RGBA_BYTES,
    MAX_SOURCE_ICON_DIMENSION, MAX_SOURCE_ICON_PIXELS,
};

fn owner(session: u64) -> ProbeSessionOwner {
    ProbeSessionOwner::new(
        SettingsSessionGeneration::new(session),
        SettingsAttachmentGeneration::new(2),
        SettingsForegroundGeneration::new(3),
    )
}

fn installed_token(owner: ProbeSessionOwner) -> ProbeToken {
    ProbeToken {
        owner,
        kind: ProbeKind::InstalledGames,
        request_generation: ProbeRequestGeneration::new(4),
    }
}

fn candidate() -> DetectedGameCandidate {
    DetectedGameCandidate {
        id_hint: "steam-1".into(),
        name: "Game".into(),
        source: DetectedGameSource::Steam,
        steam_app_id: Some(1),
        install_dir: Some("c:\\games\\game".into()),
        exe_name: "game.exe".into(),
        process_path: Some("c:\\games\\game\\game.exe".into()),
        window_title: String::new(),
        icon: None,
        confidence: 80,
    }
}

fn png_data_url(width: u32, height: u32, rgba: &[u8]) -> String {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(rgba).unwrap();
    }
    format!("data:image/png;base64,{}", STANDARD.encode(bytes))
}

fn solid_png_data_url(width: u32, height: u32, rgba: [u8; 4]) -> String {
    let pixel_count = usize::try_from(u64::from(width) * u64::from(height)).unwrap();
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(pixel_count * 4).unwrap();
    for _ in 0..pixel_count {
        pixels.extend_from_slice(&rgba);
    }
    png_data_url(width, height, &pixels)
}

#[test]
fn icon_id_scopes_every_item_and_rejects_a_candidate_from_another_owner() {
    let first_owner = owner(1);
    let plugin = GameIconId::new(
        first_owner,
        GameItemIdentity::Plugin(PluginGameIdentity::new(OSU_ID).unwrap()),
    )
    .unwrap();
    assert_eq!(plugin.owner(), first_owner);
    assert!(matches!(plugin.item(), GameItemIdentity::Plugin(_)));

    let catalog =
        InstalledGameIdentityCatalog::build(installed_token(first_owner), vec![candidate()])
            .unwrap();
    let item = GameItemIdentity::Candidate(catalog.identity_at(0).unwrap().clone());
    assert!(GameIconId::new(first_owner, item.clone()).is_ok());
    assert_eq!(
        GameIconId::new(owner(9), item).unwrap_err(),
        GameIconError::CandidateOwnerMismatch
    );

    let states = [
        GameIconLoadState::Missing,
        GameIconLoadState::Loading,
        GameIconLoadState::Ready,
        GameIconLoadState::Failed,
    ];
    assert_eq!(states.len(), 4);
}

#[test]
fn sources_enforce_mime_base64_overhead_and_asset_path_bounds() {
    let smallest = solid_png_data_url(1, 1, [1, 2, 3, 255]);
    assert!(GameIconSource::png_data_url(smallest).is_ok());
    assert_eq!(
        GameIconSource::png_data_url("data:image/jpeg;base64,AAAA".into()).unwrap_err(),
        GameIconError::UnsupportedPngDataUrl
    );
    assert_eq!(
        GameIconSource::png_data_url("not-a-data-url".into()).unwrap_err(),
        GameIconError::UnsupportedPngDataUrl
    );
    let oversized = format!(
        "data:image/png;base64,{}",
        "A".repeat(MAX_GAME_ICON_BASE64_BYTES + 1)
    );
    assert_eq!(
        GameIconSource::png_data_url(oversized).unwrap_err(),
        GameIconError::EncodedSourceTooLarge
    );

    let asset = GameIconSource::first_party_asset_path("assets/games/osu.png".into()).unwrap();
    assert_eq!(
        asset.as_first_party_asset_path(),
        Some("assets/games/osu.png")
    );
    assert!(asset.as_png_data_url().is_none());
    assert_eq!(
        GameIconSource::first_party_asset_path(String::new()).unwrap_err(),
        GameIconError::InvalidAssetPath
    );
    assert_eq!(
        GameIconSource::first_party_asset_path("bad\0path".into()).unwrap_err(),
        GameIconError::InvalidAssetPath
    );
    assert_eq!(
        GameIconSource::first_party_asset_path("assets/games/../../private.png".into())
            .unwrap_err(),
        GameIconError::InvalidAssetPath
    );
    assert_eq!(
        GameIconSource::first_party_asset_path("assets/games/unreviewed.png".into()).unwrap_err(),
        GameIconError::InvalidAssetPath
    );
    assert_eq!(
        GameIconSource::first_party_asset_path("x".repeat(MAX_GAME_ICON_ASSET_PATH_BYTES + 1))
            .unwrap_err(),
        GameIconError::AssetPathTooLarge
    );
}

#[test]
fn decode_preserves_small_rgba_and_accepts_the_exact_output_boundary() {
    let source = GameIconSource::png_data_url(solid_png_data_url(2, 1, [7, 8, 9, 10])).unwrap();
    let decoded = decode_game_icon_source(&source).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (2, 1));
    assert_eq!(decoded.rgba(), &[7, 8, 9, 10, 7, 8, 9, 10]);

    let boundary = GameIconSource::png_data_url(solid_png_data_url(
        MAX_GAME_ICON_DIMENSION,
        MAX_GAME_ICON_DIMENSION,
        [20, 30, 40, 255],
    ))
    .unwrap();
    let decoded = decode_game_icon_source(&boundary).unwrap();
    assert_eq!(
        (decoded.width(), decoded.height()),
        (MAX_GAME_ICON_DIMENSION, MAX_GAME_ICON_DIMENSION)
    );
    assert_eq!(decoded.into_rgba().len(), MAX_GAME_ICON_RGBA_BYTES);
}

#[test]
fn decode_resizes_aspect_ratio_within_the_pinned_output_cap() {
    let source = GameIconSource::png_data_url(solid_png_data_url(
        MAX_SOURCE_ICON_DIMENSION,
        MAX_SOURCE_ICON_DIMENSION / 2,
        [100, 110, 120, 255],
    ))
    .unwrap();
    let decoded = decode_game_icon_source(&source).unwrap();
    assert_eq!(
        (decoded.width(), decoded.height()),
        (MAX_GAME_ICON_DIMENSION, MAX_GAME_ICON_DIMENSION / 2)
    );
    assert_eq!(
        decoded.rgba().len(),
        usize::try_from(MAX_GAME_ICON_DIMENSION * (MAX_GAME_ICON_DIMENSION / 2) * 4).unwrap()
    );
    assert!(decoded.rgba().len() <= MAX_GAME_ICON_RGBA_BYTES);
}

#[test]
fn decoder_allocation_limit_accepts_the_exact_source_pixel_boundary() {
    let source = GameIconSource::png_data_url(solid_png_data_url(
        MAX_SOURCE_ICON_DIMENSION,
        MAX_SOURCE_ICON_DIMENSION,
        [1, 2, 3, 255],
    ))
    .unwrap();
    let decoded = decode_game_icon_source(&source).unwrap();
    assert_eq!(
        (decoded.width(), decoded.height()),
        (MAX_GAME_ICON_DIMENSION, MAX_GAME_ICON_DIMENSION)
    );
    assert_eq!(decoded.rgba().len(), MAX_GAME_ICON_RGBA_BYTES);
}

#[test]
fn header_preflight_rejects_hostile_dimensions_before_png_decode() {
    let mut header = Vec::from(&b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR"[..]);
    header.extend_from_slice(&(MAX_SOURCE_ICON_DIMENSION + 1).to_be_bytes());
    header.extend_from_slice(&1_u32.to_be_bytes());
    let source =
        GameIconSource::png_data_url(format!("data:image/png;base64,{}", STANDARD.encode(header)))
            .unwrap();
    assert_eq!(
        decode_game_icon_source(&source).unwrap_err(),
        GameIconError::SourceDimensionsTooLarge
    );
    assert_eq!(
        MAX_SOURCE_ICON_PIXELS,
        u64::from(MAX_SOURCE_ICON_DIMENSION) * u64::from(MAX_SOURCE_ICON_DIMENSION)
    );
}

#[test]
fn malformed_base64_png_and_deferred_sources_fail_closed() {
    let invalid_base64 = GameIconSource::png_data_url("data:image/png;base64,%%%".into()).unwrap();
    assert_eq!(
        decode_game_icon_source(&invalid_base64).unwrap_err(),
        GameIconError::InvalidBase64
    );
    let not_png = GameIconSource::png_data_url(format!(
        "data:image/png;base64,{}",
        STANDARD.encode(b"not png")
    ))
    .unwrap();
    assert_eq!(
        decode_game_icon_source(&not_png).unwrap_err(),
        GameIconError::InvalidPngHeader
    );
    assert_eq!(
        decode_game_icon_source(&GameIconSource::missing()).unwrap_err(),
        GameIconError::MissingSource
    );
    let deferred = GameIconSource::first_party_asset_path("assets/games/osu.png".into()).unwrap();
    assert_eq!(
        decode_game_icon_source(&deferred).unwrap_err(),
        GameIconError::AssetReadDeferred
    );
}

#[test]
fn decoded_png_bytes_are_bounded_independently_of_base64_overhead() {
    let exact_bytes = vec![0_u8; MAX_GAME_ICON_ENCODED_PNG_BYTES];
    let exact_url = format!("data:image/png;base64,{}", STANDARD.encode(exact_bytes));
    assert_eq!(exact_url.len(), MAX_GAME_ICON_DATA_URL_BYTES);
    let exact_source = GameIconSource::png_data_url(exact_url).unwrap();
    assert_eq!(
        decode_game_icon_source(&exact_source).unwrap_err(),
        GameIconError::InvalidPngHeader
    );

    let one_over = vec![0_u8; MAX_GAME_ICON_ENCODED_PNG_BYTES + 1];
    let source = GameIconSource::png_data_url(format!(
        "data:image/png;base64,{}",
        STANDARD.encode(one_over)
    ))
    .unwrap();
    assert_eq!(
        decode_game_icon_source(&source).unwrap_err(),
        GameIconError::EncodedPngTooLarge
    );
}
