use clipline_library::{
    gallery_card_preview, marker_digest, marker_style, AssetAlias, GalleryCardConfig,
    GalleryCardIconConfig, GalleryCardInput, GalleryCardStat, GalleryCardTitleFormat,
    GalleryCardTitlePolicy, GalleryMarker, GalleryPlay, GalleryPresentation, GallerySummaryMode,
    MarkerCategoryPresentation, MarkerKindPresentation, MarkerPresentation, PlayerCardSummary,
};

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
