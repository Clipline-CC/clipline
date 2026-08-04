use clipline_games::channel::{
    games_result_channel, GamesBarrier, GamesProbeCatalog, GamesProbeFailed, GamesProbeReady,
    GamesResult, GamesResultError, GamesResultPublishOutcome,
};
use clipline_games::discovery::{DetectedGameCandidate, DetectedGameSource};
use clipline_games::identity::InstalledGameIdentityCatalog;
use clipline_games::plugin::{
    catalog_bounded, MAX_GAME_PLUGINS, MAX_PLUGIN_CATALOG_BYTES, MAX_PLUGIN_ICON_BYTES,
    MAX_PLUGIN_ICON_DATA_URL_BYTES, MAX_PLUGIN_TEXT_BYTES,
};
use clipline_settings::{
    ProbeKind, ProbeRequestGeneration, ProbeSessionOwner, ProbeToken, SettingsAttachmentGeneration,
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

fn failed(owner: ProbeSessionOwner, generation: u64) -> GamesResult {
    GamesResult::ProbeFailed(
        GamesProbeFailed::new(
            token(owner, ProbeKind::InstalledGames, generation),
            "probe failed".into(),
        )
        .unwrap(),
    )
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

#[test]
fn ten_thousand_probe_results_coalesce_to_the_strictly_newest_generation() {
    let owner = owner(1);
    let (sender, receiver) = games_result_channel(owner);
    for generation in 1..=10_000 {
        let outcome = sender.try_send(failed(owner, generation)).unwrap();
        assert_eq!(
            outcome,
            if generation == 1 {
                GamesResultPublishOutcome::Queued
            } else {
                GamesResultPublishOutcome::Replaced
            }
        );
    }
    assert_eq!(receiver.len(), 1);
    let GamesResult::ProbeFailed(result) = receiver.try_recv().unwrap() else {
        panic!("expected probe failure")
    };
    assert_eq!(result.token().request_generation.get(), 10_000);
}

#[test]
fn equal_older_and_replaced_owner_results_are_recoverably_stale() {
    let first = owner(2);
    let second = owner(3);
    let (sender, receiver) = games_result_channel(first);
    sender.try_send(failed(first, 2)).unwrap();
    for generation in [2, 1] {
        let rejected = sender.try_send(failed(first, generation)).unwrap_err();
        assert_eq!(rejected.error, GamesResultError::Stale);
    }
    sender
        .try_send(GamesResult::Barrier(GamesBarrier::draft_replace(
            first, second,
        )))
        .unwrap();
    let rejected = sender.try_send(failed(first, 3)).unwrap_err();
    assert_eq!(rejected.error, GamesResultError::StaleOwner);
    assert_eq!(receiver.len(), 2);
}

#[test]
fn draft_replacement_requires_a_strictly_newer_owner() {
    let first = owner(3);
    let older = owner(2);
    let (sender, _receiver) = games_result_channel(first);
    for current in [first, older] {
        let rejected = sender
            .try_send(GamesResult::Barrier(GamesBarrier::draft_replace(
                first, current,
            )))
            .unwrap_err();
        assert_eq!(rejected.error, GamesResultError::InvalidPayload);
    }
}

#[test]
fn explicit_barriers_prevent_probe_coalescing_across_them() {
    let owner = owner(5);
    let (sender, receiver) = games_result_channel(owner);
    sender.try_send(failed(owner, 1)).unwrap();
    sender
        .try_send(GamesResult::Barrier(GamesBarrier::draft_discard(owner)))
        .unwrap();
    sender.try_send(failed(owner, 2)).unwrap();
    assert_eq!(receiver.len(), 3);
}

#[test]
fn disconnected_receiver_returns_the_move_owned_payload() {
    let owner = owner(6);
    let (sender, receiver) = games_result_channel(owner);
    drop(receiver);
    let rejected = sender.try_send(failed(owner, 1)).unwrap_err();
    assert_eq!(rejected.error, GamesResultError::Disconnected);
    assert!(matches!(rejected.result, GamesResult::ProbeFailed(_)));
}

#[test]
fn ready_candidate_catalog_requires_and_returns_its_exact_token() {
    let owner = owner(7);
    let exact = token(owner, ProbeKind::InstalledGames, 2);
    let wrong = token(owner, ProbeKind::InstalledGames, 3);
    let catalog = InstalledGameIdentityCatalog::build(exact, vec![candidate()]).unwrap();
    assert_eq!(
        GamesProbeReady::new(wrong, GamesProbeCatalog::Installed(catalog)).unwrap_err(),
        GamesResultError::InvalidPayload
    );

    let catalog = InstalledGameIdentityCatalog::build(exact, vec![candidate()]).unwrap();
    let ready = GamesProbeReady::new(exact, GamesProbeCatalog::Installed(catalog)).unwrap();
    let (sender, receiver) = games_result_channel(owner);
    sender.try_send(GamesResult::ProbeReady(ready)).unwrap();
    let GamesResult::ProbeReady(ready) = receiver.try_recv().unwrap() else {
        panic!("expected ready catalog")
    };
    let GamesProbeCatalog::Installed(catalog) = ready.into_catalog() else {
        panic!("expected installed catalog")
    };
    assert_eq!(catalog.token(), exact);
    assert_eq!(catalog.len(), 1);
}

#[test]
fn ready_plugin_catalog_revalidates_the_owned_payload_before_fixed_accounting() {
    let owner = owner(8);
    let exact = token(owner, ProbeKind::GamePlugins, 1);
    let base = catalog_bounded(std::path::Path::new("unused")).unwrap();
    let seed = base.first().unwrap().clone();

    let mut count = base.clone();
    count.resize(MAX_GAME_PLUGINS + 1, seed.clone());
    let mut text = base.clone();
    text[0].name = "x".repeat(MAX_PLUGIN_TEXT_BYTES + 1);
    let mut icon = base.clone();
    icon[0].icon = Some("x".repeat(MAX_PLUGIN_ICON_DATA_URL_BYTES + 1));
    let mut aggregate = vec![seed; 5];
    for plugin in &mut aggregate {
        plugin.icon = Some("x".repeat(MAX_PLUGIN_ICON_BYTES));
    }
    assert!(
        aggregate
            .iter()
            .map(|plugin| plugin.icon.as_ref().unwrap().len())
            .sum::<usize>()
            > MAX_PLUGIN_CATALOG_BYTES
    );

    for plugins in [count, text, icon, aggregate] {
        assert_eq!(
            GamesProbeReady::new(exact, GamesProbeCatalog::Plugins(plugins)).unwrap_err(),
            GamesResultError::InvalidPayload
        );
    }
}
