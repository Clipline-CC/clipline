use clipline_games::discovery::{DetectedGameCandidate, DetectedGameSource};
use clipline_games::identity::{
    GameItemIdentity, GameWindowIdentityCatalog, InstalledGameIdentityCatalog,
    LEAGUE_OF_LEGENDS_ID, OSU_ID,
};
use clipline_games::plugin::GamePluginInfo;
use clipline_games::presentation::{
    game_page_window, GameCandidateCatalog, GameCatalog, GameCatalogInput, GamePageIndex,
    GamePageOutcome, GamePageWindow, GamePresentationError, GameProjectionReservation, GameRowKind,
    ResolvedGameCatalogMember, SystemGameProjectionReservation, MAX_GAME_CATALOG_ROWS,
    MAX_GAME_PAGE_ROWS, MAX_GAME_ROW_TEXT_BYTES,
};
use clipline_settings::{
    CustomGameSettings, GamePluginPreference, GamePluginReviewSettings, GamePluginSettings,
    GamePreferences, GameRecordingMode, ProbeKind, ProbeRequestGeneration, ProbeSessionOwner,
    ProbeToken, SettingsAttachmentGeneration, SettingsForegroundGeneration,
    SettingsSessionGeneration,
};

fn owner() -> ProbeSessionOwner {
    ProbeSessionOwner::new(
        SettingsSessionGeneration::new(2),
        SettingsAttachmentGeneration::new(3),
        SettingsForegroundGeneration::new(5),
    )
}

fn token(kind: ProbeKind, generation: u64) -> ProbeToken {
    ProbeToken {
        owner: owner(),
        kind,
        request_generation: ProbeRequestGeneration::new(generation),
    }
}

fn plugin(id: &str, name: &str) -> GamePluginInfo {
    GamePluginInfo {
        id: id.into(),
        name: name.into(),
        summary: format!("{name} support"),
        default_enabled: true,
        default_recording_mode: GameRecordingMode::FullSession,
        default_review: GamePluginReviewSettings::default(),
        event_markers: false,
        presentation: None,
        icon: None,
    }
}

fn custom(index: usize) -> CustomGameSettings {
    CustomGameSettings {
        id: format!("custom-game-{index}"),
        legacy_ids: Vec::new(),
        name: format!("Custom {index}"),
        enabled: index.is_multiple_of(2),
        exe_name: format!("custom-{index}.exe"),
        process_path: Some(format!(r"C:\Games\Custom-{index}\custom-{index}.exe")),
        window_title: format!("Custom Window {index}"),
        recording_mode: GameRecordingMode::ReplaysOnly,
        icon: None,
    }
}

fn candidate(index: usize) -> DetectedGameCandidate {
    DetectedGameCandidate {
        id_hint: format!("steam-{index}"),
        name: format!("Candidate {index}"),
        source: DetectedGameSource::Steam,
        steam_app_id: Some(1_000 + index as u32),
        install_dir: Some(format!(r"C:\Steam\Candidate-{index}")),
        exe_name: format!("candidate-{index}.exe"),
        process_path: Some(format!(r"C:\Steam\Candidate-{index}\candidate-{index}.exe")),
        window_title: String::new(),
        icon: None,
        confidence: 75,
    }
}

fn preferences(customs: usize) -> GamePreferences {
    GamePreferences {
        auto_detect: true,
        pause_when_no_game: false,
        plugins: Vec::new(),
        custom_games: (0..customs).map(custom).collect(),
    }
}

fn plugin_preference(
    id: &str,
    enabled: bool,
    recording_mode: GameRecordingMode,
) -> GamePluginPreference {
    GamePluginPreference {
        id: id.into(),
        settings: GamePluginSettings {
            enabled,
            recording_mode,
            review: GamePluginReviewSettings::default(),
        },
    }
}

fn catalog(customs: usize, candidates: usize) -> GameCatalog {
    let settings = preferences(customs);
    let candidates = InstalledGameIdentityCatalog::build(
        token(ProbeKind::InstalledGames, 9),
        (0..candidates).map(candidate).collect(),
    )
    .unwrap();
    GameCatalog::try_build(
        GameCatalogInput {
            owner: owner(),
            plugins_token: token(ProbeKind::GamePlugins, 7),
            plugins: Vec::new(),
            settings,
            candidates: Some(GameCandidateCatalog::Installed(candidates)),
        },
        &SystemGameProjectionReservation,
    )
    .unwrap()
}

fn page(catalog: &GameCatalog, index: u32) -> GamePageOutcome {
    catalog
        .project_page(
            GamePageIndex::new(index),
            &[],
            |_| clipline_games::icon::GameIconLoadState::Missing,
            &SystemGameProjectionReservation,
        )
        .unwrap()
}

#[test]
fn stable_order_exact_membership_and_current_draft_dedupe_hold() {
    let mut settings = preferences(2);
    // UI-owned plugin preferences have their own stable lexical order. The
    // catalog must look them up by id without changing either that order or
    // the registry order used by projected rows.
    settings.plugins = vec![
        plugin_preference(OSU_ID, false, GameRecordingMode::ReplaysOnly),
        plugin_preference(LEAGUE_OF_LEGENDS_ID, true, GameRecordingMode::FullSession),
    ];
    let mut duplicates_custom = candidate(10);
    duplicates_custom.exe_name = settings.custom_games[1].exe_name.clone();
    duplicates_custom.process_path = settings.custom_games[1].process_path.clone();
    let retained = candidate(11);
    let candidates = InstalledGameIdentityCatalog::build(
        token(ProbeKind::InstalledGames, 12),
        vec![duplicates_custom, retained.clone()],
    )
    .unwrap();
    let catalog = GameCatalog::try_build(
        GameCatalogInput {
            owner: owner(),
            plugins_token: token(ProbeKind::GamePlugins, 11),
            plugins: vec![
                plugin(LEAGUE_OF_LEGENDS_ID, "League"),
                plugin(OSU_ID, "osu!"),
            ],
            settings,
            candidates: Some(GameCandidateCatalog::Installed(candidates)),
        },
        &SystemGameProjectionReservation,
    )
    .unwrap();

    assert_eq!(catalog.len(), 5);
    assert_eq!(catalog.plugins_token(), token(ProbeKind::GamePlugins, 11));
    assert_eq!(
        catalog.candidate_token(),
        Some(token(ProbeKind::InstalledGames, 12))
    );
    let GamePageOutcome::Page(page) = page(&catalog, 0) else {
        panic!("first page must exist")
    };
    assert_eq!(
        page.rows.iter().map(|row| row.kind).collect::<Vec<_>>(),
        vec![
            GameRowKind::Plugin,
            GameRowKind::Plugin,
            GameRowKind::Custom,
            GameRowKind::Custom,
            GameRowKind::InstalledCandidate,
        ]
    );
    assert_eq!(page.rows[1].enabled, Some(false));
    assert_eq!(page.rows.last().unwrap().title, retained.name);
    assert!(page.rows.iter().all(|row| {
        !row.subtitle.contains("C:\\") && !row.title.contains("data:image") && row.icon_id.is_none()
    }));

    assert!(matches!(
        catalog.resolve(&page.rows[0].identity),
        Some(ResolvedGameCatalogMember::Plugin(plugin)) if plugin.id == LEAGUE_OF_LEGENDS_ID
    ));
    assert!(matches!(
        catalog.resolve(&page.rows[2].identity),
        Some(ResolvedGameCatalogMember::Custom(game)) if game.id == "custom-game-0"
    ));
    assert!(matches!(
        catalog.resolve(&page.rows[4].identity),
        Some(ResolvedGameCatalogMember::InstalledCandidate(game)) if game.id_hint == retained.id_hint
    ));
}

#[test]
fn rows_publish_only_typed_icon_ids_and_bounded_load_state() {
    let mut league = plugin(LEAGUE_OF_LEGENDS_ID, "League");
    league.icon = Some("data:image/png;base64,private-payload".into());
    let catalog = GameCatalog::try_build(
        GameCatalogInput {
            owner: owner(),
            plugins_token: token(ProbeKind::GamePlugins, 30),
            plugins: vec![league],
            settings: GamePreferences::default(),
            candidates: None,
        },
        &SystemGameProjectionReservation,
    )
    .unwrap();
    let GamePageOutcome::Page(page) = catalog
        .project_page(
            GamePageIndex::new(0),
            &[],
            |_| clipline_games::icon::GameIconLoadState::Loading,
            &SystemGameProjectionReservation,
        )
        .unwrap()
    else {
        panic!("page zero must exist")
    };
    let row = &page.rows[0];
    let icon = row.icon_id.as_ref().expect("typed icon identity");
    assert_eq!(icon.owner(), owner());
    assert_eq!(icon.item(), &row.identity);
    assert_eq!(
        row.icon_state,
        clipline_games::icon::GameIconLoadState::Loading
    );
    assert!(!row.title.contains("private-payload"));
    assert!(!row.subtitle.contains("private-payload"));
}

#[test]
fn running_window_catalog_uses_its_real_sources_and_shared_custom_dedupe() {
    let existing = custom(2);
    let windows = GameWindowIdentityCatalog::build(
        token(ProbeKind::GameWindows, 20),
        vec![
            clipline_games::detection::GameWindowInfo {
                title: "Already configured".into(),
                process_id: 12,
                exe_name: existing.exe_name.clone(),
                exe_path: existing.process_path.clone(),
            },
            clipline_games::detection::GameWindowInfo {
                title: "New running game".into(),
                process_id: 13,
                exe_name: "new-game.exe".into(),
                exe_path: Some(r"C:\Games\New\new-game.exe".into()),
            },
        ],
    )
    .unwrap();
    let catalog = GameCatalog::try_build(
        GameCatalogInput {
            owner: owner(),
            plugins_token: token(ProbeKind::GamePlugins, 19),
            plugins: Vec::new(),
            settings: GamePreferences {
                custom_games: vec![existing],
                ..GamePreferences::default()
            },
            candidates: Some(GameCandidateCatalog::RunningWindows(windows)),
        },
        &SystemGameProjectionReservation,
    )
    .unwrap();
    assert_eq!(catalog.len(), 2);
    let candidate_identity = catalog.identities().nth(1).unwrap();
    assert!(matches!(
        catalog.resolve(candidate_identity),
        Some(ResolvedGameCatalogMember::RunningWindow(window))
            if window.process_id == 13 && window.title == "New running game"
    ));
    let GamePageOutcome::Page(page) = page(&catalog, 0) else {
        panic!("page zero must exist")
    };
    assert_eq!(page.rows[1].kind, GameRowKind::RunningWindow);
    assert!(!page.rows[1].subtitle.contains("C:\\Games"));
}

#[test]
fn page_math_is_exact_at_every_pinned_boundary() {
    for (total, expected_pages, last_size) in [
        (0, 0, 0),
        (60, 1, 60),
        (61, 2, 1),
        (128, 3, 8),
        (256, 5, 16),
        (400, 7, 40),
    ] {
        assert!(total <= MAX_GAME_CATALOG_ROWS);
        if total == 0 {
            assert_eq!(
                game_page_window(total, GamePageIndex::new(0)),
                GamePageWindow::Page {
                    page_count: 0,
                    start: 0,
                    end: 0,
                }
            );
        } else {
            let last = expected_pages - 1;
            assert_eq!(
                game_page_window(total, GamePageIndex::new(last as u32)),
                GamePageWindow::Page {
                    page_count: expected_pages,
                    start: last * MAX_GAME_PAGE_ROWS,
                    end: total,
                }
            );
            assert_eq!(total - last * MAX_GAME_PAGE_ROWS, last_size);
        }
        let first_past_end = if total == 0 { 1 } else { expected_pages };
        assert_eq!(
            game_page_window(total, GamePageIndex::new(first_past_end as u32)),
            GamePageWindow::PastEnd {
                page_count: expected_pages,
                fallback_page: expected_pages
                    .checked_sub(1)
                    .map(|page| GamePageIndex::new(page as u32)),
            }
        );
    }
}

#[test]
fn real_catalog_pages_cover_rows_once_and_past_end_is_not_clamped() {
    let catalog = catalog(0, 256);
    let mut identities = Vec::new();
    for index in 0..5 {
        let GamePageOutcome::Page(page) = page(&catalog, index) else {
            panic!("expected page {index}")
        };
        assert!(page.rows.len() <= MAX_GAME_PAGE_ROWS);
        identities.extend(page.rows.into_iter().map(|row| row.identity));
    }
    assert_eq!(identities.len(), 256);
    let mut sorted = identities.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), identities.len());

    assert!(matches!(
        page(&catalog, 5),
        GamePageOutcome::PastEnd {
            requested_page,
            fallback_page: Some(fallback),
            total: 256,
            page_count: 5,
            ..
        } if requested_page.get() == 5 && fallback.get() == 4
    ));
    assert!(matches!(
        page(&catalog, u32::MAX),
        GamePageOutcome::PastEnd {
            requested_page,
            fallback_page: Some(fallback),
            ..
        } if requested_page.get() == u32::MAX && fallback.get() == 4
    ));
}

#[test]
fn empty_page_zero_is_valid_and_later_empty_pages_are_past_end() {
    let catalog = catalog(0, 0);
    assert!(matches!(
        page(&catalog, 0),
        GamePageOutcome::Page(ref page)
            if page.rows.is_empty() && page.total == 0 && page.page_count == 0
    ));
    assert!(matches!(
        page(&catalog, 1),
        GamePageOutcome::PastEnd {
            fallback_page: None,
            total: 0,
            page_count: 0,
            ..
        }
    ));
}

#[test]
fn wrong_tokens_duplicate_configured_ids_and_wrong_plugin_order_fail_closed() {
    let wrong_kind = GameCatalog::try_build(
        GameCatalogInput {
            owner: owner(),
            plugins_token: token(ProbeKind::InstalledGames, 1),
            plugins: Vec::new(),
            settings: GamePreferences::default(),
            candidates: None,
        },
        &SystemGameProjectionReservation,
    )
    .unwrap_err();
    assert!(matches!(
        wrong_kind,
        GamePresentationError::WrongProbeKind {
            expected: ProbeKind::GamePlugins,
            actual: ProbeKind::InstalledGames
        }
    ));

    let foreign_owner = ProbeSessionOwner::new(
        SettingsSessionGeneration::new(99),
        SettingsAttachmentGeneration::new(3),
        SettingsForegroundGeneration::new(5),
    );
    let foreign_token = ProbeToken {
        owner: foreign_owner,
        kind: ProbeKind::GamePlugins,
        request_generation: ProbeRequestGeneration::new(1),
    };
    assert_eq!(
        GameCatalog::try_build(
            GameCatalogInput {
                owner: owner(),
                plugins_token: foreign_token,
                plugins: Vec::new(),
                settings: GamePreferences::default(),
                candidates: None,
            },
            &SystemGameProjectionReservation,
        )
        .unwrap_err(),
        GamePresentationError::OwnerMismatch
    );

    let reverse_plugins = GameCatalog::try_build(
        GameCatalogInput {
            owner: owner(),
            plugins_token: token(ProbeKind::GamePlugins, 1),
            plugins: vec![
                plugin(OSU_ID, "osu!"),
                plugin(LEAGUE_OF_LEGENDS_ID, "League"),
            ],
            settings: GamePreferences::default(),
            candidates: None,
        },
        &SystemGameProjectionReservation,
    )
    .unwrap_err();
    assert_eq!(reverse_plugins, GamePresentationError::PluginOrder);

    let duplicate = custom(1);
    let duplicate_custom = GameCatalog::try_build(
        GameCatalogInput {
            owner: owner(),
            plugins_token: token(ProbeKind::GamePlugins, 1),
            plugins: Vec::new(),
            settings: GamePreferences {
                custom_games: vec![duplicate.clone(), duplicate],
                ..GamePreferences::default()
            },
            candidates: None,
        },
        &SystemGameProjectionReservation,
    )
    .unwrap_err();
    assert_eq!(duplicate_custom, GamePresentationError::DuplicateIdentity);
}

#[test]
fn forged_candidate_handle_cannot_resolve_or_enter_selection() {
    let catalog = catalog(0, 1);
    let identity = catalog.identities().next().unwrap();
    let mut forged = serde_json::to_value(identity).unwrap();
    let opaque = forged["value"]["opaque_id"].as_str().unwrap();
    let replacement = if opaque.ends_with('0') { '1' } else { '0' };
    let mut changed = opaque[..opaque.len() - 1].to_owned();
    changed.push(replacement);
    forged["value"]["opaque_id"] = changed.into();
    let forged: GameItemIdentity = serde_json::from_value(forged).unwrap();
    assert!(catalog.resolve(&forged).is_none());
    assert_eq!(
        catalog
            .project_page(
                GamePageIndex::new(0),
                &[forged],
                |_| clipline_games::icon::GameIconLoadState::Missing,
                &SystemGameProjectionReservation,
            )
            .unwrap_err(),
        GamePresentationError::InvalidSelection
    );
}

#[test]
fn candidate_selection_is_exact_sorted_unique_and_candidate_only() {
    let catalog = catalog(1, 3);
    let custom = catalog.identities().next().unwrap().clone();
    let mut candidates = catalog.identities().skip(1).cloned().collect::<Vec<_>>();
    candidates.sort();
    let selected = candidates[..2].to_vec();

    let GamePageOutcome::Page(page) = catalog
        .project_page(
            GamePageIndex::new(0),
            &selected,
            |_| clipline_games::icon::GameIconLoadState::Missing,
            &SystemGameProjectionReservation,
        )
        .unwrap()
    else {
        panic!("page zero must exist")
    };
    assert_eq!(page.rows.iter().filter(|row| row.selected).count(), 2);
    assert!(page.rows.iter().filter(|row| row.selected).all(|row| {
        matches!(row.identity, GameItemIdentity::Candidate(_))
            && selected.binary_search(&row.identity).is_ok()
    }));

    let duplicate = vec![selected[0].clone(), selected[0].clone()];
    assert_eq!(
        catalog
            .project_page(
                GamePageIndex::new(0),
                &duplicate,
                |_| clipline_games::icon::GameIconLoadState::Missing,
                &SystemGameProjectionReservation,
            )
            .unwrap_err(),
        GamePresentationError::InvalidSelection
    );

    let mut reversed = selected.clone();
    reversed.reverse();
    assert_eq!(
        catalog
            .project_page(
                GamePageIndex::new(0),
                &reversed,
                |_| clipline_games::icon::GameIconLoadState::Missing,
                &SystemGameProjectionReservation,
            )
            .unwrap_err(),
        GamePresentationError::InvalidSelection
    );
    assert_eq!(
        catalog
            .project_page(
                GamePageIndex::new(0),
                &[custom],
                |_| clipline_games::icon::GameIconLoadState::Missing,
                &SystemGameProjectionReservation,
            )
            .unwrap_err(),
        GamePresentationError::InvalidSelection
    );
}

struct FailReservation(&'static str);

impl GameProjectionReservation for FailReservation {
    fn before_reserve(
        &self,
        field: &'static str,
        _additional: usize,
    ) -> Result<(), GamePresentationError> {
        if field == self.0 {
            Err(GamePresentationError::Allocation { field })
        } else {
            Ok(())
        }
    }
}

#[test]
fn projection_reservations_fail_before_partial_catalog_or_page_publication() {
    let candidates = InstalledGameIdentityCatalog::build(
        token(ProbeKind::InstalledGames, 4),
        vec![candidate(1)],
    )
    .unwrap();
    let error = GameCatalog::try_build(
        GameCatalogInput {
            owner: owner(),
            plugins_token: token(ProbeKind::GamePlugins, 3),
            plugins: Vec::new(),
            settings: GamePreferences::default(),
            candidates: Some(GameCandidateCatalog::Installed(candidates)),
        },
        &FailReservation("games.catalog_members"),
    )
    .unwrap_err();
    assert_eq!(
        error,
        GamePresentationError::Allocation {
            field: "games.catalog_members"
        }
    );

    let catalog = catalog(0, 1);
    assert_eq!(
        catalog
            .project_page(
                GamePageIndex::new(0),
                &[],
                |_| clipline_games::icon::GameIconLoadState::Missing,
                &FailReservation("games.page_rows"),
            )
            .unwrap_err(),
        GamePresentationError::Allocation {
            field: "games.page_rows"
        }
    );
}

#[test]
fn hostile_display_text_is_utf8_bounded_without_leaking_source_paths() {
    let mut game = custom(1);
    game.name = "é".repeat(MAX_GAME_ROW_TEXT_BYTES);
    game.exe_name = "x".repeat(MAX_GAME_ROW_TEXT_BYTES);
    game.window_title = "y".repeat(MAX_GAME_ROW_TEXT_BYTES);
    let catalog = GameCatalog::try_build(
        GameCatalogInput {
            owner: owner(),
            plugins_token: token(ProbeKind::GamePlugins, 1),
            plugins: Vec::new(),
            settings: GamePreferences {
                custom_games: vec![game],
                ..GamePreferences::default()
            },
            candidates: None,
        },
        &SystemGameProjectionReservation,
    )
    .unwrap();
    let GamePageOutcome::Page(page) = page(&catalog, 0) else {
        panic!("page zero must exist")
    };
    let row = &page.rows[0];
    assert_eq!(row.title.len(), MAX_GAME_ROW_TEXT_BYTES);
    assert!(row.title.is_char_boundary(row.title.len()));
    assert!(row.subtitle.len() <= MAX_GAME_ROW_TEXT_BYTES);
    assert!(!row.subtitle.contains("C:\\Games"));
}

#[test]
fn into_input_returns_exact_owned_buffers_tokens_and_candidate_authority() {
    let plugins_token = token(ProbeKind::GamePlugins, 41);
    let candidates_token = token(ProbeKind::InstalledGames, 42);
    let plugins = vec![
        plugin(LEAGUE_OF_LEGENDS_ID, "League"),
        plugin(OSU_ID, "osu!"),
    ];
    let settings = GamePreferences {
        auto_detect: false,
        pause_when_no_game: true,
        // This is the normalized SettingsPreferences lexical order, which is
        // intentionally independent from registry presentation order.
        plugins: vec![
            plugin_preference(LEAGUE_OF_LEGENDS_ID, false, GameRecordingMode::ReplaysOnly),
            plugin_preference(OSU_ID, true, GameRecordingMode::FullSession),
        ],
        custom_games: vec![custom(7)],
    };
    let candidates =
        InstalledGameIdentityCatalog::build(candidates_token, vec![candidate(9)]).unwrap();

    let plugin_buffer = plugins.as_ptr();
    let preference_buffer = settings.plugins.as_ptr();
    let custom_buffer = settings.custom_games.as_ptr();
    let candidate_identity = candidates.identity_at(0).unwrap().clone();
    let candidate_identity_address = candidates.identity_at(0).unwrap() as *const _;
    let candidate_source_address = candidates.source_at(0).unwrap() as *const _;

    let catalog = GameCatalog::try_build(
        GameCatalogInput {
            owner: owner(),
            plugins_token,
            plugins,
            settings,
            candidates: Some(GameCandidateCatalog::Installed(candidates)),
        },
        &SystemGameProjectionReservation,
    )
    .unwrap();

    let input = catalog.into_input();
    assert_eq!(input.owner, owner());
    assert_eq!(input.plugins_token, plugins_token);
    assert_eq!(input.plugins.as_ptr(), plugin_buffer);
    assert_eq!(input.settings.plugins.as_ptr(), preference_buffer);
    assert_eq!(input.settings.custom_games.as_ptr(), custom_buffer);
    assert_eq!(
        input
            .settings
            .plugins
            .iter()
            .map(|plugin| plugin.id.as_str())
            .collect::<Vec<_>>(),
        vec![LEAGUE_OF_LEGENDS_ID, OSU_ID]
    );
    let Some(GameCandidateCatalog::Installed(candidates)) = input.candidates.as_ref() else {
        panic!("installed candidate authority must be returned")
    };
    assert_eq!(candidates.token(), candidates_token);
    assert_eq!(
        candidates.identity_at(0).unwrap() as *const _,
        candidate_identity_address
    );
    assert_eq!(
        candidates.source_at(0).unwrap() as *const _,
        candidate_source_address
    );
    assert_eq!(
        candidates.resolve(&candidate_identity).unwrap().id_hint,
        "steam-9"
    );

    let rebuilt = GameCatalog::try_build(input, &SystemGameProjectionReservation).unwrap();
    assert_eq!(rebuilt.plugins_token(), plugins_token);
    assert_eq!(rebuilt.candidate_token(), Some(candidates_token));
    assert!(matches!(
        rebuilt.resolve(&GameItemIdentity::Candidate(candidate_identity)),
        Some(ResolvedGameCatalogMember::InstalledCandidate(source))
            if source.id_hint == "steam-9"
    ));
}
