use std::collections::BTreeMap;

use clipline_library::{
    build_catalog_projection, gallery_card_preview, marker_digest, marker_style, AssetAlias,
    CatalogDialogAudioTrackProjection, CatalogDialogKind, CatalogDialogProjection,
    CatalogItemIdentity, CatalogLoadState, CatalogMenuProjection, CatalogProjectionInput,
    CatalogProjectionSource, CatalogRevision, CatalogUploadVisibility, ClipGame,
    CloudAccountGeneration, CloudAccountKey, CloudCatalogOwner, CloudLibraryItem, CloudPageNumber,
    GalleryCardConfig, GalleryCardIconConfig, GalleryCardInput, GalleryCardStat,
    GalleryCardTitleFormat, GalleryCardTitlePolicy, GalleryMarker, GalleryPlay,
    GalleryPresentation, GallerySummaryMode, LocalClipGrouping, LocalClipItem, LocalDay,
    LocalDayResolver, LocalGalleryOptions, LocalPageIndex, MarkerCategoryPresentation,
    MarkerKindPresentation, MarkerPresentation, MarkerSidecarSummary, PlayOutcomeSummary,
    PlayerCardSummary, PosterStatus, PresentationError, ProjectionReservation,
    SystemProjectionReservation, UploadSummary, MAX_CATALOG_PAGE_ROWS, MAX_LOCAL_INDEX_ROWS,
};

struct FailingProjectionReservation {
    field: &'static str,
}

impl FailingProjectionReservation {
    const fn new(field: &'static str) -> Self {
        Self { field }
    }
}

impl ProjectionReservation for FailingProjectionReservation {
    fn before_reserve(
        &self,
        field: &'static str,
        _additional: usize,
    ) -> Result<(), PresentationError> {
        if field == self.field {
            Err(PresentationError::Allocation { field })
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy)]
struct TestDays;

impl LocalDayResolver for TestDays {
    fn today_start_unix(&self) -> u64 {
        2_000_000
    }

    fn resolve_day(&self, timestamp: u64) -> LocalDay {
        let day = timestamp / 86_400;
        LocalDay {
            key: format!("day-{day}"),
            label: format!("Day {day}"),
        }
    }
}

fn local_clip(index: usize) -> LocalClipItem {
    LocalClipItem {
        path: format!(r"C:\Clips\Clip-{index:05}.mp4"),
        name: format!("Clip-{index:05}.mp4"),
        title: None,
        kind: "replay".into(),
        session: Some(format!("Session-{}", index / 10)),
        size_mb: index as f64,
        modified_unix: index as u64,
        duration_s: Some(7.0),
        marker_count: 0,
        game: Some(ClipGame {
            id: "test-game".into(),
            name: "Test Game".into(),
        }),
        marker_summary: MarkerSidecarSummary::default(),
    }
}

fn local_projection(
    items: &[LocalClipItem],
    page: u32,
    reservation: &dyn clipline_library::ProjectionReservation,
) -> Result<clipline_library::CatalogProjection, PresentationError> {
    let options = LocalGalleryOptions {
        grouping: LocalClipGrouping::None,
        ..LocalGalleryOptions::default()
    };
    let selected = Vec::new();
    let posters = BTreeMap::new();
    build_catalog_projection(
        &CatalogProjectionInput {
            revision: CatalogRevision::new(9),
            source: CatalogProjectionSource::Local {
                items,
                options: &options,
                page: LocalPageIndex::new(page).unwrap(),
            },
            gallery: &GalleryPresentation::default(),
            selected: &selected,
            active: None,
            posters: &posters,
            menu: None,
            dialog: None,
            uploads: &[],
            load_state: CatalogLoadState::Ready,
        },
        &TestDays,
        reservation,
    )
}

fn league_summary() -> PlayerCardSummary {
    PlayerCardSummary {
        champion_name: "Vel'Koz".into(),
        kills: 11,
        deaths: 19,
        assists: 34,
        creep_score: Some(204),
        game_time_s: Some(1_800),
    }
}

#[test]
fn marker_digest_and_style_match_default_and_plugin_categories() {
    let markers = vec![
        GalleryMarker::new("ChampionKill"),
        GalleryMarker::new("ChampionKill"),
        GalleryMarker::new("DragonKill"),
        GalleryMarker::new("ChampionAssist"),
    ];
    assert_eq!(
        marker_digest(&markers, None).unwrap(),
        "2 kills · 1 assist · 1 objective"
    );

    let plugin = MarkerPresentation {
        kinds: vec![
            MarkerKindPresentation {
                key: "ChampionKill".into(),
                category: "hero".into(),
                glyph: "!".into(),
            },
            MarkerKindPresentation {
                key: "DragonKill".into(),
                category: "objective".into(),
                glyph: String::new(),
            },
        ],
        categories: vec![
            MarkerCategoryPresentation {
                key: "hero".into(),
                singular: "hero play".into(),
                plural: "hero plays".into(),
                glyph: "!".into(),
                color: Some("#ff44aa".into()),
            },
            MarkerCategoryPresentation {
                key: "objective".into(),
                singular: "map objective".into(),
                plural: "map objectives".into(),
                glyph: "◆".into(),
                color: None,
            },
        ],
    };
    assert_eq!(
        marker_digest(&markers, Some(&plugin)).unwrap(),
        "2 hero plays · 1 map objective · 1 assist"
    );
    let style = marker_style("ChampionKill", Some(&plugin)).unwrap();
    assert_eq!(style.category, "hero");
    assert_eq!(style.glyph, "!");
    assert_eq!(style.color.as_deref(), Some("#ff44aa"));

    let default_kill = marker_style("ChampionKill", None).unwrap();
    let default_death = marker_style("ChampionDeath", None).unwrap();
    assert_eq!(default_kill.glyph, "✕");
    assert_eq!(default_death.glyph, "✕");
    assert_eq!(default_kill.category, "kill");
    assert_eq!(default_kill.color.as_deref(), Some("#ff9b9e"));
}

#[test]
fn marker_digest_accepts_the_full_bounded_clip_detail_marker_set() {
    let markers = vec![GalleryMarker::new("ChampionKill"); 10_000];
    assert_eq!(marker_digest(&markers, None).unwrap(), "10000 kills");
}

#[test]
fn gallery_card_prefers_a_custom_clip_title() {
    let preview = gallery_card_preview(
        &GalleryCardInput {
            name: "session_123.mp4".into(),
            title: Some("  Ranked win vs Lux  ".into()),
            kind: "session".into(),
            fallback_title: "Jul 2 · 7:30 PM".into(),
            ..GalleryCardInput::default()
        },
        &GalleryPresentation::default(),
    )
    .unwrap();
    assert_eq!(preview.title, "Ranked win vs Lux");
    assert_eq!(preview.title_source, "clip");
    assert_eq!(preview.summary, "");
}

#[test]
fn league_card_uses_summary_stats_and_data_dragon_portrait() {
    let presentation = GalleryPresentation {
        summary: GallerySummaryMode::PlayerSummaryKda,
        card: GalleryCardConfig {
            title: GalleryCardTitlePolicy::SummaryForFullSession,
            title_format: Some(GalleryCardTitleFormat {
                separator: " | ".into(),
                stats: vec![
                    GalleryCardStat::Kda,
                    GalleryCardStat::CsPerMin {
                        label: "CS/min".into(),
                    },
                ],
            }),
            icon: Some(GalleryCardIconConfig::Portrait {
                label: "Champion".into(),
                aliases: vec![AssetAlias {
                    alias: "vel'koz".into(),
                    key: "Velkoz".into(),
                }],
            }),
        },
        data_dragon_version: Some("16.13.1".into()),
        ..GalleryPresentation::default()
    };
    let session = gallery_card_preview(
        &GalleryCardInput {
            kind: "session".into(),
            fallback_title: "Jun 28 · 12:15 PM".into(),
            player_summary: Some(league_summary()),
            ..GalleryCardInput::default()
        },
        &presentation,
    )
    .unwrap();
    assert_eq!(session.title, "11/19/34 | 6.8 CS/min");
    assert_eq!(session.title_source, "summary");
    assert_eq!(session.summary, "Vel'Koz | 11/19/34");
    let icon = session.icon.unwrap();
    assert_eq!(icon.kind, "portrait");
    assert_eq!(
        icon.url,
        "https://ddragon.leagueoflegends.com/cdn/16.13.1/img/champion/Velkoz.png"
    );
    assert_eq!(icon.label, "Vel'Koz");

    let replay = gallery_card_preview(
        &GalleryCardInput {
            kind: "replay".into(),
            fallback_title: "Jun 28 · 12:15 PM".into(),
            player_summary: Some(league_summary()),
            ..GalleryCardInput::default()
        },
        &presentation,
    )
    .unwrap();
    assert_eq!(replay.title, "Jun 28 · 12:15 PM");
    assert_eq!(replay.title_source, "clip");
}

#[test]
fn osu_session_summary_and_non_session_title_match_player_core() {
    let presentation = GalleryPresentation {
        summary: GallerySummaryMode::OsuSetPlays,
        card: GalleryCardConfig {
            title: GalleryCardTitlePolicy::OsuSessionSummary,
            ..GalleryCardConfig::default()
        },
        ..GalleryPresentation::default()
    };
    let plays = vec![
        GalleryPlay {
            passed: true,
            rank: String::new(),
            pp: None,
        },
        GalleryPlay {
            passed: false,
            rank: "F".into(),
            pp: None,
        },
    ];
    let session = gallery_card_preview(
        &GalleryCardInput {
            kind: "session".into(),
            fallback_title: "Jun 30 · 8:20 PM".into(),
            plays: plays.clone(),
            ..GalleryCardInput::default()
        },
        &presentation,
    )
    .unwrap();
    assert_eq!(session.title, "2 submitted plays");
    assert_eq!(session.summary, "1 pass · 1 fail");

    let replay = gallery_card_preview(
        &GalleryCardInput {
            name: "I MY ME MINE - Trouble.mp4".into(),
            kind: "replay".into(),
            fallback_title: "Jul 1 · 1:20 AM".into(),
            plays,
            ..GalleryCardInput::default()
        },
        &presentation,
    )
    .unwrap();
    assert_eq!(replay.title, "I MY ME MINE - Trouble");
    assert_eq!(replay.title_source, "clip");
}

#[test]
fn plugin_asset_icon_is_owned_and_bounded() {
    let preview = gallery_card_preview(
        &GalleryCardInput {
            name: "Named clip".into(),
            kind: "trim".into(),
            fallback_title: "Custom title".into(),
            ..GalleryCardInput::default()
        },
        &GalleryPresentation {
            card: GalleryCardConfig {
                icon: Some(GalleryCardIconConfig::Asset {
                    url: "data:image/png;base64,plugin-logo".into(),
                    label: "Arena logo".into(),
                }),
                ..GalleryCardConfig::default()
            },
            ..GalleryPresentation::default()
        },
    )
    .unwrap();
    let icon = preview.icon.unwrap();
    assert_eq!(icon.kind, "asset");
    assert_eq!(icon.url, "data:image/png;base64,plugin-logo");
    assert_eq!(icon.label, "Arena logo");
}

#[test]
fn extension_only_clip_name_falls_back_to_the_original_name_like_player_core() {
    let preview = gallery_card_preview(
        &GalleryCardInput {
            name: ".mp4".into(),
            fallback_title: "Fallback".into(),
            ..GalleryCardInput::default()
        },
        &GalleryPresentation::default(),
    )
    .unwrap();
    assert_eq!(preview.title, ".mp4");
}

#[test]
fn local_projection_snapshots_are_bounded_and_deterministic_at_realistic_index_sizes() {
    let cases = [
        (50_usize, 0_u32, 50_usize, "1–50 of 50"),
        (500, 4, MAX_CATALOG_PAGE_ROWS, "241–300 of 500"),
        (2_000, 32, MAX_CATALOG_PAGE_ROWS, "1921–1980 of 2000"),
    ];
    for (count, page, expected_rows, expected_range) in cases {
        let items = (0..count).map(local_clip).collect::<Vec<_>>();
        let projection = local_projection(&items, page, &SystemProjectionReservation).unwrap();
        let rebuilt = local_projection(&items, page, &SystemProjectionReservation).unwrap();
        assert_eq!(projection, rebuilt, "{count}-row rebuild was not stable");
        assert_eq!(projection.rows.len(), expected_rows);
        assert_eq!(projection.groups.len(), usize::from(expected_rows != 0));
        assert_eq!(
            projection.groups.first().map(|group| group.row_start),
            Some(0)
        );
        assert_eq!(
            projection.groups.first().map(|group| group.row_count),
            Some(expected_rows)
        );
        assert_eq!(projection.page.range_text, expected_range);
        assert_eq!(projection.page.total, Some(count));
        assert_eq!(projection.page.page_count, Some(count.div_ceil(60) as u32));
        assert!(projection.rows.iter().all(|row| {
            row.identity.source() == clipline_library::CatalogSource::Local
                && row.title.len() <= clipline_library::MAX_CATALOG_STRING_BYTES
        }));

        let snapshot = serde_json::json!({
            "revision": projection.revision.get(),
            "source": projection.source,
            "rows": projection.rows.len(),
            "groups": projection.groups,
            "page": projection.page,
        });
        assert_eq!(snapshot["revision"], 9);
        assert_eq!(snapshot["source"], "local");
        assert_eq!(snapshot["page"]["range_text"], expected_range);
    }
}

#[test]
fn group_spans_reference_rows_without_duplicating_item_arrays() {
    let mut items = (0..100).map(local_clip).collect::<Vec<_>>();
    for (index, item) in items.iter_mut().enumerate() {
        item.game = Some(ClipGame {
            id: format!("game-{index:03}"),
            name: format!("Game {index:03}"),
        });
    }
    let options = LocalGalleryOptions {
        grouping: LocalClipGrouping::Game,
        ..LocalGalleryOptions::default()
    };
    let selected = Vec::new();
    let posters = BTreeMap::new();
    let projection = build_catalog_projection(
        &CatalogProjectionInput {
            revision: CatalogRevision::new(1),
            source: CatalogProjectionSource::Local {
                items: &items,
                options: &options,
                page: LocalPageIndex::new(0).unwrap(),
            },
            gallery: &GalleryPresentation::default(),
            selected: &selected,
            active: None,
            posters: &posters,
            menu: None,
            dialog: None,
            uploads: &[],
            load_state: CatalogLoadState::Ready,
        },
        &TestDays,
        &SystemProjectionReservation,
    )
    .unwrap();
    assert_eq!(projection.rows.len(), MAX_CATALOG_PAGE_ROWS);
    assert_eq!(projection.groups.len(), MAX_CATALOG_PAGE_ROWS);
    for (index, group) in projection.groups.iter().enumerate() {
        assert_eq!(group.row_start, index);
        assert_eq!(group.row_count, 1);
        assert_eq!(group.total_count, 1);
        assert_eq!(group.start_in_group, 0);
    }
}

#[test]
fn compact_local_projection_formats_aggregate_play_outcomes_without_fake_play_rows() {
    let mut item = local_clip(1);
    item.kind = "session".into();
    item.marker_summary = MarkerSidecarSummary {
        duration_s: 7.0,
        review_marker_count: 3,
        marker_digest: "2 kills · 1 assist".into(),
        audio_track_count: 2,
        plays: PlayOutcomeSummary {
            total: 4,
            passed: 2,
            failed: 1,
            incomplete: 1,
        },
        player_summary: None,
        search_text: "participant champion spell item play".into(),
    };
    let options = LocalGalleryOptions {
        grouping: LocalClipGrouping::None,
        ..LocalGalleryOptions::default()
    };
    let selected = vec![CatalogItemIdentity::Local {
        path: item.path_identity().unwrap(),
    }];
    let posters = BTreeMap::from([(
        item.path_identity().unwrap(),
        PosterStatus::Ready {
            path: r"C:\Posters\one.jpg".into(),
        },
    )]);
    let presentation = GalleryPresentation {
        summary: GallerySummaryMode::OsuSetPlays,
        card: GalleryCardConfig {
            title: GalleryCardTitlePolicy::OsuSessionSummary,
            ..GalleryCardConfig::default()
        },
        ..GalleryPresentation::default()
    };
    let projection = build_catalog_projection(
        &CatalogProjectionInput {
            revision: CatalogRevision::new(1),
            source: CatalogProjectionSource::Local {
                items: std::slice::from_ref(&item),
                options: &options,
                page: LocalPageIndex::new(0).unwrap(),
            },
            gallery: &presentation,
            selected: &selected,
            active: selected.first(),
            posters: &posters,
            menu: None,
            dialog: None,
            uploads: &[],
            load_state: CatalogLoadState::Ready,
        },
        &TestDays,
        &SystemProjectionReservation,
    )
    .unwrap();
    let row = &projection.rows[0];
    assert_eq!(row.title, "4 submitted plays");
    assert_eq!(
        row.outcome_badge.as_deref(),
        Some("2 passes · 1 incomplete · 1 fail")
    );
    assert_eq!(row.marker_badge.as_deref(), Some("2 kills · 1 assist"));
    assert!(row.selected);
    assert!(row.active);
    assert!(matches!(
        row.poster,
        clipline_library::PresentationPoster::Ready { .. }
    ));
}

#[test]
fn projection_retains_exactly_one_menu_dialog_and_sixteen_or_fewer_upload_summaries() {
    let item = local_clip(7);
    let identity = CatalogItemIdentity::Local {
        path: item.path_identity().unwrap(),
    };
    let menu = CatalogMenuProjection {
        target: identity.clone(),
        can_review: true,
        can_rename: true,
        can_delete: true,
        can_upload: true,
        can_reveal: true,
        can_open_browser: false,
        can_copy_link: false,
    };
    let dialog = CatalogDialogProjection {
        kind: CatalogDialogKind::Upload,
        target: identity,
        title: "Upload clip".into(),
        message: "Choose the upload options.".into(),
        confirm_label: "Upload".into(),
        destructive: false,
        text_value: Some("Clip title".into()),
        description: Some("Description".into()),
        visibility: Some(CatalogUploadVisibility::Private),
        audio_tracks: vec![
            CatalogDialogAudioTrackProjection {
                id: "mic".into(),
                label: "Microphone".into(),
                selected: false,
            },
            CatalogDialogAudioTrackProjection {
                id: "output".into(),
                label: "Desktop audio".into(),
                selected: true,
            },
        ],
        delete_local_after_upload: true,
    };
    let upload = UploadSummary {
        local_clip_id: "local-7".into(),
        path: item.path.clone(),
        upload_status: "uploading".into(),
        received_size_bytes: 512,
        file_size_bytes: 1_024,
        remote_clip_id: None,
        remote_url: None,
        error: None,
    };
    let options = LocalGalleryOptions {
        grouping: LocalClipGrouping::None,
        ..LocalGalleryOptions::default()
    };
    let selected = Vec::new();
    let posters = BTreeMap::new();
    let gallery = GalleryPresentation::default();
    let input = CatalogProjectionInput {
        revision: CatalogRevision::new(1),
        source: CatalogProjectionSource::Local {
            items: std::slice::from_ref(&item),
            options: &options,
            page: LocalPageIndex::new(0).unwrap(),
        },
        gallery: &gallery,
        selected: &selected,
        active: None,
        posters: &posters,
        menu: Some(&menu),
        dialog: Some(&dialog),
        uploads: std::slice::from_ref(&upload),
        load_state: CatalogLoadState::Ready,
    };
    assert_eq!(
        build_catalog_projection(
            &input,
            &TestDays,
            &FailingProjectionReservation::new("projection.dialog_audio_tracks")
        ),
        Err(PresentationError::Allocation {
            field: "projection.dialog_audio_tracks"
        })
    );
    assert_eq!(
        build_catalog_projection(
            &input,
            &TestDays,
            &FailingProjectionReservation::new("projection.uploads")
        ),
        Err(PresentationError::Allocation {
            field: "projection.uploads"
        })
    );
    let projection =
        build_catalog_projection(&input, &TestDays, &SystemProjectionReservation).unwrap();
    assert_eq!(projection.menu.as_ref(), Some(&menu));
    assert_eq!(projection.dialog.as_ref(), Some(&dialog));
    assert_eq!(projection.uploads.as_slice(), std::slice::from_ref(&upload));
    assert_eq!(
        projection.rows[0].upload_badge.as_deref(),
        Some("uploading")
    );

    let mut duplicate_tracks = dialog.clone();
    duplicate_tracks.audio_tracks[1].id = "mic".into();
    let duplicate_input = CatalogProjectionInput {
        dialog: Some(&duplicate_tracks),
        ..input.clone()
    };
    assert!(matches!(
        build_catalog_projection(&duplicate_input, &TestDays, &SystemProjectionReservation),
        Err(PresentationError::Invalid {
            field: "projection.dialog.duplicate_audio_track_id"
        })
    ));
}

#[test]
fn cloud_projection_keeps_server_paging_truth_without_inventing_totals() {
    let owner = CloudCatalogOwner {
        account_key: CloudAccountKey::new("account-a").unwrap(),
        account_generation: CloudAccountGeneration::new(4),
    };
    let items = (0..2)
        .map(|index| CloudLibraryItem {
            remote_clip_id: format!("remote-{index}"),
            local_clip_id: None,
            path: String::new(),
            title: format!("Cloud {index}"),
            remote_url: format!("https://example.invalid/clips/{index}"),
            visibility: "private".into(),
            upload_status: "ready".into(),
            updated_at_unix: 1,
            uploaded_at_unix: Some(1),
            duration_ms: Some(7_000),
            file_size_bytes: Some(1_024),
            source_type: Some("recording".into()),
        })
        .collect::<Vec<_>>();
    let selected = Vec::new();
    let posters = BTreeMap::new();
    let projection = build_catalog_projection(
        &CatalogProjectionInput {
            revision: CatalogRevision::new(2),
            source: CatalogProjectionSource::Cloud {
                owner: &owner,
                page: CloudPageNumber::new(3).unwrap(),
                items: &items,
                has_next: false,
            },
            gallery: &GalleryPresentation::default(),
            selected: &selected,
            active: None,
            posters: &posters,
            menu: None,
            dialog: None,
            uploads: &[],
            load_state: CatalogLoadState::Ready,
        },
        &TestDays,
        &SystemProjectionReservation,
    )
    .unwrap();
    assert_eq!(projection.page.page, 3);
    assert_eq!(projection.page.page_count, None);
    assert_eq!(projection.page.total, None);
    assert_eq!(projection.page.range_text, "121–122");
    assert!(projection.page.has_previous);
    assert!(!projection.page.has_next);
    assert!(matches!(
        &projection.rows[0].identity,
        CatalogItemIdentity::Cloud { account_generation, .. }
            if *account_generation == CloudAccountGeneration::new(4)
    ));
}

#[test]
fn cloud_projection_rejects_selection_wrong_source_and_wrong_account_owners() {
    let owner = CloudCatalogOwner {
        account_key: CloudAccountKey::new("account-a").unwrap(),
        account_generation: CloudAccountGeneration::new(4),
    };
    let item = CloudLibraryItem {
        remote_clip_id: "remote-1".into(),
        local_clip_id: None,
        path: String::new(),
        title: "Cloud clip".into(),
        remote_url: "https://example.invalid/clips/1".into(),
        visibility: "private".into(),
        upload_status: "ready".into(),
        updated_at_unix: 1,
        uploaded_at_unix: Some(1),
        duration_ms: Some(1_000),
        file_size_bytes: Some(1),
        source_type: None,
    };
    let local_identity = CatalogItemIdentity::Local {
        path: local_clip(1).path_identity().unwrap(),
    };
    let wrong_cloud_identity = CatalogItemIdentity::Cloud {
        account_key: CloudAccountKey::new("account-b").unwrap(),
        account_generation: owner.account_generation,
        remote_clip_id: clipline_library::RemoteClipId::new("remote-1").unwrap(),
    };
    let selected = vec![local_identity.clone()];
    let posters = BTreeMap::new();
    let gallery = GalleryPresentation::default();
    let build = |selected: &[CatalogItemIdentity],
                 active: Option<&CatalogItemIdentity>,
                 menu: Option<&CatalogMenuProjection>,
                 dialog: Option<&CatalogDialogProjection>| {
        build_catalog_projection(
            &CatalogProjectionInput {
                revision: CatalogRevision::new(1),
                source: CatalogProjectionSource::Cloud {
                    owner: &owner,
                    page: CloudPageNumber::new(1).unwrap(),
                    items: std::slice::from_ref(&item),
                    has_next: false,
                },
                gallery: &gallery,
                selected,
                active,
                posters: &posters,
                menu,
                dialog,
                uploads: &[],
                load_state: CatalogLoadState::Ready,
            },
            &TestDays,
            &SystemProjectionReservation,
        )
    };
    assert!(matches!(
        build(&selected, None, None, None),
        Err(PresentationError::Invalid {
            field: "projection.cloud_selection"
        })
    ));
    assert!(matches!(
        build(&[], Some(&local_identity), None, None),
        Err(PresentationError::Invalid {
            field: "projection.identity_source"
        })
    ));
    assert!(matches!(
        build(&[], Some(&wrong_cloud_identity), None, None),
        Err(PresentationError::Invalid {
            field: "projection.cloud_identity_owner"
        })
    ));

    let wrong_menu = CatalogMenuProjection {
        target: wrong_cloud_identity.clone(),
        can_review: true,
        can_rename: false,
        can_delete: false,
        can_upload: false,
        can_reveal: false,
        can_open_browser: true,
        can_copy_link: true,
    };
    assert!(matches!(
        build(&[], None, Some(&wrong_menu), None),
        Err(PresentationError::Invalid {
            field: "projection.cloud_identity_owner"
        })
    ));
    let wrong_dialog = CatalogDialogProjection {
        kind: CatalogDialogKind::Delete,
        target: wrong_cloud_identity,
        title: "Delete?".into(),
        message: String::new(),
        confirm_label: "Delete".into(),
        destructive: true,
        text_value: None,
        description: None,
        visibility: None,
        audio_tracks: Vec::new(),
        delete_local_after_upload: false,
    };
    assert!(matches!(
        build(&[], None, None, Some(&wrong_dialog)),
        Err(PresentationError::Invalid {
            field: "projection.cloud_identity_owner"
        })
    ));
}

#[test]
fn empty_loading_error_and_disconnected_local_states_keep_an_explicit_zero_range() {
    let items = Vec::new();
    let options = LocalGalleryOptions::default();
    let selected = Vec::new();
    let posters = BTreeMap::new();
    let gallery = GalleryPresentation::default();
    for state in [
        CatalogLoadState::Empty,
        CatalogLoadState::Loading,
        CatalogLoadState::Ready,
        CatalogLoadState::Disconnected,
        CatalogLoadState::Error {
            message: "refresh failed".into(),
        },
    ] {
        let projection = build_catalog_projection(
            &CatalogProjectionInput {
                revision: CatalogRevision::new(1),
                source: CatalogProjectionSource::Local {
                    items: &items,
                    options: &options,
                    page: LocalPageIndex::new(0).unwrap(),
                },
                gallery: &gallery,
                selected: &selected,
                active: None,
                posters: &posters,
                menu: None,
                dialog: None,
                uploads: &[],
                load_state: state.clone(),
            },
            &TestDays,
            &SystemProjectionReservation,
        )
        .unwrap();
        assert_eq!(projection.load_state, state);
        assert_eq!(projection.page.range_text, "0 of 0");
        assert_eq!((projection.page.start, projection.page.end), (0, 0));
        assert!(projection.rows.is_empty());
    }
}

#[test]
fn an_empty_nonfirst_cloud_page_is_not_projected_as_accepted_data() {
    let owner = CloudCatalogOwner {
        account_key: CloudAccountKey::new("account-a").unwrap(),
        account_generation: CloudAccountGeneration::new(1),
    };
    let selected = Vec::new();
    let posters = BTreeMap::new();
    assert!(matches!(
        build_catalog_projection(
            &CatalogProjectionInput {
                revision: CatalogRevision::new(1),
                source: CatalogProjectionSource::Cloud {
                    owner: &owner,
                    page: CloudPageNumber::new(2).unwrap(),
                    items: &[],
                    has_next: false,
                },
                gallery: &GalleryPresentation::default(),
                selected: &selected,
                active: None,
                posters: &posters,
                menu: None,
                dialog: None,
                uploads: &[],
                load_state: CatalogLoadState::Ready,
            },
            &TestDays,
            &SystemProjectionReservation,
        ),
        Err(PresentationError::Invalid {
            field: "projection.cloud_empty_nonfirst"
        })
    ));
}

#[test]
fn disconnected_cloud_projection_is_empty_and_rejects_owned_targets() {
    let selected = Vec::new();
    let posters = BTreeMap::new();
    let projection = build_catalog_projection(
        &CatalogProjectionInput {
            revision: CatalogRevision::new(1),
            source: CatalogProjectionSource::CloudDisconnected,
            gallery: &GalleryPresentation::default(),
            selected: &selected,
            active: None,
            posters: &posters,
            menu: None,
            dialog: None,
            uploads: &[],
            load_state: CatalogLoadState::Disconnected,
        },
        &TestDays,
        &SystemProjectionReservation,
    )
    .unwrap();
    assert_eq!(projection.source, clipline_library::CatalogSource::Cloud);
    assert_eq!(projection.load_state, CatalogLoadState::Disconnected);
    assert!(projection.rows.is_empty());
    assert_eq!(projection.page.range_text, "0");

    let owner = CloudCatalogOwner {
        account_key: CloudAccountKey::new("account-a").unwrap(),
        account_generation: CloudAccountGeneration::new(1),
    };
    let target = CatalogItemIdentity::Cloud {
        account_key: owner.account_key,
        account_generation: owner.account_generation,
        remote_clip_id: clipline_library::RemoteClipId::new("remote-a").unwrap(),
    };
    assert!(matches!(
        build_catalog_projection(
            &CatalogProjectionInput {
                revision: CatalogRevision::new(2),
                source: CatalogProjectionSource::CloudDisconnected,
                gallery: &GalleryPresentation::default(),
                selected: &selected,
                active: Some(&target),
                posters: &posters,
                menu: None,
                dialog: None,
                uploads: &[],
                load_state: CatalogLoadState::Disconnected,
            },
            &TestDays,
            &SystemProjectionReservation,
        ),
        Err(PresentationError::Invalid {
            field: "projection.disconnected_cloud_target"
        })
    ));
}

#[test]
fn projection_rejects_invalid_bounds_and_injected_reservation_failure_atomically() {
    let mut invalid = local_clip(0);
    invalid.path.clear();
    assert!(matches!(
        local_projection(&[invalid], 0, &SystemProjectionReservation),
        Err(PresentationError::Invalid {
            field: "local.path_identity"
        })
    ));

    let item = local_clip(1);
    let identity = CatalogItemIdentity::Local {
        path: item.path_identity().unwrap(),
    };
    let duplicate_selection = vec![identity.clone(), identity];
    let options = LocalGalleryOptions::default();
    let posters = BTreeMap::new();
    assert!(matches!(
        build_catalog_projection(
            &CatalogProjectionInput {
                revision: CatalogRevision::new(1),
                source: CatalogProjectionSource::Local {
                    items: std::slice::from_ref(&item),
                    options: &options,
                    page: LocalPageIndex::new(0).unwrap(),
                },
                gallery: &GalleryPresentation::default(),
                selected: &duplicate_selection,
                active: None,
                posters: &posters,
                menu: None,
                dialog: None,
                uploads: &[],
                load_state: CatalogLoadState::Ready,
            },
            &TestDays,
            &SystemProjectionReservation,
        ),
        Err(PresentationError::Invalid {
            field: "projection.selected_order"
        })
    ));

    let items = (0..100).map(local_clip).collect::<Vec<_>>();
    for field in [
        "projection.local_index",
        "projection.rows",
        "projection.groups",
    ] {
        assert_eq!(
            local_projection(&items, 0, &FailingProjectionReservation::new(field)),
            Err(PresentationError::Allocation { field })
        );
    }

    let over_cap = vec![local_clip(0); MAX_LOCAL_INDEX_ROWS + 1];
    assert!(matches!(
        local_projection(&over_cap, 0, &SystemProjectionReservation),
        Err(PresentationError::TooLarge {
            field: "projection.local_items",
            actual,
            maximum: MAX_LOCAL_INDEX_ROWS,
        }) if actual == MAX_LOCAL_INDEX_ROWS + 1
    ));
}
