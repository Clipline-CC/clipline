use clipline_settings::{
    SettingsChange, SettingsPathResolver, SettingsProfile, SettingsTransaction,
};
use clipline_slint_spike::settings::{CandidateSettings, CandidateSettingsProfile};
use clipline_test_utils::TestDir;

struct PoisonProductionResolver;

impl SettingsPathResolver for PoisonProductionResolver {
    fn resolve_settings_profile(&self) -> SettingsProfile {
        panic!("isolated tests must never resolve the installed Clipline profile")
    }
}

#[test]
fn isolated_candidate_loads_a_complete_document_without_saving_on_bootstrap() {
    let dir = TestDir::new("clipline-slint-settings", "bootstrap");
    let settings = CandidateSettings::open_with_resolver(
        CandidateSettingsProfile::Isolated(dir.path().to_path_buf()),
        &PoisonProductionResolver,
    )
    .unwrap();
    let snapshot = settings.snapshot().unwrap();

    assert_eq!(snapshot.document.hotkey, "Alt+F10");
    assert!(snapshot.document.close_to_tray);
    assert_eq!(
        snapshot.document.media_dir,
        dir.path().join("media").display().to_string()
    );
    assert!(!dir.path().join("settings.json").exists());
}

#[test]
fn relative_candidate_profile_is_absolute_and_never_resolves_production() {
    let relative = std::path::PathBuf::from(format!(
        "target/clipline-slint-relative-profile-{}",
        std::process::id()
    ));
    let settings = CandidateSettings::open_with_resolver(
        CandidateSettingsProfile::Isolated(relative),
        &PoisonProductionResolver,
    )
    .unwrap();
    let snapshot = settings.snapshot().unwrap();

    assert!(settings.store().profile().settings_path().is_absolute());
    assert!(snapshot.document.media_dir_path().unwrap().is_absolute());
    snapshot.document.validate().unwrap();
    assert!(!settings.store().profile().settings_path().exists());
}

#[test]
fn isolated_candidate_persists_only_through_the_shared_transaction_api() {
    let dir = TestDir::new("clipline-slint-settings", "transaction");
    let settings =
        CandidateSettings::open(CandidateSettingsProfile::Isolated(dir.path().to_path_buf()))
            .unwrap();
    let before = settings.snapshot().unwrap();
    let media = dir.path().join("clips").display().to_string();
    let after = settings
        .transact(SettingsTransaction {
            expected_revision: before.revision,
            expected_account_generation: before.account_generation,
            change: SettingsChange::SetMediaRoot(media.clone()),
        })
        .unwrap();

    assert_eq!(after.document.media_dir, media);
    assert!(dir.path().join("settings.json").exists());
    let reopened =
        CandidateSettings::open(CandidateSettingsProfile::Isolated(dir.path().to_path_buf()))
            .unwrap();
    assert_eq!(reopened.snapshot().unwrap().document, after.document);
}

#[test]
fn window_lifecycle_reads_do_not_mutate_the_process_owned_store() {
    let dir = TestDir::new("clipline-slint-settings", "window-lifecycle");
    let settings =
        CandidateSettings::open(CandidateSettingsProfile::Isolated(dir.path().to_path_buf()))
            .unwrap();
    let before = settings.snapshot().unwrap();

    // Create/drop/recreate projections read the same process-owned snapshot.
    let first_projection = settings.snapshot().unwrap();
    drop(first_projection);
    let second_projection = settings.snapshot().unwrap();

    assert_eq!(second_projection, before);
    assert!(!dir.path().join("settings.json").exists());
}

#[test]
fn candidate_adapter_has_no_credential_or_secret_access() {
    let source = include_str!("../src/settings.rs");
    for forbidden in [
        "CredentialStore",
        "access_token",
        "refresh_token",
        "client_secret",
        "password",
    ] {
        assert!(
            !source.contains(forbidden),
            "candidate settings adapter must not contain {forbidden}"
        );
    }
}
