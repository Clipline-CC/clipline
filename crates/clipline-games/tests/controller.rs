use clipline_games::channel::{GamesProbeCatalog, GamesProbeFailed, GamesProbeReady};
use clipline_games::controller::{
    GameCandidateSource, GamesController, GamesControllerError, GamesProbePhase,
    RejectedGamesProbeReady,
};
use clipline_games::discovery::{DetectedGameCandidate, DetectedGameSource};
use clipline_games::identity::{
    GameItemIdentity, GameWindowIdentityCatalog, InstalledGameIdentityCatalog,
    LEAGUE_OF_LEGENDS_ID, OSU_ID,
};
use clipline_games::plugin::GamePluginInfo;
use clipline_games::presentation::{
    GamePageIndex, GamePresentationError, GameProjectionReservation,
    SystemGameProjectionReservation,
};
use clipline_settings::{
    CustomGameSettings, GamePluginReviewSettings, GamePreferences, GameRecordingMode, ProbeKind,
    ProbeRequestGeneration, ProbeSessionOwner, ProbeToken, SettingsAttachmentGeneration,
    SettingsForegroundGeneration, SettingsSessionGeneration,
};

fn owner(session: u64) -> ProbeSessionOwner {
    ProbeSessionOwner::new(
        SettingsSessionGeneration::new(session),
        SettingsAttachmentGeneration::new(2),
        SettingsForegroundGeneration::new(3),
    )
}

fn token(owner: ProbeSessionOwner, kind: ProbeKind, generation: u64) -> ProbeToken {
    ProbeToken {
        owner,
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
        enabled: true,
        exe_name: format!("custom-{index}.exe"),
        process_path: Some(format!(r"C:\Games\Custom-{index}\custom-{index}.exe")),
        window_title: format!("Custom Window {index}"),
        recording_mode: GameRecordingMode::ReplaysOnly,
        icon: None,
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
        confidence: 80,
    }
}

fn plugins_ready(token: ProbeToken, plugins: Vec<GamePluginInfo>) -> GamesProbeReady {
    GamesProbeReady::new(token, GamesProbeCatalog::Plugins(plugins)).unwrap()
}

fn installed_ready(token: ProbeToken, rows: Vec<DetectedGameCandidate>) -> GamesProbeReady {
    GamesProbeReady::new(
        token,
        GamesProbeCatalog::Installed(InstalledGameIdentityCatalog::build(token, rows).unwrap()),
    )
    .unwrap()
}

fn windows_ready(token: ProbeToken, index: u32) -> GamesProbeReady {
    GamesProbeReady::new(
        token,
        GamesProbeCatalog::RunningWindows(
            GameWindowIdentityCatalog::build(
                token,
                vec![clipline_games::detection::GameWindowInfo {
                    title: format!("Window {index}"),
                    process_id: index,
                    exe_name: format!("window-{index}.exe"),
                    exe_path: Some(format!(r"C:\Games\Window-{index}\window-{index}.exe")),
                }],
            )
            .unwrap(),
        ),
    )
    .unwrap()
}

fn retry_ready(rejected: RejectedGamesProbeReady) -> GamesProbeReady {
    GamesProbeReady::new(rejected.token, rejected.catalog).unwrap()
}

fn accept_plugins(controller: &mut GamesController, owner: ProbeSessionOwner, generation: u64) {
    let token = token(owner, ProbeKind::GamePlugins, generation);
    controller.register_probe(token).unwrap();
    controller
        .accept_probe_ready(
            plugins_ready(token, vec![plugin(LEAGUE_OF_LEGENDS_ID, "League")]),
            &SystemGameProjectionReservation,
        )
        .unwrap();
}

struct FailMembers;

impl GameProjectionReservation for FailMembers {
    fn before_reserve(
        &self,
        field: &'static str,
        _additional: usize,
    ) -> Result<(), GamePresentationError> {
        if field == "games.catalog_members" {
            Err(GamePresentationError::Allocation { field })
        } else {
            Ok(())
        }
    }
}

#[test]
fn candidate_payload_can_arrive_before_plugins_without_clone_or_reenumeration() {
    let owner = owner(1);
    let mut controller = GamesController::new(owner, preferences(0));
    let installed = token(owner, ProbeKind::InstalledGames, 1);
    controller.register_probe(installed).unwrap();
    let update = controller
        .accept_probe_ready(installed_ready(installed, vec![candidate(1)]), &FailMembers)
        .unwrap();
    assert!(!update.summary.catalog_ready);

    accept_plugins(&mut controller, owner, 1);
    let summary = controller.summary();
    assert!(summary.catalog_ready);
    assert_eq!(summary.total, 2);
    let page = controller
        .project_current(
            |_| clipline_games::icon::GameIconLoadState::Missing,
            &SystemGameProjectionReservation,
        )
        .unwrap();
    assert_eq!(page.rows.len(), 2);
    assert!(matches!(
        page.rows[1].identity,
        GameItemIdentity::Candidate(_)
    ));
}

#[test]
fn only_the_exact_registered_pending_probe_can_publish_once() {
    let other_owner = owner(3);
    let owner = owner(2);
    let mut controller = GamesController::new(owner, preferences(0));
    let exact = token(owner, ProbeKind::GamePlugins, 2);
    assert_eq!(
        controller
            .register_probe(token(owner, ProbeKind::GamePlugins, 0))
            .unwrap_err(),
        GamesControllerError::StaleProbe
    );
    let rejected = controller
        .accept_probe_ready(
            plugins_ready(exact, Vec::new()),
            &SystemGameProjectionReservation,
        )
        .unwrap_err();
    assert_eq!(rejected.error, GamesControllerError::UnexpectedProbeResult);
    let initial = controller.summary();
    let registered = controller.register_probe(exact).unwrap();
    assert_eq!(registered.revision, initial.revision);
    assert!(registered.view_generation > initial.view_generation);
    assert_eq!(
        controller
            .register_probe(token(owner, ProbeKind::GamePlugins, 1))
            .unwrap_err(),
        GamesControllerError::StaleProbe
    );
    let failed = controller
        .accept_probe_failed(GamesProbeFailed::new(exact, "bounded failure".into()).unwrap())
        .unwrap();
    assert_eq!(failed.summary.revision, registered.revision);
    assert!(failed.summary.view_generation > registered.view_generation);
    assert_eq!(failed.summary.plugins.phase, GamesProbePhase::Failed);
    assert_eq!(
        controller.probe_failure(ProbeKind::GamePlugins).unwrap(),
        Some("bounded failure")
    );
    let rejected = controller
        .accept_probe_ready(
            plugins_ready(exact, Vec::new()),
            &SystemGameProjectionReservation,
        )
        .unwrap_err();
    assert_eq!(rejected.error, GamesControllerError::UnexpectedProbeResult);
    assert_eq!(
        controller
            .register_probe(token(other_owner, ProbeKind::GamePlugins, 3))
            .unwrap_err(),
        GamesControllerError::WrongOwner
    );
}

#[test]
fn failed_refresh_preserves_the_prior_catalog_and_initial_input_can_retry() {
    let owner = owner(4);
    let mut controller = GamesController::new(owner, preferences(1));
    let first = token(owner, ProbeKind::GamePlugins, 1);
    controller.register_probe(first).unwrap();
    let first_plugins = vec![plugin(LEAGUE_OF_LEGENDS_ID, "League")];
    let first_plugins_buffer = first_plugins.as_ptr();
    let rejected = controller
        .accept_probe_ready(plugins_ready(first, first_plugins), &FailMembers)
        .unwrap_err();
    assert!(matches!(
        rejected.error,
        GamesControllerError::Presentation(GamePresentationError::Allocation { .. })
    ));
    assert_eq!(controller.summary().plugins.phase, GamesProbePhase::Pending);
    let GamesProbeCatalog::Plugins(returned_plugins) = &rejected.catalog else {
        panic!("plugin replacement must return the exact plugin catalog")
    };
    assert_eq!(returned_plugins.as_ptr(), first_plugins_buffer);
    controller
        .accept_probe_ready(retry_ready(*rejected), &SystemGameProjectionReservation)
        .unwrap();
    let before = controller.summary();

    let replacement_settings = preferences(2);
    let replacement_buffer = replacement_settings.custom_games.as_ptr();
    let rejected = controller
        .replace_settings(
            controller.action_fence(),
            replacement_settings,
            &FailMembers,
        )
        .unwrap_err();
    assert_eq!(rejected.settings.custom_games.as_ptr(), replacement_buffer);
    assert!(matches!(
        rejected.error,
        GamesControllerError::Presentation(GamePresentationError::Allocation { .. })
    ));
    assert_eq!(controller.summary().revision, before.revision);

    let refresh = token(owner, ProbeKind::GamePlugins, 3);
    controller.register_probe(refresh).unwrap();
    let refresh_plugins = vec![plugin(OSU_ID, "osu!")];
    let refresh_plugins_buffer = refresh_plugins.as_ptr();
    let rejected = controller
        .accept_probe_ready(plugins_ready(refresh, refresh_plugins), &FailMembers)
        .unwrap_err();
    let GamesProbeCatalog::Plugins(returned_plugins) = &rejected.catalog else {
        panic!("plugin refresh must return the exact plugin catalog")
    };
    assert_eq!(returned_plugins.as_ptr(), refresh_plugins_buffer);
    let after = controller.summary();
    assert_eq!(after.total, before.total);
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.plugins.phase, GamesProbePhase::Pending);
    assert_eq!(
        controller
            .project_current(
                |_| clipline_games::icon::GameIconLoadState::Missing,
                &SystemGameProjectionReservation,
            )
            .unwrap()
            .rows[0]
            .title,
        "League"
    );
    controller
        .accept_probe_ready(retry_ready(*rejected), &SystemGameProjectionReservation)
        .unwrap();
    assert_eq!(
        controller
            .project_current(
                |_| clipline_games::icon::GameIconLoadState::Missing,
                &SystemGameProjectionReservation,
            )
            .unwrap()
            .rows[0]
            .title,
        "osu!"
    );

    let installed = token(owner, ProbeKind::InstalledGames, 1);
    controller.register_probe(installed).unwrap();
    let installed_catalog =
        InstalledGameIdentityCatalog::build(installed, vec![candidate(1)]).unwrap();
    let candidate_address = installed_catalog.source_at(0).unwrap() as *const _;
    let rejected = controller
        .accept_probe_ready(
            GamesProbeReady::new(installed, GamesProbeCatalog::Installed(installed_catalog))
                .unwrap(),
            &FailMembers,
        )
        .unwrap_err();
    let GamesProbeCatalog::Installed(returned_candidates) = &rejected.catalog else {
        panic!("candidate refresh must return the exact candidate catalog")
    };
    assert_eq!(
        returned_candidates.source_at(0).unwrap() as *const _,
        candidate_address
    );
    assert_eq!(
        controller.summary().installed.phase,
        GamesProbePhase::Pending
    );
    controller
        .accept_probe_ready(retry_ready(*rejected), &SystemGameProjectionReservation)
        .unwrap();
}

#[test]
fn only_refresh_of_the_active_candidate_authority_invalidates_selection() {
    let owner = owner(5);
    let mut controller = GamesController::new(owner, preferences(0));
    accept_plugins(&mut controller, owner, 1);

    let installed1 = token(owner, ProbeKind::InstalledGames, 1);
    controller.register_probe(installed1).unwrap();
    controller
        .accept_probe_ready(
            installed_ready(installed1, vec![candidate(1)]),
            &SystemGameProjectionReservation,
        )
        .unwrap();
    let identity = controller
        .project_current(
            |_| clipline_games::icon::GameIconLoadState::Missing,
            &SystemGameProjectionReservation,
        )
        .unwrap()
        .rows
        .into_iter()
        .find(|row| matches!(row.identity, GameItemIdentity::Candidate(_)))
        .unwrap()
        .identity;
    controller
        .replace_selection(controller.action_fence(), vec![identity])
        .unwrap();

    let windows1 = token(owner, ProbeKind::GameWindows, 1);
    controller.register_probe(windows1).unwrap();
    controller
        .accept_probe_ready(
            windows_ready(windows1, 10),
            &SystemGameProjectionReservation,
        )
        .unwrap();
    assert_eq!(controller.summary().selected_count, 1);

    let plugins2 = token(owner, ProbeKind::GamePlugins, 2);
    controller.register_probe(plugins2).unwrap();
    controller
        .accept_probe_ready(
            plugins_ready(plugins2, vec![plugin(LEAGUE_OF_LEGENDS_ID, "League 2")]),
            &SystemGameProjectionReservation,
        )
        .unwrap();
    assert_eq!(controller.summary().selected_count, 1);

    let installed2 = token(owner, ProbeKind::InstalledGames, 2);
    controller.register_probe(installed2).unwrap();
    controller
        .accept_probe_ready(
            installed_ready(installed2, vec![candidate(2)]),
            &SystemGameProjectionReservation,
        )
        .unwrap();
    assert_eq!(controller.summary().selected_count, 0);
}

#[test]
fn source_switch_moves_owned_catalogs_and_stale_action_fences_fail() {
    let owner = owner(6);
    let mut controller = GamesController::new(owner, preferences(0));
    accept_plugins(&mut controller, owner, 1);
    let installed = token(owner, ProbeKind::InstalledGames, 1);
    controller.register_probe(installed).unwrap();
    controller
        .accept_probe_ready(
            installed_ready(installed, vec![candidate(1)]),
            &SystemGameProjectionReservation,
        )
        .unwrap();
    let windows = token(owner, ProbeKind::GameWindows, 1);
    controller.register_probe(windows).unwrap();
    controller
        .accept_probe_ready(windows_ready(windows, 7), &SystemGameProjectionReservation)
        .unwrap();
    let stale = controller.action_fence();
    controller
        .set_candidate_source(
            stale,
            GameCandidateSource::RunningWindows,
            &SystemGameProjectionReservation,
        )
        .unwrap();
    assert_eq!(
        controller.summary().candidate_source,
        GameCandidateSource::RunningWindows
    );
    assert_eq!(
        controller
            .set_candidate_source(
                stale,
                GameCandidateSource::Installed,
                &SystemGameProjectionReservation,
            )
            .unwrap_err(),
        GamesControllerError::StaleAction
    );
    controller
        .set_candidate_source(
            controller.action_fence(),
            GameCandidateSource::Installed,
            &SystemGameProjectionReservation,
        )
        .unwrap();
    assert!(controller
        .project_current(
            |_| clipline_games::icon::GameIconLoadState::Missing,
            &SystemGameProjectionReservation,
        )
        .unwrap()
        .rows
        .iter()
        .any(|row| row.title == "Candidate 1"));
}

#[test]
fn page_and_selection_transitions_fail_closed_and_catalog_shrink_corrects_page() {
    let owner = owner(7);
    let mut controller = GamesController::new(owner, preferences(128));
    accept_plugins(&mut controller, owner, 1);
    let before_page = controller.summary();
    let page_update = controller
        .set_page(
            controller.action_fence(),
            GamePageIndex::new(2),
            &SystemGameProjectionReservation,
        )
        .unwrap();
    assert_eq!(page_update.summary.revision, before_page.revision);
    assert!(page_update.summary.view_generation > before_page.view_generation);
    let update = controller
        .replace_settings(
            controller.action_fence(),
            preferences(1),
            &SystemGameProjectionReservation,
        )
        .unwrap();
    assert!(update.page_corrected);
    assert_eq!(update.summary.page, GamePageIndex::new(0));
    assert_eq!(
        controller
            .set_page(
                controller.action_fence(),
                GamePageIndex::new(9),
                &SystemGameProjectionReservation,
            )
            .unwrap_err(),
        GamesControllerError::PastEnd {
            fallback: Some(GamePageIndex::new(0))
        }
    );
    let plugin_identity = controller
        .project_current(
            |_| clipline_games::icon::GameIconLoadState::Missing,
            &SystemGameProjectionReservation,
        )
        .unwrap()
        .rows[0]
        .identity
        .clone();
    assert_eq!(
        controller
            .replace_selection(controller.action_fence(), vec![plugin_identity])
            .unwrap_err(),
        GamesControllerError::InvalidSelection
    );
}

#[test]
fn detach_rejects_every_later_result_and_action() {
    let other_owner = owner(9);
    let owner = owner(8);
    let mut controller = GamesController::new(owner, preferences(0));
    accept_plugins(&mut controller, owner, 1);
    let fence = controller.action_fence();
    assert_eq!(
        controller.detach(other_owner).unwrap_err(),
        GamesControllerError::WrongOwner
    );
    let before = controller.summary();
    let detached = controller.detach(owner).unwrap();
    assert!(!detached.summary.attached);
    assert!(!detached.summary.catalog_ready);
    assert_eq!(detached.summary.total, 0);
    assert_eq!(detached.summary.page_count, 0);
    assert_eq!(detached.summary.page, GamePageIndex::new(0));
    assert_eq!(detached.summary.selected_count, 0);
    assert_eq!(detached.summary.plugins.phase, GamesProbePhase::Idle);
    assert_eq!(detached.summary.installed.phase, GamesProbePhase::Idle);
    assert_eq!(
        detached.summary.running_windows.phase,
        GamesProbePhase::Idle
    );
    assert!(detached.summary.revision > before.revision);
    assert!(detached.summary.view_generation > before.view_generation);
    assert_eq!(controller.summary(), detached.summary);
    assert_eq!(
        controller.replace_selection(fence, Vec::new()).unwrap_err(),
        GamesControllerError::Detached
    );
    assert_eq!(
        controller
            .register_probe(token(owner, ProbeKind::InstalledGames, 1))
            .unwrap_err(),
        GamesControllerError::Detached
    );
}
