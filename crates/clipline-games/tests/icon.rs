mod identity {
    pub use clipline_games::identity::*;
}

#[path = "../src/icon.rs"]
mod icon;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use clipline_games::discovery::{DetectedGameCandidate, DetectedGameSource};
use clipline_games::identity::{
    CustomGameIdentity, GameItemIdentity, InstalledGameIdentityCatalog, PluginGameIdentity, OSU_ID,
};
use clipline_settings::{
    ProbeKind, ProbeRequestGeneration, ProbeSessionOwner, ProbeToken, SettingsAttachmentGeneration,
    SettingsForegroundGeneration, SettingsSessionGeneration,
};
use icon::{
    decode_game_icon_source, GameIconCache, GameIconCacheError, GameIconCompletionOutcome,
    GameIconError, GameIconId, GameIconLoadState, GameIconManifest, GameIconManifestEntry,
    GameIconManifestGeneration, GameIconSource, MAX_GAME_ICON_ASSET_PATH_BYTES,
    MAX_GAME_ICON_BASE64_BYTES, MAX_GAME_ICON_DATA_URL_BYTES,
    MAX_GAME_ICON_DECODED_OWNERSHIP_BYTES, MAX_GAME_ICON_DIMENSION,
    MAX_GAME_ICON_ENCODED_PNG_BYTES, MAX_GAME_ICON_MANIFEST_ENTRIES, MAX_GAME_ICON_RGBA_BYTES,
    MAX_GAME_ICON_SLOTS, MAX_SOURCE_ICON_DIMENSION, MAX_SOURCE_ICON_PIXELS,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

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

fn icon_id(owner: ProbeSessionOwner, index: usize) -> GameIconId {
    GameIconId::new(
        owner,
        GameItemIdentity::Custom(CustomGameIdentity::new(&format!("custom-game-{index}")).unwrap()),
    )
    .unwrap()
}

fn try_manifest(
    owner: ProbeSessionOwner,
    generation: u64,
    count: usize,
    source: &GameIconSource,
) -> Result<GameIconManifest, GameIconCacheError> {
    let entries = (0..count)
        .map(|index| GameIconManifestEntry::new(icon_id(owner, index), source.clone()))
        .collect();
    GameIconManifest::new(owner, GameIconManifestGeneration::new(generation)?, entries)
}

fn manifest(
    owner: ProbeSessionOwner,
    generation: u64,
    count: usize,
    source: &GameIconSource,
) -> GameIconManifest {
    try_manifest(owner, generation, count, source).unwrap()
}

#[derive(Debug)]
struct TrackedHandle(Arc<AtomicUsize>);

impl Drop for TrackedHandle {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
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
    let smallest_source = GameIconSource::png_data_url(smallest.clone()).unwrap();
    let debug = format!("{smallest_source:?}");
    assert!(!debug.contains(&smallest));
    assert!(!debug.contains("assets/games/osu.png"));
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
    assert!(!format!("{asset:?}").contains("assets/games/osu.png"));
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

#[test]
fn sixty_row_manifest_admits_only_thirty_two_slots_and_eight_mib() {
    let owner = owner(40);
    let source = GameIconSource::png_data_url(solid_png_data_url(
        MAX_GAME_ICON_DIMENSION,
        MAX_GAME_ICON_DIMENSION,
        [1, 2, 3, 255],
    ))
    .unwrap();
    let page = manifest(owner, 1, MAX_GAME_ICON_MANIFEST_ENTRIES, &source);
    let ids: Vec<_> = page
        .entries()
        .iter()
        .map(|entry| entry.id().clone())
        .collect();
    let mut cache = GameIconCache::<TrackedHandle>::new(owner);
    cache.sync_manifest(page).unwrap();
    let update = cache.set_viewport(&ids).unwrap();
    assert_eq!(update.queued.len(), MAX_GAME_ICON_SLOTS);
    assert_eq!(cache.issued_count(), MAX_GAME_ICON_SLOTS);
    assert_eq!(cache.ownership_count(), MAX_GAME_ICON_SLOTS);
    assert_eq!(cache.load_state(&ids[32]), GameIconLoadState::Missing);

    let drops = Arc::new(AtomicUsize::new(0));
    let last_work = update.queued[0].clone();
    for work in update.queued.into_iter().rev() {
        let decoded = decode_game_icon_source(work.source()).unwrap();
        let completion = cache
            .complete_decoded(&work, decoded, |_| Ok(TrackedHandle(drops.clone())))
            .unwrap();
        assert_eq!(completion.outcome, GameIconCompletionOutcome::Ready);
        assert!(completion.update.queued.is_empty());
    }
    assert_eq!(cache.retained_count(), MAX_GAME_ICON_SLOTS);
    assert_eq!(cache.issued_count(), 0);
    assert_eq!(
        cache.owned_rgba_bytes(),
        MAX_GAME_ICON_DECODED_OWNERSHIP_BYTES
    );
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    let duplicate_constructed = Arc::new(AtomicUsize::new(0));
    let duplicate_decoded = decode_game_icon_source(last_work.source()).unwrap();
    assert_eq!(
        cache
            .complete_decoded(&last_work, duplicate_decoded, |_| {
                duplicate_constructed.fetch_add(1, Ordering::SeqCst);
                Ok(TrackedHandle(Arc::new(AtomicUsize::new(0))))
            })
            .unwrap_err(),
        GameIconCacheError::CompletionMismatch
    );
    assert_eq!(duplicate_constructed.load(Ordering::SeqCst), 0);
}

#[test]
fn slow_canceled_work_keeps_slots_until_exact_ack_then_admits_in_order() {
    let owner = owner(41);
    let source = GameIconSource::png_data_url(solid_png_data_url(1, 1, [1, 2, 3, 255])).unwrap();
    let page = manifest(owner, 1, 60, &source);
    let ids: Vec<_> = page
        .entries()
        .iter()
        .map(|entry| entry.id().clone())
        .collect();
    let mut cache = GameIconCache::<TrackedHandle>::new(owner);
    cache.sync_manifest(page).unwrap();
    let first = cache.set_viewport(&ids[..32]).unwrap();
    assert_eq!(first.queued.len(), 32);

    let churn = cache.set_viewport(&ids[28..60]).unwrap();
    assert_eq!(churn.canceled.len(), 28);
    assert!(churn.queued.is_empty());
    assert_eq!(cache.ownership_count(), 32);

    let ack = cache.acknowledge_canceled(&first.queued[0]).unwrap();
    assert_eq!(ack.queued.len(), 1);
    assert_eq!(ack.queued[0].id(), &ids[32]);
    assert_eq!(cache.ownership_count(), 32);
    assert_eq!(
        cache.acknowledge_canceled(&first.queued[0]).unwrap_err(),
        GameIconCacheError::CompletionMismatch
    );
}

#[test]
fn stale_completion_and_source_replacement_never_construct_a_handle() {
    let owner = owner(42);
    let source = GameIconSource::png_data_url(solid_png_data_url(1, 1, [1, 2, 3, 255])).unwrap();
    let mut cache = GameIconCache::<TrackedHandle>::new(owner);
    let first_page = manifest(owner, 1, 1, &source);
    let id = first_page.entries()[0].id().clone();
    cache.sync_manifest(first_page).unwrap();
    let work = cache.set_viewport(&[id]).unwrap().queued.remove(0);

    let replacement_source =
        GameIconSource::png_data_url(solid_png_data_url(1, 1, [9, 8, 7, 255])).unwrap();
    assert_ne!(work.source_fingerprint(), replacement_source.fingerprint());
    let replacement = manifest(owner, 2, 1, &replacement_source);
    let replaced = cache.sync_manifest(replacement).unwrap();
    assert_eq!(replaced.canceled.len(), 1);
    assert_eq!(replaced.queued.len(), 1);
    assert_eq!(replaced.queued[0].id(), work.id());

    let constructed = Arc::new(AtomicUsize::new(0));
    let decoded = decode_game_icon_source(work.source()).unwrap();
    let result = cache
        .complete_decoded(&work, decoded, |_| {
            constructed.fetch_add(1, Ordering::SeqCst);
            Ok(TrackedHandle(Arc::new(AtomicUsize::new(0))))
        })
        .unwrap();
    assert_eq!(result.outcome, GameIconCompletionOutcome::Ignored);
    assert_eq!(constructed.load(Ordering::SeqCst), 0);
    assert!(result.update.queued.is_empty());
    assert_eq!(cache.ownership_count(), 1);
}

#[test]
fn ready_handle_survives_only_an_identical_source_fingerprint() {
    let owner = owner(49);
    let first_source =
        GameIconSource::png_data_url(solid_png_data_url(1, 1, [1, 2, 3, 255])).unwrap();
    let first_manifest = manifest(owner, 1, 1, &first_source);
    let id = first_manifest.entries()[0].id().clone();
    let mut cache = GameIconCache::<TrackedHandle>::new(owner);
    cache.sync_manifest(first_manifest).unwrap();
    let work = cache
        .set_viewport(std::slice::from_ref(&id))
        .unwrap()
        .queued
        .remove(0);
    let drops = Arc::new(AtomicUsize::new(0));
    cache
        .complete_decoded(
            &work,
            decode_game_icon_source(work.source()).unwrap(),
            |_| Ok(TrackedHandle(drops.clone())),
        )
        .unwrap();

    let retained = cache
        .sync_manifest(manifest(owner, 2, 1, &first_source))
        .unwrap();
    assert_eq!(retained.released, 0);
    assert!(retained.queued.is_empty());
    assert_eq!(cache.load_state(&id), GameIconLoadState::Ready);
    assert_eq!(drops.load(Ordering::SeqCst), 0);

    let changed_source =
        GameIconSource::png_data_url(solid_png_data_url(1, 1, [9, 8, 7, 255])).unwrap();
    let changed = cache
        .sync_manifest(manifest(owner, 3, 1, &changed_source))
        .unwrap();
    assert_eq!(changed.released, 1);
    assert_eq!(changed.queued.len(), 1);
    assert_eq!(cache.load_state(&id), GameIconLoadState::Loading);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn full_pending_replacement_waits_for_exact_ack_before_admission() {
    let owner = owner(46);
    let first_source =
        GameIconSource::png_data_url(solid_png_data_url(1, 1, [1, 2, 3, 255])).unwrap();
    let second_source =
        GameIconSource::png_data_url(solid_png_data_url(1, 1, [4, 5, 6, 255])).unwrap();
    let first_manifest = manifest(owner, 1, 32, &first_source);
    let ids: Vec<_> = first_manifest
        .entries()
        .iter()
        .map(|entry| entry.id().clone())
        .collect();
    let mut cache = GameIconCache::<TrackedHandle>::new(owner);
    cache.sync_manifest(first_manifest).unwrap();
    let first = cache.set_viewport(&ids).unwrap();
    assert_eq!(first.queued.len(), 32);

    let replacement = cache
        .sync_manifest(manifest(owner, 2, 32, &second_source))
        .unwrap();
    assert_eq!(replacement.canceled.len(), 32);
    assert!(replacement.queued.is_empty());
    assert_eq!(cache.ownership_count(), 32);

    let update = cache.acknowledge_canceled(&first.queued[7]).unwrap();
    assert_eq!(update.queued.len(), 1);
    assert_eq!(update.queued[0].id(), &ids[0]);
    assert_ne!(
        update.queued[0].source_fingerprint(),
        first.queued[7].source_fingerprint()
    );
    assert_eq!(cache.ownership_count(), 32);
}

#[test]
fn per_icon_decode_and_handle_failure_do_not_invalidate_the_page() {
    let owner = owner(43);
    let source = GameIconSource::png_data_url(solid_png_data_url(1, 1, [1, 2, 3, 255])).unwrap();
    let page = manifest(owner, 1, 2, &source);
    let ids: Vec<_> = page
        .entries()
        .iter()
        .map(|entry| entry.id().clone())
        .collect();
    let mut cache = GameIconCache::<TrackedHandle>::new(owner);
    cache.sync_manifest(page).unwrap();
    let work = cache.set_viewport(&ids).unwrap().queued;

    let failed = cache.complete_failed(&work[0]).unwrap();
    assert_eq!(failed.outcome, GameIconCompletionOutcome::Failed);
    assert_eq!(cache.load_state(&ids[0]), GameIconLoadState::Failed);
    assert_eq!(cache.load_state(&ids[1]), GameIconLoadState::Loading);

    let decoded = decode_game_icon_source(work[1].source()).unwrap();
    let failed = cache
        .complete_decoded(&work[1], decoded, |_| Err("allocation failed".into()))
        .unwrap();
    assert_eq!(failed.outcome, GameIconCompletionOutcome::Failed);
    assert_eq!(cache.load_state(&ids[1]), GameIconLoadState::Failed);
}

#[test]
fn viewport_and_detach_release_every_move_only_handle() {
    let owner = owner(44);
    let source = GameIconSource::png_data_url(solid_png_data_url(1, 1, [1, 2, 3, 255])).unwrap();
    let page = manifest(owner, 1, 2, &source);
    let ids: Vec<_> = page
        .entries()
        .iter()
        .map(|entry| entry.id().clone())
        .collect();
    let mut cache = GameIconCache::<TrackedHandle>::new(owner);
    cache.sync_manifest(page).unwrap();
    let work = cache.set_viewport(&ids).unwrap().queued;
    let drops = Arc::new(AtomicUsize::new(0));
    for work in &work {
        let decoded = decode_game_icon_source(work.source()).unwrap();
        cache
            .complete_decoded(work, decoded, |_| Ok(TrackedHandle(drops.clone())))
            .unwrap();
    }
    assert_eq!(cache.load_state(&ids[0]), GameIconLoadState::Ready);
    let left = cache.set_viewport(&ids[1..]).unwrap();
    assert_eq!(left.released, 1);
    assert_eq!(drops.load(Ordering::SeqCst), 1);

    let detached = cache.detach().unwrap();
    assert_eq!(detached.released, 1);
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    assert_eq!(cache.owned_rgba_bytes(), 0);
}

#[test]
fn detach_keeps_pending_capacity_charged_until_worker_acknowledges() {
    let owner = owner(47);
    let source = GameIconSource::png_data_url(solid_png_data_url(1, 1, [1, 2, 3, 255])).unwrap();
    let page = manifest(owner, 1, 2, &source);
    let ids: Vec<_> = page
        .entries()
        .iter()
        .map(|entry| entry.id().clone())
        .collect();
    let mut cache = GameIconCache::<TrackedHandle>::new(owner);
    cache.sync_manifest(page).unwrap();
    let work = cache.set_viewport(&ids).unwrap().queued;
    let detached = cache.detach().unwrap();
    assert_eq!(detached.canceled.len(), 2);
    assert_eq!(cache.ownership_count(), 2);
    assert!(cache
        .acknowledge_canceled(&work[0])
        .unwrap()
        .queued
        .is_empty());
    assert!(cache
        .acknowledge_canceled(&work[1])
        .unwrap()
        .queued
        .is_empty());
    assert_eq!(cache.ownership_count(), 0);
}

#[test]
fn ticket_exhaustion_never_wraps_or_mutates_an_existing_manifest() {
    let owner = owner(48);
    let source = GameIconSource::png_data_url(solid_png_data_url(1, 1, [1, 2, 3, 255])).unwrap();
    let page = manifest(owner, 1, 1, &source);
    let id = page.entries()[0].id().clone();
    let mut cache = GameIconCache::<TrackedHandle>::new(owner);
    cache.sync_manifest(page).unwrap();
    cache.set_next_ticket_for_test(u64::MAX);
    let update = cache.set_viewport(std::slice::from_ref(&id)).unwrap();
    assert_eq!(
        update.admission_error,
        Some(GameIconCacheError::TicketExhausted)
    );
    assert_eq!(cache.load_state(&id), GameIconLoadState::Failed);
    assert_eq!(cache.ownership_count(), 0);
}

#[test]
fn manifest_and_generation_bounds_fail_closed() {
    let current_owner = owner(45);
    assert_eq!(
        GameIconManifestGeneration::new(0).unwrap_err(),
        GameIconCacheError::ZeroGeneration
    );
    assert_eq!(
        GameIconManifestGeneration::new(u64::MAX)
            .unwrap()
            .checked_next()
            .unwrap_err(),
        GameIconCacheError::GenerationExhausted
    );
    let source = GameIconSource::missing();
    let bounded = manifest(current_owner, 7, 1, &source);
    assert_eq!(bounded.owner(), current_owner);
    assert_eq!(bounded.generation().get(), 7);
    assert_eq!(bounded.entries()[0].source(), &source);
    assert_eq!(
        bounded.entries()[0].source_fingerprint(),
        source.fingerprint()
    );
    assert_eq!(
        try_manifest(
            current_owner,
            1,
            MAX_GAME_ICON_MANIFEST_ENTRIES + 1,
            &GameIconSource::missing(),
        )
        .unwrap_err(),
        GameIconCacheError::ManifestTooLarge
    );

    let wrong_owner_entry =
        GameIconManifestEntry::new(icon_id(owner(99), 1), GameIconSource::missing());
    assert_eq!(
        GameIconManifest::new(
            current_owner,
            GameIconManifestGeneration::new(9).unwrap(),
            vec![wrong_owner_entry]
        )
        .unwrap_err(),
        GameIconCacheError::OwnerMismatch
    );
    let duplicate =
        GameIconManifestEntry::new(icon_id(current_owner, 2), GameIconSource::missing());
    assert_eq!(
        GameIconManifest::new(
            current_owner,
            GameIconManifestGeneration::new(10).unwrap(),
            vec![duplicate.clone(), duplicate]
        )
        .unwrap_err(),
        GameIconCacheError::DuplicateIdentity
    );
}
