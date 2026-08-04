use std::sync::{mpsc, Arc, Barrier};
use std::time::Duration;

use clipline_settings::cloud::{cloud_credential_target, CloudUploadRecord};
use clipline_settings::osu::osu_credential_target;
use clipline_settings::{
    AccountGeneration, CloudAccountIdentity, CloudAccountPublicationOwner, CloudProfileCas,
    CloudRecordCas, CloudRecordCasKind, CloudRecordSlot, OsuApiSettings, SettingsChange,
    SettingsPreferences, SettingsProfile, SettingsStore, SettingsTransaction,
    SettingsTransactionError,
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

fn ui_preferences(snapshot: &clipline_settings::SettingsSnapshot) -> SettingsPreferences {
    SettingsPreferences::from_document(&snapshot.document).unwrap()
}

fn cloud_account_with_record(
    snapshot: &clipline_settings::SettingsSnapshot,
    host: &str,
    user: &str,
    updated_at_unix: u64,
) -> clipline_settings::CloudSettings {
    let mut cloud = snapshot.document.cloud.clone();
    cloud.host_url = host.into();
    cloud.public_url = Some(format!("{host}/public"));
    cloud.connected_user_id = Some(user.into());
    cloud.connected_username = Some(format!("{user}-name"));
    cloud.connected_display_name = Some(format!("{user} display"));
    cloud.credential_target = Some(cloud_credential_target(host, user));
    cloud.credential_cleanup_targets = vec![format!("old-{user}")];
    cloud.upload_generation_sequence = updated_at_unix;
    cloud.uploads.insert(
        "source-1".into(),
        CloudUploadRecord {
            local_clip_id: "source-1".into(),
            client_clip_id: Some("client-1".into()),
            upload_generation: Some(updated_at_unix),
            path: r"D:\Clips\source-1.mp4".into(),
            remote_clip_id: Some("remote-1".into()),
            remote_url: Some(format!("{host}/clips/remote-1")),
            visibility: "public".into(),
            upload_status: "ready".into(),
            error: None,
            updated_at_unix,
        },
    );
    cloud
}

#[test]
fn preference_cas_preserves_cloud_account_aba_upload_progress_and_osu_bytes() {
    let dir = TestDir::new("clipline-settings", "preference-cas-backend-race");
    let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
    let initial = store.snapshot().unwrap();
    let expected = ui_preferences(&initial);

    let account_a = store
        .transact(transaction(
            &initial,
            SettingsChange::ReplaceCloudSettings(cloud_account_with_record(
                &initial,
                "https://a.example",
                "user-a",
                7,
            )),
        ))
        .unwrap();
    let account_b = store
        .transact(transaction(
            &account_a,
            SettingsChange::ReplaceCloudSettings(cloud_account_with_record(
                &account_a,
                "https://b.example",
                "user-b",
                8,
            )),
        ))
        .unwrap();
    let account_a_again = store
        .transact(transaction(
            &account_b,
            SettingsChange::ReplaceCloudSettings(cloud_account_with_record(
                &account_b,
                "https://a.example",
                "user-a",
                9,
            )),
        ))
        .unwrap();
    let osu = OsuApiSettings {
        account_generation: clipline_settings::OsuAccountGeneration::INITIAL,
        client_id: Some("12345".into()),
        user: Some("dain".into()),
        credential_target: Some(osu_credential_target("12345", "dain")),
        credential_cleanup_targets: vec!["old-osu-target".into()],
        last_connected_username: Some("Dain".into()),
    };
    let raced = store
        .transact(transaction(
            &account_a_again,
            SettingsChange::ReplaceOsuProfile(osu),
        ))
        .unwrap();
    let cloud_bytes = serde_json::to_vec(&raced.document.cloud).unwrap();
    let osu_bytes = serde_json::to_vec(&raced.document.osu).unwrap();

    let mut replacement = expected.try_clone_bounded().unwrap();
    replacement.replay_window_s = 77.0;
    replacement.close_to_tray = false;
    let committed = store
        .replace_preferences_if_unchanged(&expected, replacement)
        .unwrap();

    assert_eq!(committed.revision.get(), raced.revision.get() + 1);
    assert_eq!(committed.account_generation, raced.account_generation);
    assert_eq!(
        serde_json::to_vec(&committed.document.cloud).unwrap(),
        cloud_bytes
    );
    assert_eq!(
        serde_json::to_vec(&committed.document.osu).unwrap(),
        osu_bytes
    );
    assert_eq!(committed.document.replay_window_s, 77.0);
    assert_eq!(committed.document.buffer_seconds, 77.0);
    assert!(!committed.document.close_to_tray);
}

#[test]
fn preference_cas_rejects_disjoint_second_session_without_any_mutation() {
    let dir = TestDir::new("clipline-settings", "preference-cas-stale-draft");
    let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
    let initial = store.snapshot().unwrap();
    let expected = ui_preferences(&initial);
    let mut first = expected.try_clone_bounded().unwrap();
    first.open_on_startup = true;
    let committed = store
        .replace_preferences_if_unchanged(&expected, first)
        .unwrap();

    let primary = file_bytes(store.profile().settings_path());
    let backup = file_bytes(&dir.path().join("settings.json.bak"));
    let mut disjoint = expected.try_clone_bounded().unwrap();
    disjoint.close_to_tray = false;
    let error = store
        .replace_preferences_if_unchanged(&expected, disjoint)
        .unwrap_err();

    assert_eq!(error, SettingsTransactionError::StalePreferences);
    assert_eq!(store.snapshot().unwrap(), committed);
    assert_eq!(file_bytes(store.profile().settings_path()), primary);
    assert_eq!(file_bytes(&dir.path().join("settings.json.bak")), backup);
}

#[test]
fn preference_cas_rejects_invalid_expected_or_replacement_before_mutation() {
    let dir = TestDir::new("clipline-settings", "preference-cas-invalid-input");
    let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
    let before = store.snapshot().unwrap();
    let expected = ui_preferences(&before);

    let mut invalid_replacement = expected.try_clone_bounded().unwrap();
    invalid_replacement.hotkey_secondary = Some(invalid_replacement.hotkey.clone());
    assert!(matches!(
        store
            .replace_preferences_if_unchanged(&expected, invalid_replacement)
            .unwrap_err(),
        SettingsTransactionError::Validation(_)
    ));

    let mut invalid_expected = expected.try_clone_bounded().unwrap();
    invalid_expected.replay_window_s = f64::NAN;
    let replacement = expected.try_clone_bounded().unwrap();
    assert!(matches!(
        store
            .replace_preferences_if_unchanged(&invalid_expected, replacement)
            .unwrap_err(),
        SettingsTransactionError::Validation(_)
    ));
    assert_eq!(store.snapshot().unwrap(), before);
    assert!(!store.profile().settings_path().exists());
}

#[test]
fn concurrent_preference_sessions_allow_one_commit_and_one_typed_conflict() {
    let dir = TestDir::new("clipline-settings", "preference-cas-concurrent");
    let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
    let snapshot = store.snapshot().unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let mut threads = Vec::new();
    for edit_startup in [false, true] {
        let store = store.clone();
        let expected = ui_preferences(&snapshot);
        let mut replacement = expected.try_clone_bounded().unwrap();
        if edit_startup {
            replacement.open_on_startup = true;
        } else {
            replacement.close_to_tray = false;
        }
        let barrier = Arc::clone(&barrier);
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            store.replace_preferences_if_unchanged(&expected, replacement)
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
            .filter(|result| matches!(result, Err(SettingsTransactionError::StalePreferences)))
            .count(),
        1
    );
}

#[test]
fn independently_opened_store_merges_preferences_into_the_current_shared_document() {
    let dir = TestDir::new("clipline-settings", "preference-cas-independent-store");
    let profile = SettingsProfile::isolated(dir.path());
    let cloud_store = SettingsStore::open(profile.clone());
    let settings_store = SettingsStore::open(profile.clone());
    let cloud_baseline = cloud_store.snapshot().unwrap();
    let settings_baseline = settings_store.snapshot().unwrap();
    let expected = ui_preferences(&settings_baseline);
    let cloud = cloud_store
        .transact(transaction(
            &cloud_baseline,
            SettingsChange::ReplaceCloudSettings(cloud_account_with_record(
                &cloud_baseline,
                "https://peer.example",
                "peer",
                4,
            )),
        ))
        .unwrap();

    let mut replacement = expected.try_clone_bounded().unwrap();
    replacement.minimize_to_tray = true;
    let committed = settings_store
        .replace_preferences_if_unchanged(&expected, replacement)
        .unwrap();

    assert_eq!(committed.revision.get(), cloud.revision.get() + 1);
    assert_eq!(committed.document.cloud, cloud.document.cloud);
    assert!(committed.document.minimize_to_tray);
    assert_eq!(settings_store.snapshot().unwrap(), committed);
    let reopened = SettingsStore::open(profile).snapshot().unwrap();
    assert_eq!(reopened.document, committed.document);
    assert_eq!(reopened.revision, committed.revision);
}

#[test]
fn preference_cas_fails_closed_on_external_invalid_primary_and_persistence_error() {
    let dir = TestDir::new("clipline-settings", "preference-cas-fail-closed");
    let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
    let initial = store.snapshot().unwrap();
    let expected = ui_preferences(&initial);
    let mut first = expected.try_clone_bounded().unwrap();
    first.close_to_tray = false;
    let committed = store
        .replace_preferences_if_unchanged(&expected, first)
        .unwrap();
    let committed_preferences = ui_preferences(&committed);
    let primary_path = store.profile().settings_path();
    let backup_path = dir.path().join("settings.json.bak");
    let primary = file_bytes(primary_path);
    let backup = file_bytes(&backup_path);

    std::fs::write(primary_path, b"{ invalid external settings").unwrap();
    let external = std::fs::read(primary_path).unwrap();
    let mut external_replacement = committed_preferences.try_clone_bounded().unwrap();
    external_replacement.minimize_to_tray = true;
    assert_eq!(
        store
            .replace_preferences_if_unchanged(&committed_preferences, external_replacement)
            .unwrap_err(),
        SettingsTransactionError::ExternalModification
    );
    assert_eq!(store.snapshot().unwrap(), committed);
    assert_eq!(std::fs::read(primary_path).unwrap(), external);
    assert_eq!(file_bytes(&backup_path), backup);

    std::fs::write(primary_path, primary.unwrap()).unwrap();
    std::fs::create_dir(&backup_path).unwrap();
    let mut persistence_replacement = committed_preferences.try_clone_bounded().unwrap();
    persistence_replacement.minimize_to_tray = true;
    let error = store
        .replace_preferences_if_unchanged(&committed_preferences, persistence_replacement)
        .unwrap_err();
    assert!(matches!(error, SettingsTransactionError::Persistence(_)));
    assert_eq!(store.snapshot().unwrap(), committed);
}

#[test]
fn preference_cas_rejects_same_bytes_at_a_replaced_file_identity() {
    let dir = TestDir::new("clipline-settings", "preference-cas-file-identity");
    let profile = SettingsProfile::isolated(dir.path());
    let store = SettingsStore::open(profile.clone());
    let initial = store.snapshot().unwrap();
    let expected = ui_preferences(&initial);
    let mut first = expected.try_clone_bounded().unwrap();
    first.close_to_tray = false;
    let committed = store
        .replace_preferences_if_unchanged(&expected, first)
        .unwrap();
    let committed_preferences = ui_preferences(&committed);
    let primary_path = store.profile().settings_path();
    let bytes = std::fs::read(primary_path).unwrap();
    std::fs::remove_file(primary_path).unwrap();
    std::fs::write(primary_path, &bytes).unwrap();
    let peer_opened_after_replacement = SettingsStore::open(profile);

    let mut replacement = committed_preferences.try_clone_bounded().unwrap();
    replacement.minimize_to_tray = true;
    assert_eq!(
        store
            .replace_preferences_if_unchanged(&committed_preferences, replacement)
            .unwrap_err(),
        SettingsTransactionError::ExternalModification
    );
    assert_eq!(store.snapshot().unwrap(), committed);
    assert_eq!(std::fs::read(primary_path).unwrap(), bytes);
    let peer_expected = ui_preferences(&peer_opened_after_replacement.snapshot().unwrap());
    let peer_replacement = peer_expected.try_clone_bounded().unwrap();
    assert_eq!(
        peer_opened_after_replacement
            .replace_preferences_if_unchanged(&peer_expected, peer_replacement)
            .unwrap_err(),
        SettingsTransactionError::ExternalModification
    );
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
        client_clip_id: None,
        upload_generation: None,
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
                expected: None,
                record,
            },
        ))
        .unwrap();
    assert_eq!(recorded.account_generation, connected.account_generation);
}

#[test]
fn cloud_profile_cas_uses_current_revision_and_rejects_a_replacement_login() {
    let dir = TestDir::new("clipline-settings", "cloud-profile-cas");
    let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
    let initial = store.snapshot().unwrap();
    let mut account_a = initial.document.cloud.clone();
    account_a.host_url = "https://a.example".into();
    account_a.connected_user_id = Some("user-a".into());
    account_a.credential_target = Some(cloud_credential_target(&account_a.host_url, "user-a"));
    account_a.upload_generation_sequence = 17;
    let connected = store
        .transact(transaction(
            &initial,
            SettingsChange::ReplaceCloudSettings(account_a),
        ))
        .unwrap();
    let patch = CloudProfileCas {
        account: connected.account.clone(),
        account_generation: connected.account_generation,
        expected_connected_user_id: "user-a".into(),
        username: "clipper-a".into(),
        display_name: Some("Clipper A".into()),
    };

    let media_root = dir.path().join("unrelated-media").display().to_string();
    let unrelated = store
        .transact(transaction(
            &connected,
            SettingsChange::SetMediaRoot(media_root.clone()),
        ))
        .unwrap();
    let profiled = store.compare_exchange_cloud_profile(patch).unwrap();
    assert_eq!(profiled.document.media_dir, media_root);
    assert_eq!(
        profiled.document.cloud.connected_user_id.as_deref(),
        Some("user-a")
    );
    assert_eq!(
        profiled.document.cloud.connected_username.as_deref(),
        Some("clipper-a")
    );
    assert_eq!(
        profiled.document.cloud.connected_display_name.as_deref(),
        Some("Clipper A")
    );
    assert_eq!(profiled.account_generation, unrelated.account_generation);
    assert_eq!(profiled.document.cloud.upload_generation_sequence, 17);

    let stale_patch = CloudProfileCas {
        account: profiled.account.clone(),
        account_generation: profiled.account_generation,
        expected_connected_user_id: "user-a".into(),
        username: "x".repeat(clipline_settings::MAX_CLOUD_UPLOAD_ID_BYTES + 1),
        display_name: None,
    };
    let mut account_b = profiled.document.cloud.clone();
    account_b.host_url = "https://b.example".into();
    account_b.connected_user_id = Some("user-b".into());
    account_b.credential_target = Some(cloud_credential_target(&account_b.host_url, "user-b"));
    let replacement = store
        .transact(transaction(
            &profiled,
            SettingsChange::ReplaceCloudProfile(account_b),
        ))
        .unwrap();

    assert_eq!(
        store
            .compare_exchange_cloud_profile(stale_patch)
            .unwrap_err(),
        SettingsTransactionError::AccountChanged
    );
    assert_eq!(store.snapshot().unwrap(), replacement);
}

#[test]
fn cloud_profile_cas_rejects_unbounded_or_empty_profile_text_without_mutation() {
    let dir = TestDir::new("clipline-settings", "cloud-profile-cas-bounds");
    let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
    let initial = store.snapshot().unwrap();
    let mut cloud = initial.document.cloud.clone();
    cloud.host_url = "https://cloud.example".into();
    cloud.connected_user_id = Some("user-1".into());
    cloud.credential_target = Some(cloud_credential_target(&cloud.host_url, "user-1"));
    cloud.upload_generation_sequence = 23;
    let connected = store
        .transact(transaction(
            &initial,
            SettingsChange::ReplaceCloudSettings(cloud),
        ))
        .unwrap();
    let oversized = "x".repeat(clipline_settings::MAX_CLOUD_UPLOAD_ID_BYTES + 1);
    let cases = [
        CloudProfileCas {
            account: connected.account.clone(),
            account_generation: connected.account_generation,
            expected_connected_user_id: "".into(),
            username: "valid".into(),
            display_name: None,
        },
        CloudProfileCas {
            account: connected.account.clone(),
            account_generation: connected.account_generation,
            expected_connected_user_id: oversized.clone(),
            username: "valid".into(),
            display_name: None,
        },
        CloudProfileCas {
            account: connected.account.clone(),
            account_generation: connected.account_generation,
            expected_connected_user_id: "user-1".into(),
            username: " ".into(),
            display_name: None,
        },
        CloudProfileCas {
            account: connected.account.clone(),
            account_generation: connected.account_generation,
            expected_connected_user_id: "user-1".into(),
            username: oversized.clone(),
            display_name: None,
        },
        CloudProfileCas {
            account: connected.account.clone(),
            account_generation: connected.account_generation,
            expected_connected_user_id: "user-1".into(),
            username: "valid".into(),
            display_name: Some("".into()),
        },
        CloudProfileCas {
            account: connected.account.clone(),
            account_generation: connected.account_generation,
            expected_connected_user_id: "user-1".into(),
            username: "valid".into(),
            display_name: Some(oversized.clone()),
        },
    ];

    for (index, change) in cases.into_iter().enumerate() {
        let error = store.compare_exchange_cloud_profile(change).unwrap_err();
        if index < 2 {
            assert_eq!(error, SettingsTransactionError::AccountChanged);
        } else {
            assert!(matches!(error, SettingsTransactionError::Validation(_)));
        }
        assert_eq!(store.snapshot().unwrap(), connected);
        assert_eq!(
            store
                .snapshot()
                .unwrap()
                .document
                .cloud
                .upload_generation_sequence,
            23
        );
    }

    // A hand-edited/current account can itself carry an oversized user id.
    // Once exact owner and user fencing succeeds, the CAS-side bound must
    // still reject it before profile mutation.
    let mut oversized_account = connected.document.cloud.clone();
    oversized_account.connected_user_id = Some(oversized.clone());
    oversized_account.credential_target = Some("oversized-user-credential".into());
    let oversized_owner = store
        .transact(transaction(
            &connected,
            SettingsChange::ReplaceCloudSettings(oversized_account),
        ))
        .unwrap();
    let error = store
        .compare_exchange_cloud_profile(CloudProfileCas {
            account: oversized_owner.account.clone(),
            account_generation: oversized_owner.account_generation,
            expected_connected_user_id: oversized,
            username: "valid".into(),
            display_name: None,
        })
        .unwrap_err();
    assert!(matches!(error, SettingsTransactionError::Validation(_)));
    assert_eq!(store.snapshot().unwrap(), oversized_owner);
    assert_eq!(
        oversized_owner.document.cloud.upload_generation_sequence,
        23
    );
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
                expected: CloudUploadRecord {
                    local_clip_id: "clip-1".into(),
                    client_clip_id: None,
                    upload_generation: None,
                    path: "clip.mp4".into(),
                    remote_clip_id: None,
                    remote_url: None,
                    visibility: "private".into(),
                    upload_status: "queued".into(),
                    error: None,
                    updated_at_unix: 1,
                },
            },
        ))
        .unwrap_err();
    assert_eq!(error, SettingsTransactionError::AccountChanged);
    assert_eq!(store.snapshot().unwrap(), snapshot);
    assert!(!store.profile().settings_path().exists());
}

#[test]
fn cloud_record_compare_and_swap_rejects_delayed_upload_or_sync_results() {
    let dir = TestDir::new("clipline-settings", "cloud-record-cas");
    let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
    let initial = store.snapshot().unwrap();
    let record = |status: &str, updated_at_unix| CloudUploadRecord {
        local_clip_id: "clip-1".into(),
        client_clip_id: None,
        upload_generation: None,
        path: dir.path().join("clip.mp4").display().to_string(),
        remote_clip_id: None,
        remote_url: None,
        visibility: "private".into(),
        upload_status: status.into(),
        error: None,
        updated_at_unix,
    };
    let queued = record("queued", 1);
    let queued_snapshot = store
        .transact(transaction(
            &initial,
            SettingsChange::UpsertCloudRecord {
                account: initial.account.clone(),
                key: "clip-1".into(),
                expected: None,
                record: queued.clone(),
            },
        ))
        .unwrap();
    let uploading = record("uploading", 2);
    let current = store
        .transact(transaction(
            &queued_snapshot,
            SettingsChange::UpsertCloudRecord {
                account: queued_snapshot.account.clone(),
                key: "clip-1".into(),
                expected: Some(queued.clone()),
                record: uploading.clone(),
            },
        ))
        .unwrap();

    let stale_write = store
        .transact(transaction(
            &current,
            SettingsChange::UpsertCloudRecord {
                account: current.account.clone(),
                key: "clip-1".into(),
                expected: Some(queued),
                record: record("failed", 3),
            },
        ))
        .unwrap_err();
    assert_eq!(stale_write, SettingsTransactionError::StaleCloudRecord);
    assert_eq!(store.snapshot().unwrap(), current);

    let stale_remove = store
        .transact(transaction(
            &current,
            SettingsChange::RemoveCloudRecord {
                account: current.account.clone(),
                key: "clip-1".into(),
                expected: record("queued", 1),
            },
        ))
        .unwrap_err();
    assert_eq!(stale_remove, SettingsTransactionError::StaleCloudRecord);
    assert_eq!(store.snapshot().unwrap(), current);
    assert_eq!(
        store.snapshot().unwrap().document.cloud.uploads["clip-1"],
        uploading
    );
}

fn durable_record(
    local_clip_id: &str,
    client_clip_id: Option<&str>,
    upload_generation: u64,
    path: impl Into<String>,
    status: &str,
) -> CloudUploadRecord {
    CloudUploadRecord {
        local_clip_id: local_clip_id.into(),
        client_clip_id: client_clip_id.map(str::to_string),
        upload_generation: Some(upload_generation),
        path: path.into(),
        remote_clip_id: None,
        remote_url: None,
        visibility: "private".into(),
        upload_status: status.into(),
        error: None,
        updated_at_unix: upload_generation,
    }
}

fn cloud_record_cas(
    snapshot: &clipline_settings::SettingsSnapshot,
    kind: CloudRecordCasKind,
    expected: Vec<CloudRecordSlot>,
    replacement: Option<CloudRecordSlot>,
) -> SettingsTransaction {
    SettingsTransaction {
        expected_revision: snapshot.revision,
        expected_account_generation: snapshot.account_generation,
        change: SettingsChange::CompareExchangeCloudRecords(CloudRecordCas {
            account: snapshot.account.clone(),
            account_generation: snapshot.account_generation,
            kind,
            expected,
            replacement,
        }),
    }
}

#[test]
fn cloud_record_cas_uses_the_current_revision_after_an_unrelated_settings_write() {
    let dir = TestDir::new("clipline-settings", "cloud-cas-current-revision");
    let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
    let initial = store.snapshot().unwrap();
    let queued = durable_record(
        "source-1",
        None,
        1,
        dir.path().join("clip.mp4").display().to_string(),
        "queued",
    );
    let change = CloudRecordCas {
        account: initial.account.clone(),
        account_generation: initial.account_generation,
        kind: CloudRecordCasKind::Admit {
            upload_generation: 1,
        },
        expected: vec![CloudRecordSlot {
            key: "source-1".into(),
            record: None,
        }],
        replacement: Some(CloudRecordSlot {
            key: "source-1".into(),
            record: Some(queued.clone()),
        }),
    };

    // The Cloud CAS was prepared from `initial`, but this unrelated settings
    // transaction wins before publication. Exact record/account ownership is
    // still current, so the upload must not fail merely because the document
    // revision advanced.
    let unrelated_media_root = dir.path().join("other-media").display().to_string();
    let unrelated = store
        .transact(transaction(
            &initial,
            SettingsChange::SetMediaRoot(unrelated_media_root.clone()),
        ))
        .unwrap();
    let committed = store.compare_exchange_cloud_records(change).unwrap();

    assert_eq!(committed.revision.get(), unrelated.revision.get() + 1);
    assert_eq!(committed.document.media_dir, unrelated_media_root);
    assert_eq!(committed.document.cloud.uploads["source-1"], queued);
}

#[test]
fn upload_generation_sequence_migrates_and_survives_ui_preference_replacement() {
    let dir = TestDir::new("clipline-settings", "upload-generation-sequence-migration");
    let profile = SettingsProfile::isolated(dir.path());
    let mut legacy = clipline_settings::AppSettings {
        media_dir: profile.default_media_dir().display().to_string(),
        ..clipline_settings::AppSettings::default()
    };
    legacy.cloud.uploads.insert(
        "source-1".into(),
        durable_record(
            "source-1",
            None,
            41,
            dir.path().join("clip.mp4").display().to_string(),
            "uploaded_private",
        ),
    );
    // Serialize the pre-watermark shape directly: startup normalization must
    // derive the sequence from durable legacy records.
    let bytes = serde_json::to_vec_pretty(&legacy).unwrap();
    std::fs::write(profile.settings_path(), bytes).unwrap();
    let store = SettingsStore::open(profile);
    let migrated = store.snapshot().unwrap();
    assert_eq!(migrated.document.cloud.upload_generation_sequence, 41);

    let mut preferences = migrated.document.clone();
    preferences.cloud.upload_generation_sequence = 0;
    preferences.cloud.uploads.clear();
    let replaced = store
        .transact(transaction(
            &migrated,
            SettingsChange::ReplaceUiPreferences(preferences),
        ))
        .unwrap();
    assert_eq!(replaced.document.cloud.upload_generation_sequence, 41);
    assert!(replaced.document.cloud.uploads.contains_key("source-1"));
}

#[test]
fn upload_generation_sequence_survives_account_switch_and_process_restart() {
    let dir = TestDir::new("clipline-settings", "upload-sequence-account-round-trip");
    let profile = SettingsProfile::isolated(dir.path());
    let store = SettingsStore::open(profile.clone());
    let initial = store.snapshot().unwrap();
    let mut account_a = initial.document.cloud.clone();
    account_a.host_url = "https://a.example".into();
    account_a.connected_user_id = Some("user-a".into());
    account_a.credential_target = Some(cloud_credential_target(&account_a.host_url, "user-a"));
    let account_a_snapshot = store
        .transact(transaction(
            &initial,
            SettingsChange::ReplaceCloudSettings(account_a.clone()),
        ))
        .unwrap();
    let admitted_generation = 7;
    let admitted = store
        .compare_exchange_cloud_records(CloudRecordCas {
            account: account_a_snapshot.account.clone(),
            account_generation: account_a_snapshot.account_generation,
            kind: CloudRecordCasKind::Admit {
                upload_generation: admitted_generation,
            },
            expected: vec![CloudRecordSlot {
                key: "source-a".into(),
                record: None,
            }],
            replacement: Some(CloudRecordSlot {
                key: "source-a".into(),
                record: Some(durable_record(
                    "source-a",
                    None,
                    admitted_generation,
                    dir.path().join("a.mp4").display().to_string(),
                    "queued",
                )),
            }),
        })
        .unwrap();

    let mut account_b = clipline_settings::CloudSettings::default();
    account_b.host_url = "https://b.example".into();
    account_b.connected_user_id = Some("user-b".into());
    account_b.credential_target = Some(cloud_credential_target(&account_b.host_url, "user-b"));
    let switched_away = store
        .transact(transaction(
            &admitted,
            SettingsChange::ReplaceCloudSettings(account_b),
        ))
        .unwrap();
    assert_eq!(
        switched_away.document.cloud.upload_generation_sequence,
        admitted_generation
    );

    let switched_back = store
        .transact(transaction(
            &switched_away,
            SettingsChange::ReplaceCloudSettings(account_a),
        ))
        .unwrap();
    drop(store);

    let reopened = SettingsStore::open(profile);
    let restart_snapshot = reopened.snapshot().unwrap();
    assert_eq!(restart_snapshot.account, switched_back.account);
    assert_eq!(
        restart_snapshot.document.cloud.upload_generation_sequence,
        admitted_generation
    );
    let next_generation = admitted_generation + 1;
    let next = reopened
        .compare_exchange_cloud_records(CloudRecordCas {
            account: restart_snapshot.account.clone(),
            account_generation: restart_snapshot.account_generation,
            kind: CloudRecordCasKind::Admit {
                upload_generation: next_generation,
            },
            expected: vec![CloudRecordSlot {
                key: "source-a-replacement".into(),
                record: None,
            }],
            replacement: Some(CloudRecordSlot {
                key: "source-a-replacement".into(),
                record: Some(durable_record(
                    "source-a-replacement",
                    None,
                    next_generation,
                    dir.path().join("a-replacement.mp4").display().to_string(),
                    "queued",
                )),
            }),
        })
        .unwrap();
    assert_eq!(
        next.document.cloud.upload_generation_sequence,
        next_generation
    );
}

#[test]
fn profile_sequence_rejects_the_same_admit_generation_for_different_slots() {
    let dir = TestDir::new("clipline-settings", "upload-generation-sequence-race");
    let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
    let initial = store.snapshot().unwrap();
    let change = |key: &str| CloudRecordCas {
        account: initial.account.clone(),
        account_generation: initial.account_generation,
        kind: CloudRecordCasKind::Admit {
            upload_generation: 1,
        },
        expected: vec![CloudRecordSlot {
            key: key.into(),
            record: None,
        }],
        replacement: Some(CloudRecordSlot {
            key: key.into(),
            record: Some(durable_record(
                key,
                None,
                1,
                dir.path().join(format!("{key}.mp4")).display().to_string(),
                "queued",
            )),
        }),
    };

    let admitted = store
        .compare_exchange_cloud_records(change("source-1"))
        .unwrap();
    assert_eq!(admitted.document.cloud.upload_generation_sequence, 1);
    let error = store
        .compare_exchange_cloud_records(change("source-2"))
        .unwrap_err();
    assert_eq!(error, SettingsTransactionError::StaleCloudRecord);
    let current = store.snapshot().unwrap();
    assert!(current.document.cloud.uploads.contains_key("source-1"));
    assert!(!current.document.cloud.uploads.contains_key("source-2"));
}

#[test]
fn whole_record_cas_admits_once_and_rejects_stale_or_aba_writers_byte_identically() {
    let dir = TestDir::new("clipline-settings", "whole-cloud-record-cas");
    let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
    let initial = store.snapshot().unwrap();
    let path = dir.path().join("clip.mp4").display().to_string();
    let queued = durable_record("source-1", None, 7, &path, "queued");
    let admitted = store
        .transact(cloud_record_cas(
            &initial,
            CloudRecordCasKind::Admit {
                upload_generation: 7,
            },
            vec![CloudRecordSlot {
                key: "source-1".into(),
                record: None,
            }],
            Some(CloudRecordSlot {
                key: "source-1".into(),
                record: Some(queued.clone()),
            }),
        ))
        .unwrap();
    assert_eq!(admitted.document.cloud.uploads["source-1"], queued);
    assert_eq!(admitted.document.media_dir, initial.document.media_dir);

    let primary = file_bytes(store.profile().settings_path());
    let backup = file_bytes(&dir.path().join("settings.json.bak"));
    let stale = store
        .transact(cloud_record_cas(
            &admitted,
            CloudRecordCasKind::Admit {
                upload_generation: 7,
            },
            vec![CloudRecordSlot {
                key: "source-1".into(),
                record: None,
            }],
            Some(CloudRecordSlot {
                key: "source-1".into(),
                record: Some(queued),
            }),
        ))
        .unwrap_err();
    assert_eq!(stale, SettingsTransactionError::StaleCloudRecord);
    assert_eq!(store.snapshot().unwrap(), admitted);
    assert_eq!(file_bytes(store.profile().settings_path()), primary);
    assert_eq!(file_bytes(&dir.path().join("settings.json.bak")), backup);

    let generation_8 = durable_record("source-1", Some("payload-8"), 8, &path, "queued");
    let current = store
        .transact(cloud_record_cas(
            &admitted,
            CloudRecordCasKind::Admit {
                upload_generation: 8,
            },
            vec![CloudRecordSlot {
                key: "source-1".into(),
                record: Some(admitted.document.cloud.uploads["source-1"].clone()),
            }],
            Some(CloudRecordSlot {
                key: "source-1".into(),
                record: Some(generation_8.clone()),
            }),
        ))
        .unwrap();
    let delayed_generation_7 = store
        .transact(cloud_record_cas(
            &current,
            CloudRecordCasKind::Advance {
                upload_generation: 7,
            },
            vec![CloudRecordSlot {
                key: "source-1".into(),
                record: Some(admitted.document.cloud.uploads["source-1"].clone()),
            }],
            Some(CloudRecordSlot {
                key: "source-1".into(),
                record: Some(durable_record(
                    "source-1",
                    Some("payload-7"),
                    7,
                    &path,
                    "failed",
                )),
            }),
        ))
        .unwrap_err();
    assert_eq!(
        delayed_generation_7,
        SettingsTransactionError::StaleCloudRecord
    );
    assert_eq!(store.snapshot().unwrap(), current);
    assert_eq!(current.document.cloud.uploads["source-1"], generation_8);
}

#[test]
fn whole_record_cas_reconciles_only_exact_expected_equivalent_paths() {
    let dir = TestDir::new("clipline-settings", "whole-cloud-record-paths");
    let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
    let initial = store.snapshot().unwrap();
    let mut seeded_document = initial.document.clone();
    let legacy_a = durable_record(
        "legacy-a",
        Some("payload-old-a"),
        3,
        r"D:\Clips\same.mp4",
        "failed",
    );
    let legacy_b = durable_record(
        "legacy-b",
        Some("payload-old-b"),
        4,
        r"\\?\d:/clips/SAME.mp4",
        "failed",
    );
    let unrelated = durable_record(
        "other",
        Some("payload-other"),
        4,
        r"D:\Clips\other.mp4",
        "uploaded_private",
    );
    seeded_document
        .cloud
        .uploads
        .insert("legacy-a".into(), legacy_a.clone());
    seeded_document
        .cloud
        .uploads
        .insert("legacy-b".into(), legacy_b.clone());
    seeded_document
        .cloud
        .uploads
        .insert("other".into(), unrelated.clone());
    let seeded = store.replace_document(&initial, seeded_document).unwrap();
    let replacement = durable_record(
        "source-1",
        Some("payload-new"),
        9,
        r"d:/CLIPS/same.mp4",
        "queued",
    );
    let reconciled = store
        .transact(cloud_record_cas(
            &seeded,
            CloudRecordCasKind::Admit {
                upload_generation: 9,
            },
            vec![
                CloudRecordSlot {
                    key: "source-1".into(),
                    record: None,
                },
                CloudRecordSlot {
                    key: "legacy-a".into(),
                    record: Some(legacy_a),
                },
                CloudRecordSlot {
                    key: "legacy-b".into(),
                    record: Some(legacy_b),
                },
            ],
            Some(CloudRecordSlot {
                key: "source-1".into(),
                record: Some(replacement.clone()),
            }),
        ))
        .unwrap();
    assert_eq!(reconciled.document.cloud.uploads.len(), 2);
    assert_eq!(reconciled.document.cloud.uploads["source-1"], replacement);
    assert_eq!(reconciled.document.cloud.uploads["other"], unrelated);

    let primary = file_bytes(store.profile().settings_path());
    let invalid = store
        .transact(cloud_record_cas(
            &reconciled,
            CloudRecordCasKind::StatusSync,
            vec![
                CloudRecordSlot {
                    key: "source-1".into(),
                    record: Some(reconciled.document.cloud.uploads["source-1"].clone()),
                },
                CloudRecordSlot {
                    key: "other".into(),
                    record: Some(reconciled.document.cloud.uploads["other"].clone()),
                },
            ],
            Some(CloudRecordSlot {
                key: "source-1".into(),
                record: Some(durable_record(
                    "source-1",
                    Some("payload-new"),
                    9,
                    r"D:\Clips\same.mp4",
                    "uploaded_private",
                )),
            }),
        ))
        .unwrap_err();
    assert!(matches!(invalid, SettingsTransactionError::Validation(_)));
    assert_eq!(store.snapshot().unwrap(), reconciled);
    assert_eq!(file_bytes(store.profile().settings_path()), primary);
}

#[test]
fn whole_record_cas_refuses_a_stable_key_collision_with_another_local_identity() {
    let dir = TestDir::new("clipline-settings", "whole-cloud-record-key-collision");
    let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
    let initial = store.snapshot().unwrap();
    let mut document = initial.document.clone();
    let collision = durable_record(
        "different-source",
        Some("different-payload"),
        3,
        r"D:\Clips\different.mp4",
        "failed",
    );
    document
        .cloud
        .uploads
        .insert("source-1".into(), collision.clone());
    let seeded = store.replace_document(&initial, document).unwrap();
    let primary = file_bytes(store.profile().settings_path());
    let error = store
        .transact(cloud_record_cas(
            &seeded,
            CloudRecordCasKind::Admit {
                upload_generation: 4,
            },
            vec![CloudRecordSlot {
                key: "source-1".into(),
                record: Some(collision),
            }],
            Some(CloudRecordSlot {
                key: "source-1".into(),
                record: Some(durable_record(
                    "source-1",
                    None,
                    4,
                    r"D:\Clips\clip.mp4",
                    "queued",
                )),
            }),
        ))
        .unwrap_err();

    assert!(matches!(error, SettingsTransactionError::Validation(_)));
    assert_eq!(store.snapshot().unwrap(), seeded);
    assert_eq!(file_bytes(store.profile().settings_path()), primary);
}

#[test]
fn status_sync_cas_advances_only_the_exact_prior_record() {
    let dir = TestDir::new("clipline-settings", "whole-cloud-record-status-sync");
    let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
    let initial = store.snapshot().unwrap();
    let processing = durable_record(
        "source-1",
        Some("payload-1"),
        6,
        r"D:\Clips\clip.mp4",
        "uploaded_processing",
    );
    let admitted = store
        .transact(cloud_record_cas(
            &initial,
            CloudRecordCasKind::Admit {
                upload_generation: 6,
            },
            vec![CloudRecordSlot {
                key: "source-1".into(),
                record: None,
            }],
            Some(CloudRecordSlot {
                key: "source-1".into(),
                record: Some(processing.clone()),
            }),
        ))
        .unwrap();
    let mut ready = processing.clone();
    ready.upload_status = "uploaded_private".into();
    ready.remote_clip_id = Some("remote-1".into());
    ready.updated_at_unix += 1;
    let synced = store
        .transact(cloud_record_cas(
            &admitted,
            CloudRecordCasKind::StatusSync,
            vec![CloudRecordSlot {
                key: "source-1".into(),
                record: Some(processing.clone()),
            }],
            Some(CloudRecordSlot {
                key: "source-1".into(),
                record: Some(ready.clone()),
            }),
        ))
        .unwrap();
    assert_eq!(synced.document.cloud.uploads["source-1"], ready);

    let delayed = store
        .transact(cloud_record_cas(
            &synced,
            CloudRecordCasKind::StatusSync,
            vec![CloudRecordSlot {
                key: "source-1".into(),
                record: Some(processing.clone()),
            }],
            Some(CloudRecordSlot {
                key: "source-1".into(),
                record: Some(processing),
            }),
        ))
        .unwrap_err();
    assert_eq!(delayed, SettingsTransactionError::StaleCloudRecord);
    assert_eq!(store.snapshot().unwrap(), synced);
}

#[test]
fn whole_record_cas_enforces_generation_client_identity_and_account_fences() {
    let dir = TestDir::new("clipline-settings", "whole-cloud-record-invariants");
    let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
    let initial = store.snapshot().unwrap();
    let current_record = durable_record(
        "source-1",
        Some("payload-1"),
        12,
        r"D:\Clips\clip.mp4",
        "uploading",
    );
    let current = store
        .transact(cloud_record_cas(
            &initial,
            CloudRecordCasKind::Admit {
                upload_generation: 12,
            },
            vec![CloudRecordSlot {
                key: "source-1".into(),
                record: None,
            }],
            Some(CloudRecordSlot {
                key: "source-1".into(),
                record: Some(current_record.clone()),
            }),
        ))
        .unwrap();

    for replacement in [
        durable_record(
            "source-1",
            Some("payload-1"),
            11,
            r"D:\Clips\clip.mp4",
            "failed",
        ),
        durable_record(
            "source-1",
            Some("payload-other"),
            12,
            r"D:\Clips\clip.mp4",
            "uploaded_private",
        ),
    ] {
        let error = store
            .transact(cloud_record_cas(
                &current,
                CloudRecordCasKind::Advance {
                    upload_generation: 12,
                },
                vec![CloudRecordSlot {
                    key: "source-1".into(),
                    record: Some(current_record.clone()),
                }],
                Some(CloudRecordSlot {
                    key: "source-1".into(),
                    record: Some(replacement),
                }),
            ))
            .unwrap_err();
        assert!(matches!(error, SettingsTransactionError::Validation(_)));
        assert_eq!(store.snapshot().unwrap(), current);
    }

    let mut wrong_owner = current.account.clone();
    wrong_owner.host_url = "https://other.example.com".into();
    let error = store
        .transact(SettingsTransaction {
            expected_revision: current.revision,
            expected_account_generation: current.account_generation,
            change: SettingsChange::CompareExchangeCloudRecords(CloudRecordCas {
                account: wrong_owner,
                account_generation: current.account_generation,
                kind: CloudRecordCasKind::StatusSync,
                expected: vec![CloudRecordSlot {
                    key: "source-1".into(),
                    record: Some(current_record.clone()),
                }],
                replacement: Some(CloudRecordSlot {
                    key: "source-1".into(),
                    record: Some(current_record),
                }),
            }),
        })
        .unwrap_err();
    assert_eq!(error, SettingsTransactionError::AccountChanged);
    assert_eq!(store.snapshot().unwrap(), current);
}

#[test]
fn whole_record_cas_carries_an_independent_exact_account_generation() {
    let dir = TestDir::new("clipline-settings", "whole-cloud-record-account-generation");
    let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
    let initial = store.snapshot().unwrap();
    let mut profile = initial.document.cloud.clone();
    profile.host_url = "https://clips.example.com".into();
    profile.connected_user_id = Some("user-1".into());
    profile.credential_target = Some(cloud_credential_target(&profile.host_url, "user-1"));
    let connected = store
        .transact(transaction(
            &initial,
            SettingsChange::ReplaceCloudProfile(profile),
        ))
        .unwrap();
    let primary = file_bytes(store.profile().settings_path());
    let error = store
        .transact(SettingsTransaction {
            expected_revision: connected.revision,
            expected_account_generation: connected.account_generation,
            change: SettingsChange::CompareExchangeCloudRecords(CloudRecordCas {
                account: connected.account.clone(),
                account_generation: initial.account_generation,
                kind: CloudRecordCasKind::Admit {
                    upload_generation: 1,
                },
                expected: vec![CloudRecordSlot {
                    key: "source-1".into(),
                    record: None,
                }],
                replacement: Some(CloudRecordSlot {
                    key: "source-1".into(),
                    record: Some(durable_record(
                        "source-1",
                        None,
                        1,
                        r"D:\Clips\clip.mp4",
                        "queued",
                    )),
                }),
            }),
        })
        .unwrap_err();

    assert_eq!(
        error,
        SettingsTransactionError::StaleAccountGeneration {
            expected: initial.account_generation,
            current: connected.account_generation,
        }
    );
    assert_eq!(store.snapshot().unwrap(), connected);
    assert_eq!(file_bytes(store.profile().settings_path()), primary);
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

fn connected_publication_account(
    snapshot: &clipline_settings::SettingsSnapshot,
    host: &str,
    user: &str,
) -> clipline_settings::CloudSettings {
    let mut cloud = snapshot.document.cloud.clone();
    cloud.host_url = host.into();
    cloud.connected_user_id = Some(user.into());
    cloud.connected_username = Some(format!("{user}-name"));
    cloud.credential_target = Some(cloud_credential_target(host, user));
    cloud
}

#[test]
fn cloud_publication_owner_checks_every_fence_before_invoking_once() {
    let dir = TestDir::new("clipline-settings", "cloud-publication-fences");
    let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
    let initial = store.snapshot().unwrap();
    let connected = store
        .transact(transaction(
            &initial,
            SettingsChange::ReplaceCloudSettings(connected_publication_account(
                &initial,
                "https://a.example",
                "user-a",
            )),
        ))
        .unwrap();
    let owner = CloudAccountPublicationOwner::from_snapshot(&connected);

    let mut publications = 0;
    store
        .publish_if_cloud_account_current(&owner, || {
            publications += 1;
            Ok::<_, &'static str>(())
        })
        .unwrap()
        .unwrap();
    assert_eq!(publications, 1);

    let mut stale_identity = owner.clone();
    stale_identity.account.host_url = "https://wrong.example".into();
    let mut stale_generation = owner.clone();
    stale_generation.account_generation = AccountGeneration::INITIAL;
    let mut stale_namespace_source = owner.clone();
    stale_namespace_source.stable_account = Some("wrong-user".into());
    for stale in [stale_identity, stale_generation, stale_namespace_source] {
        let result = store.publish_if_cloud_account_current(&stale, || {
            publications += 1;
            Ok::<_, &'static str>(())
        });
        assert!(matches!(
            result,
            Err(SettingsTransactionError::AccountChanged
                | SettingsTransactionError::StaleAccountGeneration { .. })
        ));
    }
    assert_eq!(publications, 1);
}

#[test]
fn cloud_publication_tolerates_unrelated_revisions_and_propagates_closure_errors() {
    let dir = TestDir::new("clipline-settings", "cloud-publication-unrelated");
    let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
    let initial = store.snapshot().unwrap();
    let connected = store
        .transact(transaction(
            &initial,
            SettingsChange::ReplaceCloudSettings(connected_publication_account(
                &initial,
                "https://a.example",
                "user-a",
            )),
        ))
        .unwrap();
    let owner = CloudAccountPublicationOwner::from_snapshot(&connected);
    store
        .transact(transaction(
            &connected,
            SettingsChange::SetMediaRoot(dir.path().join("unrelated").display().to_string()),
        ))
        .unwrap();

    let result = store
        .publish_if_cloud_account_current(&owner, || Err::<(), _>("publication failed"))
        .unwrap();
    assert_eq!(result, Err("publication failed"));
}

#[test]
fn cloud_publication_rejects_switch_away_and_back_aba() {
    let dir = TestDir::new("clipline-settings", "cloud-publication-aba");
    let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
    let initial = store.snapshot().unwrap();
    let account_a = connected_publication_account(&initial, "https://a.example", "user-a");
    let connected_a = store
        .transact(transaction(
            &initial,
            SettingsChange::ReplaceCloudSettings(account_a.clone()),
        ))
        .unwrap();
    let stale_owner = CloudAccountPublicationOwner::from_snapshot(&connected_a);
    let account_b = connected_publication_account(&connected_a, "https://b.example", "user-b");
    let connected_b = store
        .transact(transaction(
            &connected_a,
            SettingsChange::ReplaceCloudSettings(account_b),
        ))
        .unwrap();
    let reconnected_a = store
        .transact(transaction(
            &connected_b,
            SettingsChange::ReplaceCloudSettings(account_a),
        ))
        .unwrap();
    assert_eq!(reconnected_a.account, stale_owner.account);
    assert!(reconnected_a.account_generation > stale_owner.account_generation);

    let mut invoked = false;
    let error = store
        .publish_if_cloud_account_current(&stale_owner, || {
            invoked = true;
            Ok::<_, &'static str>(())
        })
        .unwrap_err();
    assert!(matches!(
        error,
        SettingsTransactionError::StaleAccountGeneration { .. }
    ));
    assert!(!invoked);
}

#[test]
fn cloud_publication_rejects_legacy_username_namespace_aba() {
    let dir = TestDir::new("clipline-settings", "cloud-publication-legacy-username-aba");
    let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
    let initial = store.snapshot().unwrap();
    let mut legacy_a = initial.document.cloud.clone();
    legacy_a.host_url = "https://legacy.example".into();
    legacy_a.connected_user_id = None;
    legacy_a.connected_username = Some("legacy-a".into());
    legacy_a.credential_target = Some(cloud_credential_target(
        &legacy_a.host_url,
        "stable-credential-owner",
    ));
    let connected_a = store
        .transact(transaction(
            &initial,
            SettingsChange::ReplaceCloudSettings(legacy_a.clone()),
        ))
        .unwrap();
    let stale_owner = CloudAccountPublicationOwner::from_snapshot(&connected_a);

    let mut legacy_b = legacy_a.clone();
    legacy_b.connected_username = Some("legacy-b".into());
    let connected_b = store
        .transact(transaction(
            &connected_a,
            SettingsChange::ReplaceCloudSettings(legacy_b),
        ))
        .unwrap();
    let restored_a = store
        .transact(transaction(
            &connected_b,
            SettingsChange::ReplaceCloudSettings(legacy_a),
        ))
        .unwrap();

    assert_eq!(restored_a.account, stale_owner.account);
    assert_eq!(
        CloudAccountPublicationOwner::from_snapshot(&restored_a).stable_account,
        stale_owner.stable_account
    );
    assert!(restored_a.account_generation > stale_owner.account_generation);
    let mut invoked = false;
    assert!(matches!(
        store.publish_if_cloud_account_current(&stale_owner, || {
            invoked = true;
            Ok::<_, &'static str>(())
        }),
        Err(SettingsTransactionError::StaleAccountGeneration { .. })
    ));
    assert!(!invoked);
}

#[test]
fn cloud_publication_blocks_an_independently_opened_account_replacement() {
    let dir = TestDir::new("clipline-settings", "cloud-publication-linearized");
    let profile = SettingsProfile::isolated(dir.path());
    let publishing_store = SettingsStore::open(profile.clone());
    let initial = publishing_store.snapshot().unwrap();
    let connected = publishing_store
        .transact(transaction(
            &initial,
            SettingsChange::ReplaceCloudSettings(connected_publication_account(
                &initial,
                "https://a.example",
                "user-a",
            )),
        ))
        .unwrap();
    let replacement_store = SettingsStore::open(profile);
    let replacement_snapshot = replacement_store.snapshot().unwrap();
    let owner = CloudAccountPublicationOwner::from_snapshot(&connected);
    let (publication_started_tx, publication_started_rx) = mpsc::channel();
    let (release_publication_tx, release_publication_rx) = mpsc::channel();
    let publisher = std::thread::spawn(move || {
        publishing_store
            .publish_if_cloud_account_current(&owner, || {
                publication_started_tx.send(()).unwrap();
                release_publication_rx.recv().unwrap();
                Ok::<_, &'static str>(())
            })
            .unwrap()
            .unwrap();
    });
    publication_started_rx
        .recv_timeout(Duration::from_secs(1))
        .unwrap();

    let (replacement_done_tx, replacement_done_rx) = mpsc::channel();
    let replacement = std::thread::spawn(move || {
        let account_b =
            connected_publication_account(&replacement_snapshot, "https://b.example", "user-b");
        let result = replacement_store.transact(transaction(
            &replacement_snapshot,
            SettingsChange::ReplaceCloudSettings(account_b),
        ));
        replacement_done_tx.send(result).unwrap();
    });
    assert!(matches!(
        replacement_done_rx.recv_timeout(Duration::from_millis(100)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));

    release_publication_tx.send(()).unwrap();
    publisher.join().unwrap();
    replacement_done_rx
        .recv_timeout(Duration::from_secs(1))
        .unwrap()
        .unwrap();
    replacement.join().unwrap();
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
