use super::*;
use crate::osu_enrichment::{pending_path, OsuEnrichmentStatus, OsuPendingEnrichment};
use clipline_test_utils::TestDir;

#[test]
fn non_numeric_usernames_are_resolved_before_recent_score_requests() {
    assert_eq!(osu_user_lookup_segment("Dain"), "@Dain");
    assert_eq!(osu_user_lookup_segment("@Dain"), "@Dain");
    assert_eq!(osu_user_lookup_segment("3426414"), "3426414");

    let resolved = ResolvedOsuUser {
        id: "3426414".into(),
        username: Some("Dain".into()),
    };
    assert_eq!(resolved.id, "3426414");
    assert_eq!(resolved.username.as_deref(), Some("Dain"));
}

#[test]
fn normalize_score_keeps_beatmap_cover_and_star_rating() {
    let raw: RawOsuScore = serde_json::from_value(serde_json::json!({
        "id": 998877,
        "beatmap": {
            "id": 123,
            "version": "Extra",
            "total_length": 178,
            "difficulty_rating": 6.54321
        },
        "beatmapset": {
            "id": 456,
            "title": "Exit This Earth's Atomosphere",
            "artist": "Camellia",
            "creator": "Sotarks",
            "covers": {
                "list": "https://assets.ppy.sh/beatmaps/456/covers/list.jpg",
                "card": "https://assets.ppy.sh/beatmaps/456/covers/card.jpg"
            }
        },
        "mods": [{"acronym": "HD"}],
        "rank": "A",
        "passed": true,
        "accuracy": 0.9876,
        "ended_at": "2026-07-01T04:10:00Z"
    }))
    .expect("deserialize score");

    let score = normalize_score(raw).expect("normalize score");

    assert_eq!(
        score.cover_url.as_deref(),
        Some("https://assets.ppy.sh/beatmaps/456/covers/list.jpg")
    );
    assert_eq!(score.star_rating, Some(6.54321));
}

#[test]
fn blank_secret_save_reuses_existing_secret_when_target_changes() {
    let plan = plan_osu_credential_save(
        "61835",
        "Dain",
        None,
        Some("Clipline osu!:61835:3426414"),
        Some("stored-secret"),
    )
    .expect("existing secret can be reused");

    assert_eq!(plan.target, "Clipline osu!:61835:Dain");
    assert_eq!(plan.secret_to_write.as_deref(), Some("stored-secret"));
    assert_eq!(
        plan.delete_target.as_deref(),
        Some("Clipline osu!:61835:3426414")
    );
}

#[test]
fn blank_secret_save_without_existing_secret_keeps_settings_unchanged() {
    let error = plan_osu_credential_save(
        "61835",
        "Dain",
        None,
        Some("Clipline osu!:61835:3426414"),
        None,
    )
    .expect_err("missing stored secret should be actionable");

    assert!(error.contains("client secret"));
}

#[tokio::test]
async fn pending_retry_without_api_credentials_reports_no_visible_update() {
    let dir = TestDir::new("clipline-osu-api", "retry-no-credentials");
    let clip = dir.path().join("session.mp4");
    std::fs::write(&clip, b"").unwrap();
    let pending = OsuPendingEnrichment {
        schema_version: 1,
        clip_path: clip.display().to_string(),
        recording_start_unix: 1_820_000_000,
        recording_end_unix: 1_820_000_120,
        clip_duration_s: 120.0,
        status: OsuEnrichmentStatus::Pending,
        attempts: 0,
        pagination_ceiling_reached: false,
        title_events: Vec::new(),
        message: None,
    };
    std::fs::write(
        pending_path(&clip),
        serde_json::to_string_pretty(&pending).unwrap(),
    )
    .unwrap();

    let changed =
        retry_pending_enrichment_with_settings(&OsuApiSettings::default(), dir.path().into())
            .await
            .unwrap();

    assert!(
        !changed,
        "missing osu! API credentials should not trigger an osu-enrichment-updated refresh loop"
    );
}

#[test]
fn enrichment_pass_lease_coalesces_per_root_and_releases_on_drop() {
    let first = TestDir::new("clipline-osu-api", "single-flight-first");
    let second = TestDir::new("clipline-osu-api", "single-flight-second");

    let lease = EnrichmentPassLease::try_acquire(first.path())
        .unwrap()
        .expect("first pass owns root");
    assert!(
        EnrichmentPassLease::try_acquire(first.path())
            .unwrap()
            .is_none(),
        "overlapping pass is coalesced"
    );
    let other = EnrichmentPassLease::try_acquire(second.path())
        .unwrap()
        .expect("another root remains independent");
    drop(other);
    drop(lease);

    assert!(
        EnrichmentPassLease::try_acquire(first.path())
            .unwrap()
            .is_some(),
        "root is released after the pass"
    );
}
