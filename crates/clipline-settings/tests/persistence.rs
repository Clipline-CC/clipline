use std::sync::{Arc, Barrier};

use clipline_settings::cloud::{cloud_credential_target, CloudUploadRecord};
use clipline_settings::osu::osu_credential_target;
use clipline_settings::{
    AccountGeneration, CloudAccountIdentity, SettingsChange, SettingsProfile, SettingsStore,
    SettingsTransaction, SettingsTransactionError,
};
use clipline_test_utils::TestDir;

fn transaction(
    snapshot: &clipline_settings::SettingsSnapshot,
    change: SettingsChange,
) -> SettingsTransaction {
    SettingsTransaction {
        expected_revision: snapshot.revision,
        expected_account_generation: snapshot.account_generation,
        change,
    }
}

fn file_bytes(path: &std::path::Path) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

#[test]
fn isolated_profile_opens_without_touching_disk_and_uses_isolated_defaults() {
    let dir = TestDir::new("clipline-settings", "isolated-open");
    let profile = SettingsProfile::isolated(dir.path());
    let store = SettingsStore::open(profile.clone());
    let snapshot = store.snapshot().unwrap();

    assert_eq!(
        snapshot.document.media_dir,
        profile.default_media_dir().display().to_string()
    );
    assert!(!profile.settings_path().exists());
    assert!(store.startup_warnings().is_empty());
}

#[test]
fn relative_isolated_profile_resolves_to_an_absolute_valid_document() {
    let relative = std::path::PathBuf::from(format!(
        "target/clipline-settings-relative-{}",
        std::process::id()
    ));
    let profile = SettingsProfile::try_isolated(&relative).unwrap();
    let store = SettingsStore::open(profile.clone());
    let snapshot = store.snapshot().unwrap();

    assert!(profile.settings_path().is_absolute());
    assert!(profile.default_media_dir().is_absolute());
    assert!(snapshot.document.media_dir_path().unwrap().is_absolute());
    snapshot.document.validate().unwrap();
    assert!(!profile.settings_path().exists());
}

#[test]
fn stale_revision_preserves_primary_backup_memory_and_revision_byte_for_byte() {
    let dir = TestDir::new("clipline-settings", "stale-revision");
    let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
    let initial = store.snapshot().unwrap();
    let first = store
        .transact(transaction(
            &initial,
            SettingsChange::SetMediaRoot(dir.path().join("first").display().to_string()),
        ))
        .unwrap();
    let second = store
        .transact(transaction(
            &first,
            SettingsChange::SetMediaRoot(dir.path().join("second").display().to_string()),
        ))
        .unwrap();
    let primary_path = store.profile().settings_path();
    let backup_path = dir.path().join("settings.json.bak");
    let primary = file_bytes(primary_path);
    let backup = file_bytes(&backup_path);

    let error = store
        .transact(transaction(
            &first,
            SettingsChange::SetMediaRoot(dir.path().join("stale").display().to_string()),
        ))
        .unwrap_err();

    assert!(matches!(
        error,
        SettingsTransactionError::StaleRevision { .. }
    ));
    assert_eq!(store.snapshot().unwrap(), second);
    assert_eq!(file_bytes(primary_path), primary);
    assert_eq!(file_bytes(&backup_path), backup);
}

#[test]
fn stale_account_generation_is_checked_after_the_matching_revision() {
    let dir = TestDir::new("clipline-settings", "stale-account");
    let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
    let initial = store.snapshot().unwrap();
    let mut profile = initial.document.cloud.clone();
    profile.host_url = "https://clips.example.com/".into();
    profile.connected_user_id = Some("user-1".into());
    profile.credential_target = Some(cloud_credential_target(&profile.host_url, "user-1"));
    let connected = store
        .transact(transaction(
            &initial,
            SettingsChange::ReplaceCloudProfile(profile),
        ))
        .unwrap();

    let error = store
        .transact(SettingsTransaction {
            expected_revision: connected.revision,
            expected_account_generation: initial.account_generation,
            change: SettingsChange::SetMediaRoot(
                dir.path().join("media-next").display().to_string(),
            ),
        })
        .unwrap_err();

    assert!(matches!(
        error,
        SettingsTransactionError::StaleAccountGeneration { .. }
    ));
    assert_eq!(store.snapshot().unwrap(), connected);
}

#[test]
fn account_generation_changes_only_with_account_identity() {
    let dir = TestDir::new("clipline-settings", "account-generation");
    let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
    let initial = store.snapshot().unwrap();
    let mut profile = initial.document.cloud.clone();
    profile.host_url = "https://clips.example.com".into();
    profile.connected_user_id = Some("user-1".into());
    profile.connected_display_name = Some("Clipper".into());
    profile.credential_target = Some(cloud_credential_target(&profile.host_url, "user-1"));
    let connected = store
        .transact(transaction(
            &initial,
            SettingsChange::ReplaceCloudProfile(profile),
        ))
        .unwrap();
    assert!(connected.account_generation > initial.account_generation);

    let record = CloudUploadRecord {
        local_clip_id: "clip-1".into(),
        path: dir.path().join("clip.mp4").display().to_string(),
        remote_clip_id: Some("remote-1".into()),
        remote_url: None,
        visibility: "private".into(),
        upload_status: "uploaded_private".into(),
        error: None,
        updated_at_unix: 1,
    };
    let recorded = store
        .transact(transaction(
            &connected,
            SettingsChange::UpsertCloudRecord {
                account: connected.account.clone(),
                key: "clip-1".into(),
                record,
            },
        ))
        .unwrap();
    assert_eq!(recorded.account_generation, connected.account_generation);
}

#[test]
fn cloud_record_rejects_the_wrong_account_without_mutation() {
    let dir = TestDir::new("clipline-settings", "wrong-account");
    let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
    let snapshot = store.snapshot().unwrap();
    let error = store
        .transact(transaction(
            &snapshot,
            SettingsChange::RemoveCloudRecord {
                account: CloudAccountIdentity {
                    host_url: "https://other.example.com".into(),
                    connected_user_id: Some("other".into()),
                    credential_target: None,
                },
                key: "clip-1".into(),
            },
        ))
        .unwrap_err();
    assert_eq!(error, SettingsTransactionError::AccountChanged);
    assert_eq!(store.snapshot().unwrap(), snapshot);
    assert!(!store.profile().settings_path().exists());
}

#[test]
fn external_primary_edit_is_detected_before_any_backup_or_memory_change() {
    let dir = TestDir::new("clipline-settings", "external-edit");
    let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
    let initial = store.snapshot().unwrap();
    let committed = store
        .transact(transaction(
            &initial,
            SettingsChange::SetMediaRoot(dir.path().join("first").display().to_string()),
        ))
        .unwrap();
    let primary_path = store.profile().settings_path();
    let mut external = std::fs::read(primary_path).unwrap();
    external.extend_from_slice(b"\n");
    std::fs::write(primary_path, &external).unwrap();
    let backup_path = dir.path().join("settings.json.bak");
    let backup = file_bytes(&backup_path);

    let error = store
        .transact(transaction(
            &committed,
            SettingsChange::SetMediaRoot(dir.path().join("second").display().to_string()),
        ))
        .unwrap_err();

    assert_eq!(error, SettingsTransactionError::ExternalModification);
    assert_eq!(std::fs::read(primary_path).unwrap(), external);
    assert_eq!(file_bytes(&backup_path), backup);
    assert_eq!(store.snapshot().unwrap(), committed);
}

#[test]
fn two_transactions_from_one_snapshot_allow_exactly_one_commit() {
    let dir = TestDir::new("clipline-settings", "concurrent-cas");
    let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
    let snapshot = store.snapshot().unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let mut threads = Vec::new();
    for name in ["a", "b"] {
        let store = store.clone();
        let snapshot = snapshot.clone();
        let barrier = Arc::clone(&barrier);
        let media = dir.path().join(name).display().to_string();
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            store.transact(transaction(&snapshot, SettingsChange::SetMediaRoot(media)))
        }));
    }
    barrier.wait();
    let results = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(SettingsTransactionError::StaleRevision { .. })))
            .count(),
        1
    );
}

#[test]
fn independently_opened_store_rejects_a_peer_commit_without_overwrite() {
    let dir = TestDir::new("clipline-settings", "independent-store-cas");
    let profile = SettingsProfile::isolated(dir.path());
    let first_store = SettingsStore::open(profile.clone());
    let second_store = SettingsStore::open(profile);
    let first_snapshot = first_store.snapshot().unwrap();
    let second_snapshot = second_store.snapshot().unwrap();
    let committed = first_store
        .transact(transaction(
            &first_snapshot,
            SettingsChange::SetMediaRoot(dir.path().join("first").display().to_string()),
        ))
        .unwrap();
    let primary_path = first_store.profile().settings_path();
    let primary = std::fs::read(primary_path).unwrap();

    let error = second_store
        .transact(transaction(
            &second_snapshot,
            SettingsChange::SetMediaRoot(dir.path().join("second").display().to_string()),
        ))
        .unwrap_err();

    assert_eq!(error, SettingsTransactionError::ExternalModification);
    assert_eq!(std::fs::read(primary_path).unwrap(), primary);
    assert_eq!(first_store.snapshot().unwrap(), committed);
    assert_eq!(second_store.snapshot().unwrap(), second_snapshot);
}

#[test]
fn validation_and_precommit_failures_preserve_all_observable_state() {
    let dir = TestDir::new("clipline-settings", "failed-commit");
    let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
    let initial = store.snapshot().unwrap();
    let committed = store
        .transact(transaction(
            &initial,
            SettingsChange::SetMediaRoot(dir.path().join("first").display().to_string()),
        ))
        .unwrap();
    let primary_path = store.profile().settings_path();
    let backup_path = dir.path().join("settings.json.bak");
    let primary = file_bytes(primary_path);
    let backup = file_bytes(&backup_path);

    let validation = store
        .transact(transaction(
            &committed,
            SettingsChange::SetMediaRoot("relative".into()),
        ))
        .unwrap_err();
    assert!(matches!(
        validation,
        SettingsTransactionError::Validation(_)
    ));
    assert_eq!(store.snapshot().unwrap(), committed);
    assert_eq!(file_bytes(primary_path), primary);
    assert_eq!(file_bytes(&backup_path), backup);

    std::fs::create_dir(&backup_path).unwrap();
    let before_directory = std::fs::metadata(&backup_path).unwrap();
    let persistence = store
        .transact(transaction(
            &committed,
            SettingsChange::SetMediaRoot(dir.path().join("second").display().to_string()),
        ))
        .unwrap_err();
    assert!(matches!(
        persistence,
        SettingsTransactionError::Persistence(_)
    ));
    assert_eq!(store.snapshot().unwrap(), committed);
    assert_eq!(file_bytes(primary_path), primary);
    assert!(before_directory.is_dir());
    assert!(std::fs::metadata(&backup_path).unwrap().is_dir());
}

#[test]
fn credential_targets_are_exact_and_secrets_have_no_document_field() {
    assert_eq!(
        cloud_credential_target("https://clips.example.com/", " user-1 "),
        "Clipline Cloud:https://clips.example.com:user-1"
    );
    assert_eq!(
        osu_credential_target(" 61835 ", " 3426414 "),
        "Clipline osu!:61835:3426414"
    );

    let document = clipline_settings::AppSettings::default();
    let json = serde_json::to_string(&document).unwrap();
    for forbidden in ["access_token", "client_secret", "refresh_token", "password"] {
        assert!(
            !json.contains(forbidden),
            "unexpected secret field {forbidden}"
        );
    }
    assert_eq!(AccountGeneration::INITIAL.get(), 1);
}
